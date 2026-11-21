//! Speech-to-text on the device.
//!
//! - [`Transcriber`]: the trait every speech recognizer implements. It takes mono 16 kHz samples
//!   and returns a [`Transcript`] whose words carry their place in the audio as 16 kHz sample
//!   positions.
//! - [`models`]: the catalog of downloadable Whisper models (each file pinned to a revision, with
//!   its SHA-256 and size), where they are kept, and (feature `download`) the verified downloader.
//!   Weights are never bundled or committed.
//! - [`whisper`] (feature `whisper`): Whisper inference in pure Rust on candle, with timestamp
//!   decoding, language detection and word times from cross-attention alignment.
//! - [`live`]: live captions: audio arrives in pieces, is cut into phrases at pauses, and each
//!   phrase is shown as it is spoken and corrected when it ends.
//! - [`resample`]: any rate and channel count down to the mono 16 kHz the recognizers take.
//! - [`mel`]: the log-mel front end. [`vad`]: word bounds tightened against the audio.

pub mod live;
pub mod mel;
pub mod models;
pub mod resample;
pub mod vad;
#[cfg(feature = "whisper")]
pub mod whisper;

use std::time::Duration;

/// The sample rate every transcriber takes.
pub const SAMPLE_RATE: u32 = 16_000;

/// The language codes the multilingual Whisper models in [`models`] recognize, in the order of
/// their tokens.
pub const WHISPER_LANGUAGES: [&str; 99] = [
    "en", "zh", "de", "es", "ru", "ko", "fr", "ja", "pt", "tr", "pl", "ca", "nl", "ar", "sv", "it", "id", "hi", "fi", "vi", "he", "uk", "el", "ms",
    "cs", "ro", "da", "hu", "ta", "no", "th", "ur", "hr", "bg", "lt", "la", "mi", "ml", "cy", "sk", "te", "fa", "lv", "bn", "sr", "az", "sl", "kn",
    "et", "mk", "br", "eu", "is", "hy", "ne", "mn", "bs", "kk", "sq", "sw", "gl", "mr", "pa", "si", "km", "sn", "yo", "so", "af", "oc", "ka", "be",
    "tg", "sd", "gu", "am", "yi", "lo", "uz", "fo", "ht", "ps", "tk", "nn", "mt", "sa", "lb", "my", "bo", "tl", "mg", "as", "tt", "haw", "ln", "ha",
    "ba", "jw", "su",
];

/// The time of 16 kHz sample `n`.
pub fn sample_time(n: u64) -> Duration {
    Duration::from_secs(n / u64::from(SAMPLE_RATE)) + Duration::from_nanos((n % u64::from(SAMPLE_RATE)) * 62_500)
}

/// The 16 kHz sample at `t`, rounded down.
pub fn time_sample(t: Duration) -> u64 {
    t.as_secs().saturating_mul(u64::from(SAMPLE_RATE)).saturating_add(u64::from(t.subsec_nanos()) / 62_500)
}

/// One recognized word: its text (with the punctuation that follows it) and its place in the
/// audio, `[start, end)` in 16 kHz samples.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct Word {
    pub text: String,
    pub start: u64,
    pub end: u64,
}

impl Word {
    pub fn new(text: impl Into<String>, start: u64, end: u64) -> Word {
        Word { text: text.into(), start, end: end.max(start) }
    }
}

/// What a transcriber heard.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct Transcript {
    /// ISO 639-1 code of the language, given or detected.
    pub language: String,
    /// The model's id.
    pub source: String,
    /// In time order, without overlaps ([`Transcript::normalize`]).
    pub words: Vec<Word>,
}

impl Transcript {
    /// The words joined by spaces.
    pub fn text(&self) -> String {
        let mut out = String::new();
        for w in &self.words {
            if w.text.is_empty() {
                continue;
            }
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(&w.text);
        }
        out
    }

    /// Sort the words by start, drop empty ones, and end each word no later than the next starts.
    pub fn normalize(&mut self) {
        self.words.retain(|w| !w.text.trim().is_empty());
        self.words.sort_by_key(|w| (w.start, w.end));
        for i in 1..self.words.len() {
            let next = self.words[i].start;
            if let Some(prev) = self.words.get_mut(i - 1) {
                prev.end = prev.end.min(next).max(prev.start);
            }
        }
        for w in &mut self.words {
            w.end = w.end.max(w.start);
        }
    }

    /// Move every word `samples` later.
    pub fn shift(&mut self, samples: u64) {
        for w in &mut self.words {
            w.start = w.start.saturating_add(samples);
            w.end = w.end.saturating_add(samples);
        }
    }
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SpeechError {
    #[error("stopped")]
    Canceled,
    #[error("the speech model `{0}` is not downloaded")]
    NotInstalled(String),
    #[error("unknown speech model `{0}`")]
    UnknownModel(String),
    #[error("speech-to-text is not available in this build ({0})")]
    Unavailable(String),
    #[error("model: {0}")]
    Model(String),
    #[error("download: {0}")]
    Download(String),
    #[error("{0}")]
    Io(String),
}

impl From<std::io::Error> for SpeechError {
    fn from(e: std::io::Error) -> Self {
        SpeechError::Io(e.to_string())
    }
}

/// Transcription options.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// ISO 639-1 language code; `None` detects it from the audio.
    pub language: Option<String>,
}

/// Progress: `(fraction 0..1, status)`; return `false` to stop.
pub type ProgressFn<'a> = &'a mut dyn FnMut(f32, &str) -> bool;

/// A speech recognizer.
pub trait Transcriber: Send + Sync {
    /// The model's stable id (`"whisper-base"`), stored in the transcript's `source`.
    fn id(&self) -> String;
    /// Transcribe mono 16 kHz `audio`. Word positions count from its first sample.
    fn transcribe(&self, audio: &[f32], opts: &Options, progress: ProgressFn) -> Result<Transcript, SpeechError>;
}

/// A transcriber that answers with a fixed transcript (tests, demonstrations and agents that bring
/// their own text). Words that start after the audio's end are dropped.
pub struct FixedTranscriber {
    pub transcript: Transcript,
    pub id: String,
}

impl Transcriber for FixedTranscriber {
    fn id(&self) -> String {
        self.id.clone()
    }
    fn transcribe(&self, audio: &[f32], _: &Options, progress: ProgressFn) -> Result<Transcript, SpeechError> {
        if !progress(0.5, "Transcribing") {
            return Err(SpeechError::Canceled);
        }
        let end = audio.len() as u64;
        let mut t = self.transcript.clone();
        t.words.retain(|w| w.start < end);
        for w in &mut t.words {
            w.end = w.end.min(end);
        }
        t.source = self.id.clone();
        progress(1.0, "Done");
        Ok(t)
    }
}

/// The transcriber for catalog model `id` in `models_dir` (feature `whisper`). Every file is
/// checked against its size and SHA-256 first.
pub fn load(models_dir: &std::path::Path, id: &str) -> Result<std::sync::Arc<dyn Transcriber>, SpeechError> {
    let m = models::find(id).ok_or_else(|| SpeechError::UnknownModel(id.into()))?;
    if !models::installed(models_dir, m) {
        return Err(SpeechError::NotInstalled(id.into()));
    }
    #[cfg(feature = "whisper")]
    {
        Ok(std::sync::Arc::new(whisper::Whisper::load(&models::model_dir(models_dir, m), m.id)?))
    }
    #[cfg(not(feature = "whisper"))]
    {
        Err(SpeechError::Unavailable("built without the `whisper` feature".into()))
    }
}

/// Whether this build can run speech models.
pub const fn available() -> bool {
    cfg!(feature = "whisper")
}

/// A word compared without case and the punctuation around it.
pub fn normalize_word(w: &str) -> String {
    w.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase()
}

/// Word error rate between a reference and a hypothesis (words compared without case or
/// punctuation, hyphenated words split): (substitutions + deletions + insertions) / reference
/// words.
pub fn word_error_rate(reference: &str, hypothesis: &str) -> f64 {
    let norm = |s: &str| -> Vec<String> { s.split(|c: char| c.is_whitespace() || c == '-').map(normalize_word).filter(|w| !w.is_empty()).collect() };
    let (r, h) = (norm(reference), norm(hypothesis));
    if r.is_empty() {
        return if h.is_empty() { 0.0 } else { 1.0 };
    }
    let mut prev: Vec<usize> = (0..=h.len()).collect();
    for (i, rw) in r.iter().enumerate() {
        let mut cur = vec![i + 1; h.len() + 1];
        for (j, hw) in h.iter().enumerate() {
            let sub = prev[j] + usize::from(rw != hw);
            cur[j + 1] = sub.min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        prev = cur;
    }
    prev[h.len()] as f64 / r.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_codes_are_distinct_iso_codes() {
        let set: std::collections::BTreeSet<&str> = WHISPER_LANGUAGES.iter().copied().collect();
        assert_eq!(set.len(), WHISPER_LANGUAGES.len());
        assert!(WHISPER_LANGUAGES.iter().all(|l| (2..=3).contains(&l.len()) && l.bytes().all(|b| b.is_ascii_lowercase())));
        assert_eq!(WHISPER_LANGUAGES.first(), Some(&"en"));
    }

    #[test]
    fn sample_times_are_exact() {
        assert_eq!(sample_time(16_000), Duration::from_secs(1));
        assert_eq!(sample_time(24_000), Duration::from_millis(1500));
        assert_eq!(time_sample(Duration::from_millis(1500)), 24_000);
        assert_eq!(time_sample(sample_time(123_457)), 123_457);
        assert_eq!(time_sample(Duration::MAX), u64::MAX);
    }

    #[test]
    fn wer_counts_edits() {
        assert_eq!(word_error_rate("the cat sat", "The cat, sat."), 0.0);
        assert!((word_error_rate("the cat sat on the mat", "the cat sat on mat") - 1.0 / 6.0).abs() < 1e-9);
        assert!((word_error_rate("a b", "a x b y") - 1.0).abs() < 1e-9);
        assert_eq!(word_error_rate("", ""), 0.0);
        assert_eq!(word_error_rate("", "x"), 1.0);
    }

    #[test]
    fn fixed_transcriber_trims_and_stops() {
        let mut t = Transcript::default();
        t.words.push(Word::new("hi", 1_600, 4_800));
        t.words.push(Word::new("late", 80_000, 88_000));
        let f = FixedTranscriber { transcript: t, id: "fixed".into() };
        let out = f.transcribe(&vec![0.0; 16_000], &Options::default(), &mut |_, _| true).unwrap();
        assert_eq!(out.words.len(), 1);
        assert_eq!(out.source, "fixed");
        assert_eq!(f.transcribe(&[], &Options::default(), &mut |_, _| false), Err(SpeechError::Canceled));
    }

    #[test]
    fn normalize_orders_and_cuts_overlaps() {
        let mut t = Transcript {
            words: vec![Word::new("b", 10, 30), Word::new(" ", 0, 5), Word::new("a", 0, 20), Word::new("c", 25, 25)],
            ..Default::default()
        };
        t.normalize();
        let got: Vec<_> = t.words.iter().map(|w| (w.text.as_str(), w.start, w.end)).collect();
        assert_eq!(got, [("a", 0, 10), ("b", 10, 25), ("c", 25, 25)]);
        assert_eq!(t.text(), "a b c");
        t.shift(u64::MAX);
        assert!(t.words.iter().all(|w| w.end >= w.start));
    }
}
