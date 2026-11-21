//! Whisper speech recognition in pure Rust (candle, CPU).
//!
//! Transcription follows the procedure published with the model (sequential 30-second windows):
//!
//! 1. The whole clip's log-mel spectrogram is computed once ([`crate::mel`]), padded with 30 s of
//!    silence. A window of 3000 frames starting at `seek` is encoded.
//! 2. **Language**: unless given, the first window's decoder logits after `<|startoftranscript|>`
//!    are compared over the language tokens and the most likely language is used.
//! 3. **Decoding** is greedy with timestamp tokens: the first token must be a timestamp (at most
//!    1 s), timestamps come in pairs and never go backward, timestamps win whenever their summed
//!    probability beats the best text token, and special tokens are suppressed. Windows whose
//!    `<|nospeech|>` probability exceeds 0.6 while the text is improbable are skipped. A loop
//!    guard stops a window that keeps repeating itself.
//! 4. `seek` advances to the last complete timestamp pair (or the whole window).
//! 5. **Word times**: the window's text tokens are run once more through the decoder
//!    (`<|notimestamps|>` prompt) and the cross-attention logits of the model's alignment heads
//!    (`generation_config.json`; by default every head of the second half of the decoder) are
//!    softmaxed over the window's audio frames, standardized per token, median-filtered (width 7)
//!    and averaged; dynamic time warping through the negated matrix gives each token's start frame
//!    (20 ms resolution). Tokens are grouped into words at spaces; punctuation joins its word.
//! 6. Word bounds are tightened against the audio ([`crate::vad`]).
//!
//! The model's files are size-checked before they are read: the JSON files may hold at most
//! 64 MiB and the weights at most 4 GiB.

mod align;
mod model;
pub mod tokenizer;

use std::path::Path;
use std::sync::Mutex;

use candle_core::Tensor;

use crate::{Options, ProgressFn, SpeechError, Transcriber, Transcript, Word};
use model::{Config, Model};
use tokenizer::Tokenizer;

/// Mel frames per window (30 s).
const N_FRAMES: usize = 3000;
/// Mel frames per timestamp step (0.02 s).
const FRAMES_PER_TS: usize = 2;
const SAMPLES_PER_FRAME: usize = crate::mel::HOP;
const MAX_TOKENS: usize = 224;
/// More than the total alignment heads in any published Whisper architecture.
const MAX_ALIGNMENT_HEADS: usize = 4_096;
/// The largest `config.json`, `generation_config.json` or `tokenizer.json` read.
const MAX_JSON_BYTES: u64 = 64 << 20;
/// The largest `model.safetensors` read.
const MAX_WEIGHT_BYTES: u64 = 4 << 30;

fn merr(e: candle_core::Error) -> SpeechError {
    SpeechError::Model(e.to_string())
}

/// Read `dir/name` when it is a file of at most `limit` bytes.
fn read_limited(dir: &Path, name: &str, limit: u64) -> Result<Vec<u8>, SpeechError> {
    let path = dir.join(name);
    let meta = std::fs::metadata(&path).map_err(|e| SpeechError::Model(format!("{}: {e}", path.display())))?;
    if !meta.is_file() || meta.len() > limit {
        return Err(SpeechError::Model(format!("{}: not a file of at most {limit} bytes", path.display())));
    }
    std::fs::read(&path).map_err(|e| SpeechError::Model(format!("{}: {e}", path.display())))
}

pub struct Whisper {
    id: String,
    model: Mutex<Model>,
    tok: Tokenizer,
    alignment_heads: Vec<(usize, usize)>,
    suppress: Vec<u32>,
    begin_suppress: Vec<u32>,
}

impl Whisper {
    /// Load a model folder (`config.json`, `generation_config.json`, `tokenizer.json`,
    /// `model.safetensors`).
    pub fn load(dir: &Path, id: &str) -> Result<Self, SpeechError> {
        let cfg: Config = serde_json::from_slice(&read_limited(dir, "config.json", MAX_JSON_BYTES)?)
            .map_err(|e| SpeechError::Model(format!("config.json: {e}")))?;
        cfg.check().map_err(merr)?;
        let gen_cfg: serde_json::Value = serde_json::from_slice(&read_limited(dir, "generation_config.json", MAX_JSON_BYTES)?)
            .map_err(|e| SpeechError::Model(format!("generation_config.json: {e}")))?;
        let tok = Tokenizer::from_json(&String::from_utf8_lossy(&read_limited(dir, "tokenizer.json", MAX_JSON_BYTES)?))?;
        tok.check(cfg.vocab_size)?;
        let alignment_heads = alignment_heads(&cfg, &gen_cfg)?;
        let ids = |key: &str| -> Option<Vec<u32>> {
            gen_cfg[key].as_array().map(|a| a.iter().filter_map(|x| x.as_u64().and_then(|v| u32::try_from(v).ok())).collect())
        };
        let mut suppress = ids("suppress_tokens").unwrap_or_default();
        suppress.extend(tok.non_text_specials());
        suppress.sort_unstable();
        suppress.dedup();
        let begin_suppress = ids("begin_suppress_tokens").unwrap_or_else(|| vec![220, tok.eot]);
        let weights = read_limited(dir, "model.safetensors", MAX_WEIGHT_BYTES)?;
        let model = Model::load(cfg, weights).map_err(merr)?;
        Ok(Self { id: id.to_string(), model: Mutex::new(model), tok, alignment_heads, suppress, begin_suppress })
    }

    /// The language codes the model knows (empty for an English-only model).
    pub fn languages(&self) -> Vec<String> {
        self.tok.languages.iter().map(|l| l.0.clone()).collect()
    }

    fn window(mel: &[f32], n_mels: usize, frames: usize, seek: usize) -> Vec<f32> {
        let mut w = vec![0f32; n_mels * N_FRAMES];
        let end = (seek + N_FRAMES).min(frames);
        if seek < end {
            for m in 0..n_mels {
                if let (Some(dst), Some(src)) = (w.get_mut(m * N_FRAMES..m * N_FRAMES + end - seek), mel.get(m * frames + seek..m * frames + end)) {
                    dst.copy_from_slice(src);
                }
            }
        }
        w
    }

    fn detect_language(&self, m: &mut Model, xa: &Tensor) -> Result<String, SpeechError> {
        m.decoder.reset();
        let (logits, _) = m.decoder.forward(&[self.tok.sot], xa, false, None).map_err(merr)?;
        let l = Model::last_logits(&logits).map_err(merr)?;
        let score = |id: u32| l.get(id as usize).copied().unwrap_or(f32::NEG_INFINITY);
        let best = self.tok.languages.iter().max_by(|a, b| score(a.1).total_cmp(&score(b.1))).map(|x| x.0.clone());
        Ok(best.unwrap_or_else(|| "en".into()))
    }

    /// Apply the timestamp rules and suppression to `logits` for the next token after `seq`
    /// (sampled tokens of this window, prompt excluded).
    fn constrain(&self, logits: &mut [f32], seq: &[u32], max_ts: Option<u32>) {
        let n = logits.len();
        let tb = (self.tok.timestamp_begin as usize).min(n);
        let eot = (self.tok.eot as usize).min(n);
        let ninf = f32::NEG_INFINITY;
        let fill = |logits: &mut [f32], from: usize, to: usize| {
            if let Some(s) = logits.get_mut(from.min(to)..to) {
                s.iter_mut().for_each(|v| *v = ninf);
            }
        };
        for &s in &self.suppress {
            if let Some(v) = logits.get_mut(s as usize) {
                *v = ninf;
            }
        }
        if seq.is_empty() {
            for &s in &self.begin_suppress {
                if let Some(v) = logits.get_mut(s as usize) {
                    *v = ninf;
                }
            }
        }
        let last_ts = seq.last().is_some_and(|&t| self.tok.is_timestamp(t));
        let penult_ts = seq.len() < 2 || seq.get(seq.len() - 2).is_some_and(|&t| self.tok.is_timestamp(t));
        if last_ts {
            if penult_ts {
                fill(logits, tb, n);
            } else {
                fill(logits, 0, eot);
            }
        }
        if let Some(&last) = seq.iter().rev().find(|&&t| self.tok.is_timestamp(t)) {
            let lim = (if last_ts && !penult_ts { last } else { last.saturating_add(1) } as usize).min(n);
            fill(logits, tb, lim);
        }
        if seq.is_empty() {
            fill(logits, 0, tb);
            if let Some(m) = max_ts {
                let cut = tb.saturating_add(m as usize).saturating_add(1).min(n);
                fill(logits, cut, n);
            }
        }
        // Timestamps win when their total probability beats every text token.
        let max = logits.iter().copied().fold(ninf, f32::max);
        if max.is_finite() {
            let lse = |s: &[f32]| -> f32 {
                let m = s.iter().copied().fold(ninf, f32::max);
                if !m.is_finite() {
                    return ninf;
                }
                m + s.iter().map(|v| (v - m).exp()).sum::<f32>().ln()
            };
            let ts = lse(logits.get(tb..).unwrap_or_default());
            let best_text = logits.get(..tb).unwrap_or_default().iter().copied().fold(ninf, f32::max);
            if ts > best_text {
                fill(logits, 0, tb);
            }
        }
    }

    /// Greedy decode of one window. Returns sampled tokens (no prompt, no end-of-text), the mean
    /// log-probability and the no-speech probability.
    fn decode_window(&self, m: &mut Model, xa: &Tensor, prompt: &[u32], stop: &mut dyn FnMut() -> bool) -> Result<(Vec<u32>, f32, f32), SpeechError> {
        m.decoder.reset();
        let mut seq: Vec<u32> = Vec::new();
        let (logits, _) = m.decoder.forward(prompt, xa, true, None).map_err(merr)?;
        // The no-speech probability from the start-of-transcript position.
        let first = logits.get(0).and_then(|l| l.get(0)).and_then(|l| l.to_vec1::<f32>()).map_err(merr)?;
        let no_speech = self.tok.no_speech.map(|ns| softmax_at(&first, ns as usize)).unwrap_or(0.0);
        let mut cur = Model::last_logits(&logits).map_err(merr)?;
        let mut sum_lp = 0f32;
        for _ in 0..MAX_TOKENS.min(m.cfg.max_target_positions / 2) {
            if stop() {
                return Err(SpeechError::Canceled);
            }
            let lp = log_softmax(&cur);
            self.constrain(&mut cur, &seq, Some(50));
            let next = argmax(&cur);
            sum_lp += lp.get(next as usize).copied().unwrap_or(f32::NEG_INFINITY);
            if next == self.tok.eot {
                break;
            }
            seq.push(next);
            if repeating(&seq, self.tok.timestamp_begin) {
                break;
            }
            let (logits, _) = m.decoder.forward(&[next], xa, true, None).map_err(merr)?;
            cur = Model::last_logits(&logits).map_err(merr)?;
        }
        let avg = sum_lp / (seq.len() + 1) as f32;
        Ok((seq, avg, no_speech))
    }
}

fn alignment_heads(cfg: &Config, generation: &serde_json::Value) -> Result<Vec<(usize, usize)>, SpeechError> {
    let bad = |message: &str| SpeechError::Model(format!("generation_config.json: {message}"));
    if let Some(values) = generation["alignment_heads"].as_array().filter(|values| !values.is_empty()) {
        if values.len() > MAX_ALIGNMENT_HEADS {
            return Err(bad("too many alignment heads"));
        }
        return values
            .iter()
            .map(|pair| {
                let at = |i: usize| {
                    pair.get(i).and_then(serde_json::Value::as_u64).and_then(|v| usize::try_from(v).ok()).ok_or_else(|| bad("invalid alignment head"))
                };
                let (layer, head) = (at(0)?, at(1)?);
                if layer >= cfg.decoder_layers || head >= cfg.decoder_attention_heads {
                    return Err(bad("alignment head is outside the decoder architecture"));
                }
                Ok((layer, head))
            })
            .collect();
    }
    let first_layer = cfg.decoder_layers / 2;
    let count = cfg
        .decoder_layers
        .saturating_sub(first_layer)
        .checked_mul(cfg.decoder_attention_heads)
        .ok_or_else(|| bad("alignment-head count overflow"))?;
    if count > MAX_ALIGNMENT_HEADS {
        return Err(bad("alignment-head fallback is too large"));
    }
    Ok((first_layer..cfg.decoder_layers).flat_map(|layer| (0..cfg.decoder_attention_heads).map(move |head| (layer, head))).collect())
}

fn softmax_at(x: &[f32], i: usize) -> f32 {
    let m = x.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let s: f32 = x.iter().map(|v| (v - m).exp()).sum();
    x.get(i).map(|v| (v - m).exp() / s).unwrap_or(0.0)
}

fn log_softmax(x: &[f32]) -> Vec<f32> {
    let m = x.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let lse = m + x.iter().map(|v| (v - m).exp()).sum::<f32>().ln();
    x.iter().map(|v| v - lse).collect()
}

fn argmax(x: &[f32]) -> u32 {
    let mut best = 0usize;
    let mut best_v = f32::NEG_INFINITY;
    for (i, &v) in x.iter().enumerate() {
        if v > best_v {
            best = i;
            best_v = v;
        }
    }
    best as u32
}

/// The tail of `seq` (text tokens) repeats a short pattern many times: a decoding loop.
fn repeating(seq: &[u32], tb: u32) -> bool {
    let text: Vec<u32> = seq.iter().copied().filter(|&t| t < tb).collect();
    for n in 1..=8 {
        let reps = if n == 1 { 12 } else { 5 };
        if text.len() < n * reps {
            continue;
        }
        let tail = &text[text.len() - n * reps..];
        if (1..reps).all(|r| tail[r * n..(r + 1) * n] == tail[..n]) {
            return true;
        }
    }
    false
}

impl Transcriber for Whisper {
    fn id(&self) -> String {
        self.id.clone()
    }

    fn transcribe(&self, audio: &[f32], opts: &Options, progress: ProgressFn) -> Result<Transcript, SpeechError> {
        let mut m = self.model.lock().unwrap_or_else(|e| e.into_inner());
        let n_mels = m.cfg.num_mel_bins;
        let content_frames = audio.len() / SAMPLES_PER_FRAME;
        let mut padded: Vec<f32> = audio.iter().map(|v| if v.is_finite() { *v } else { 0.0 }).collect();
        padded.extend(std::iter::repeat_n(0.0, N_FRAMES * SAMPLES_PER_FRAME));
        if !progress(0.0, "Analyzing audio") {
            return Err(SpeechError::Canceled);
        }
        let (frames, mel) = crate::mel::log_mel(&padded, n_mels);
        let mut language = opts.language.clone().filter(|l| !l.is_empty() && l != "auto");
        if let Some(l) = language.as_deref()
            && self.tok.multilingual()
            && self.tok.language_token(l).is_none()
        {
            return Err(SpeechError::Model(format!("the model does not know the language `{l}`")));
        }
        let mut words: Vec<Word> = Vec::new();
        let mut seek = 0usize;
        while seek < content_frames {
            let frac = seek as f32 / content_frames.max(1) as f32;
            if !progress(frac * 0.95, "Transcribing") {
                return Err(SpeechError::Canceled);
            }
            let seg_frames = N_FRAMES.min(content_frames - seek);
            let win = Self::window(&mel, n_mels, frames, seek);
            let x = Tensor::from_vec(win, (1, n_mels, N_FRAMES), &m.device).map_err(merr)?;
            let xa = m.encoder.forward(&x).map_err(merr)?;
            let mut prompt = vec![self.tok.sot];
            if self.tok.multilingual() {
                if language.is_none() {
                    language = Some(self.detect_language(&mut m, &xa)?);
                }
                let lang = language.as_deref().unwrap_or("en");
                prompt.push(self.tok.language_token(lang).unwrap_or(self.tok.sot + 1));
                prompt.push(self.tok.transcribe);
            }
            let (toks, avg_lp, no_speech) = self.decode_window(&mut m, &xa, &prompt, &mut || !progress(frac * 0.95, "Transcribing"))?;
            let skip = no_speech > 0.6 && avg_lp < -1.0;
            // Where the next window starts.
            let is_ts: Vec<bool> = toks.iter().map(|&t| self.tok.is_timestamp(t)).collect();
            let single_ending = is_ts.len() >= 2 && !is_ts[is_ts.len() - 2] && is_ts[is_ts.len() - 1];
            let last_pair = (1..is_ts.len()).rev().find(|&i| is_ts[i] && is_ts[i - 1]);
            let advance = match last_pair {
                Some(i) if !single_ending => (toks[i - 1].saturating_sub(self.tok.timestamp_begin) as usize * FRAMES_PER_TS).clamp(1, seg_frames),
                _ => seg_frames,
            };
            if !skip && toks.iter().any(|&t| t < self.tok.eot) {
                // The text up to the last complete segment (what `advance` covers).
                let upto = match last_pair {
                    Some(i) if !single_ending => i,
                    _ => toks.len(),
                };
                let text: Vec<u32> = toks[..upto].iter().copied().filter(|&t| t < self.tok.eot).collect();
                let mut aprompt = prompt.clone();
                aprompt.push(self.tok.no_timestamps);
                let times = align::align(&mut m, &xa, &aprompt, &text, self.tok.eot, &self.alignment_heads, seg_frames).map_err(merr)?;
                let offset = seek * SAMPLES_PER_FRAME;
                let limit = (seek + advance) * SAMPLES_PER_FRAME;
                for (wtext, r) in tokenizer::group_words(&self.tok, &text) {
                    let (Some(first), Some(last)) = (times.get(r.start), r.end.checked_sub(1).and_then(|e| times.get(e))) else { continue };
                    let a = (offset + first.0 * SAMPLES_PER_FRAME).min(limit);
                    let b = (offset + last.1 * SAMPLES_PER_FRAME).min(limit).max(a);
                    words.push(Word::new(wtext, a as u64, b as u64));
                }
            }
            seek += advance;
        }
        let mut t = Transcript { language: language.unwrap_or_else(|| "en".into()), source: self.id.clone(), words };
        t.normalize();
        crate::vad::tighten_words(audio, &mut t.words);
        progress(1.0, "Done");
        Ok(t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(layers: usize, heads: usize) -> Config {
        Config {
            num_mel_bins: 80,
            d_model: 384,
            encoder_layers: 4,
            encoder_attention_heads: 6,
            decoder_layers: layers,
            decoder_attention_heads: heads,
            max_source_positions: 1_500,
            max_target_positions: 448,
            vocab_size: 51_865,
        }
    }

    #[test]
    fn fallback_alignment_heads_are_bounded_before_collection() {
        let error = alignment_heads(&config(usize::MAX, usize::MAX), &serde_json::Value::Null).unwrap_err();
        assert!(error.to_string().contains("alignment-head"));
        assert_eq!(alignment_heads(&config(4, 6), &serde_json::Value::Null).unwrap().len(), 12);
        let listed = serde_json::json!({ "alignment_heads": [[3, 5], [2, 0]] });
        assert_eq!(alignment_heads(&config(4, 6), &listed).unwrap(), [(3, 5), (2, 0)]);
        let outside = serde_json::json!({ "alignment_heads": [[4, 0]] });
        assert!(alignment_heads(&config(4, 6), &outside).is_err());
    }

    #[test]
    fn repetition_guard() {
        let tb = 1000;
        assert!(repeating(&[5; 12], tb));
        assert!(!repeating(&[5, 5, 5, 6, 7, 8], tb));
        let mut s = Vec::new();
        for _ in 0..5 {
            s.extend([1, 2, 3]);
        }
        assert!(repeating(&s, tb));
        assert!(!repeating(&[1, 2, 3, 4, 5, 6, 7], tb));
    }

    #[test]
    fn argmax_skips_nan_and_picks_the_largest() {
        assert_eq!(argmax(&[f32::NAN, 1.0, 3.0, 2.0]), 2);
        assert_eq!(argmax(&[]), 0);
    }

    #[test]
    fn a_folder_with_oversized_or_missing_files_is_refused_before_reading() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Whisper::load(dir.path(), "x").is_err());
        std::fs::create_dir(dir.path().join("config.json")).unwrap();
        let e = Whisper::load(dir.path(), "x").err().map(|e| e.to_string()).unwrap_or_default();
        assert!(e.contains("not a file"), "{e}");
    }

    #[test]
    fn a_malformed_generation_config_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let config = r#"{"num_mel_bins":80,"d_model":384,"encoder_layers":4,"encoder_attention_heads":6,"decoder_layers":4,
            "decoder_attention_heads":6,"max_source_positions":1500,"max_target_positions":448,"vocab_size":51865}"#;
        std::fs::write(dir.path().join("config.json"), config).unwrap();
        std::fs::write(dir.path().join("generation_config.json"), "{not json").unwrap();
        let e = Whisper::load(dir.path(), "x").err().map(|e| e.to_string()).unwrap_or_default();
        assert!(e.contains("generation_config.json"), "{e}");
    }
}
