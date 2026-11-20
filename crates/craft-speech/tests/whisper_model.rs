//! Whisper end to end on real speech, when the weights are on this machine.
//!
//! The test never downloads. It looks for the model in `$CRAFT_SPEECH_MODELS_DIR/<id>/`, then in
//! `<workspace>/target/models/<id>/`. Speech samples are
//! `<workspace>/target/fixtures/speech/**/<name>.f32` (mono 16 kHz little-endian f32) with the
//! reference text in `<name>.txt`. Without either the test prints SKIPPED and passes.
#![cfg(feature = "whisper")]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use craft_speech::live::{LiveCaptioner, LiveConfig, LiveEvent};
use craft_speech::{Options, Transcriber, models, word_error_rate};

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn model_dir(id: &str) -> Option<PathBuf> {
    let m = models::find(id)?;
    let mut roots: Vec<PathBuf> = std::env::var_os("CRAFT_SPEECH_MODELS_DIR").map(PathBuf::from).into_iter().collect();
    roots.push(workspace().join("target/models"));
    roots.into_iter().find(|r| models::installed(r, m)).map(|r| models::model_dir(&r, m))
}

fn samples() -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    let Ok(dirs) = std::fs::read_dir(workspace().join("target/fixtures/speech")) else { return out };
    for d in dirs.flatten() {
        let Ok(files) = std::fs::read_dir(d.path()) else { continue };
        for f in files.flatten() {
            let p = f.path();
            if p.extension().is_some_and(|e| e == "f32")
                && let Ok(txt) = std::fs::read_to_string(p.with_extension("txt"))
            {
                out.push((p, txt.trim().to_string()));
            }
        }
    }
    out.sort();
    out
}

fn read_f32(p: &Path) -> Vec<f32> {
    std::fs::read(p).unwrap_or_default().as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect()
}

#[test]
fn tiny_model_transcribes_speech_whole_and_live() {
    let Some(dir) = model_dir("whisper-tiny") else {
        eprintln!("SKIPPED: whisper-tiny weights not found (see the test's module docs)");
        return;
    };
    let samples = samples();
    if samples.is_empty() {
        eprintln!("SKIPPED: no speech samples in target/fixtures/speech");
        return;
    }
    let w = Arc::new(craft_speech::whisper::Whisper::load(&dir, "whisper-tiny").unwrap());
    let opts = Options { language: Some("en".into()) };
    let (mut errs, mut live_errs, mut words) = (0.0, 0.0, 0usize);
    for (p, reference) in samples.iter().take(8) {
        let audio = read_f32(p);
        let t = w.transcribe(&audio, &opts, &mut |_, _| true).unwrap();
        let n = reference.split_whitespace().count();
        errs += word_error_rate(reference, &t.text()) * n as f64;
        words += n;
        assert!(t.words.windows(2).all(|p| p[0].end <= p[1].start), "{}", p.display());
        assert!(t.words.iter().all(|x| x.end <= audio.len() as u64), "{}", p.display());
        let mut c = LiveCaptioner::new(w.clone(), opts.clone(), LiveConfig::default(), Arc::new(AtomicBool::new(false)));
        let mut events = Vec::new();
        for piece in audio.chunks(1600) {
            events.extend(c.push(piece).unwrap());
        }
        events.extend(c.flush().unwrap());
        let live: Vec<String> = events.into_iter().filter_map(|e| if let LiveEvent::Final(p) = e { Some(p.text) } else { None }).collect();
        live_errs += word_error_rate(reference, &live.join(" ")) * n as f64;
    }
    let wer = errs / words.max(1) as f64;
    let live_wer = live_errs / words.max(1) as f64;
    eprintln!("whisper-tiny WER {:.1}% whole, {:.1}% live, over {words} words", wer * 100.0, live_wer * 100.0);
    assert!(wer < 0.25, "WER {wer}");
    assert!(live_wer < 0.35, "live WER {live_wer}");
}
