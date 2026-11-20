//! Live captions: audio arrives in pieces while it is spoken, and comes out as phrases.
//!
//! [`LiveCaptioner`] measures the level of every 10 ms frame of mono 16 kHz audio against a noise
//! floor it follows: a frame at least [`LiveConfig::voice_above_floor_db`] above the floor, and
//! no quieter than [`LiveConfig::min_voice_db`], is voice. The floor drops to a quieter frame at
//! once and rises slowly with quiet frames; it also rises to the quietest frame of the last 15 s,
//! so steady background sound stops counting as voice. Voice opens a phrase, which starts
//! [`LiveConfig::pre_roll`] before the first voiced frame so a soft first syllable is kept. While
//! the phrase is open, the audio so far is transcribed every [`LiveConfig::partial_every`] and
//! given as a [`LiveEvent::Partial`]; a pause of [`LiveConfig::min_pause`] ends the phrase, which is
//! transcribed once more as a whole and given as a [`LiveEvent::Final`] with its words. A phrase
//! that reaches [`LiveConfig::max_phrase`] ends there, and the speech that follows opens the next.
//! A phrase with less voice than [`LiveConfig::min_voice`] is a click or a breath and gives no
//! event.
//!
//! The captioner runs the transcriber on the caller's thread. When a transcription takes longer
//! than half the partial interval, the next partial waits twice as long as that transcription
//! took, so a slow model or computer falls behind by less. Positions are 16 kHz samples counted
//! from the first sample given.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::vad::{FRAME, level_db};
use crate::{Options, SpeechError, Transcriber, Word, time_sample};

/// How a [`LiveCaptioner`] cuts phrases.
#[derive(Clone, Debug, PartialEq)]
pub struct LiveConfig {
    /// A pause this long ends a phrase.
    pub min_pause: Duration,
    /// A phrase ends when it is this long (at most 28 s).
    pub max_phrase: Duration,
    /// How often the open phrase is transcribed for a partial result.
    pub partial_every: Duration,
    /// How much audio before the first voiced frame a phrase keeps.
    pub pre_roll: Duration,
    /// A phrase with less voice than this gives no event.
    pub min_voice: Duration,
    /// How far above the noise floor a frame is voice, in dB.
    pub voice_above_floor_db: f32,
    /// The quietest voice, in dBFS.
    pub min_voice_db: f32,
}

impl Default for LiveConfig {
    fn default() -> Self {
        LiveConfig {
            min_pause: Duration::from_millis(600),
            max_phrase: Duration::from_secs(12),
            partial_every: Duration::from_secs(1),
            pre_roll: Duration::from_millis(200),
            min_voice: Duration::from_millis(200),
            voice_above_floor_db: 12.0,
            min_voice_db: -50.0,
        }
    }
}

/// A finished phrase.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct Phrase {
    /// `[start, end)` of the phrase's audio, in 16 kHz samples.
    pub start: u64,
    pub end: u64,
    /// The words joined by spaces.
    pub text: String,
    /// Each word's place in the audio, counted from the first sample given to the captioner.
    pub words: Vec<Word>,
}

/// What [`LiveCaptioner::push`] reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LiveEvent {
    /// The phrase being spoken, as far as it has been heard; a later event replaces it.
    Partial { start: u64, text: String },
    /// The phrase, finished. Its text may be empty when nothing was recognized.
    Final(Phrase),
}

struct Open {
    start: u64,
    audio: Vec<f32>,
    /// Samples of voice in the phrase.
    voiced: u64,
    /// The end of the last voiced frame.
    voice_end: u64,
    /// Frames of silence since that frame.
    silent: usize,
    /// Samples added since the last partial result.
    since_partial: u64,
}

/// Cuts a stream of mono 16 kHz audio into phrases and transcribes them; see the module docs.
pub struct LiveCaptioner {
    recognizer: Arc<dyn Transcriber>,
    opts: Options,
    cfg: LiveConfig,
    stop: Arc<AtomicBool>,
    /// Samples consumed, frames and the partial frame alike.
    pos: u64,
    frame: Vec<f32>,
    floor_db: f32,
    /// The levels of the last [`FLOOR_WINDOW`] frames.
    recent_db: VecDeque<f32>,
    /// The last `pre_roll` of audio while no phrase is open.
    pre_roll: VecDeque<f32>,
    phrase: Option<Open>,
    /// Samples between partial results, at least `partial_every`.
    partial_gap: u64,
    last_partial: String,
}

/// The noise floor a captioner starts from, in dBFS.
const START_FLOOR_DB: f32 = -70.0;
/// Frames whose quietest level the floor rises to (15 s): sound with no quieter frame for that
/// long is background, not speech.
const FLOOR_WINDOW: usize = 1500;
/// The longest phrase a captioner keeps (one recognizer window is 30 s).
const MAX_PHRASE: Duration = Duration::from_secs(28);

impl LiveCaptioner {
    /// A captioner that transcribes with `recognizer`. Setting `stop` stops a transcription that
    /// is running and every later one with [`SpeechError::Canceled`].
    pub fn new(recognizer: Arc<dyn Transcriber>, opts: Options, mut cfg: LiveConfig, stop: Arc<AtomicBool>) -> LiveCaptioner {
        let frame_time = Duration::from_millis(10);
        cfg.max_phrase = cfg.max_phrase.clamp(Duration::from_secs(1), MAX_PHRASE);
        cfg.min_pause = cfg.min_pause.clamp(frame_time, cfg.max_phrase);
        cfg.partial_every = cfg.partial_every.max(Duration::from_millis(100));
        cfg.pre_roll = cfg.pre_roll.min(Duration::from_secs(2));
        cfg.min_voice = cfg.min_voice.min(cfg.max_phrase);
        if !cfg.voice_above_floor_db.is_finite() {
            cfg.voice_above_floor_db = LiveConfig::default().voice_above_floor_db;
        }
        if !cfg.min_voice_db.is_finite() {
            cfg.min_voice_db = LiveConfig::default().min_voice_db;
        }
        let partial_gap = time_sample(cfg.partial_every);
        LiveCaptioner {
            recognizer,
            opts,
            cfg,
            stop,
            pos: 0,
            frame: Vec::with_capacity(FRAME),
            floor_db: START_FLOOR_DB,
            recent_db: VecDeque::with_capacity(FLOOR_WINDOW),
            pre_roll: VecDeque::new(),
            phrase: None,
            partial_gap,
            last_partial: String::new(),
        }
    }

    /// Samples given so far.
    pub fn position(&self) -> u64 {
        self.pos
    }

    /// Every phrase that ends before this sample has been given as a [`LiveEvent::Final`]: the
    /// start of the open phrase, or the position when none is open.
    pub fn finalized_until(&self) -> u64 {
        self.phrase.as_ref().map_or(self.pos.saturating_sub(self.frame.len() as u64), |p| p.start)
    }

    /// Whether a phrase is open.
    pub fn in_phrase(&self) -> bool {
        self.phrase.is_some()
    }

    /// Take more audio. Returns what it brought, in order.
    pub fn push(&mut self, audio: &[f32]) -> Result<Vec<LiveEvent>, SpeechError> {
        let mut events = Vec::new();
        for &x in audio {
            self.frame.push(if x.is_finite() { x } else { 0.0 });
            self.pos = self.pos.saturating_add(1);
            if self.frame.len() == FRAME {
                let frame = std::mem::take(&mut self.frame);
                self.on_frame(&frame, &mut events)?;
                self.frame = frame;
                self.frame.clear();
            }
        }
        Ok(events)
    }

    /// End the open phrase now (the audio stopped).
    pub fn flush(&mut self) -> Result<Vec<LiveEvent>, SpeechError> {
        let mut events = Vec::new();
        if let Some(p) = self.phrase.take() {
            self.finish(p, &mut events)?;
        }
        Ok(events)
    }

    /// `samples` of audio were lost: the open phrase ends, and counting goes on after the gap.
    pub fn gap(&mut self, samples: u64) -> Result<Vec<LiveEvent>, SpeechError> {
        let events = self.flush()?;
        self.frame.clear();
        self.pre_roll.clear();
        self.pos = self.pos.saturating_add(samples);
        Ok(events)
    }

    fn on_frame(&mut self, frame: &[f32], events: &mut Vec<LiveEvent>) -> Result<(), SpeechError> {
        let db = level_db(frame);
        if self.recent_db.len() == FLOOR_WINDOW {
            self.recent_db.pop_front();
        }
        self.recent_db.push_back(if db.is_finite() { db } else { -120.0 });
        if self.recent_db.len() == FLOOR_WINDOW {
            let quietest = self.recent_db.iter().copied().fold(f32::INFINITY, f32::min);
            if quietest > self.floor_db {
                self.floor_db = quietest;
            }
        }
        let voice = db >= (self.floor_db + self.cfg.voice_above_floor_db).max(self.cfg.min_voice_db);
        if !voice {
            // The floor follows quiet frames: at once when it drops, slowly when it rises.
            self.floor_db = if db < self.floor_db { db.max(-120.0) } else { self.floor_db + (db - self.floor_db) * 0.02 };
        }
        let frame_end = self.pos;
        let frame_start = frame_end.saturating_sub(FRAME as u64);
        let Some(p) = &mut self.phrase else {
            if voice {
                let mut audio: Vec<f32> = self.pre_roll.drain(..).collect();
                let start = frame_start.saturating_sub(audio.len() as u64);
                audio.extend_from_slice(frame);
                self.phrase = Some(Open { start, audio, voiced: FRAME as u64, voice_end: frame_end, silent: 0, since_partial: 0 });
                self.last_partial.clear();
            } else {
                self.pre_roll.extend(frame);
                let keep = time_sample(self.cfg.pre_roll) as usize;
                while self.pre_roll.len() > keep {
                    self.pre_roll.pop_front();
                }
            }
            return Ok(());
        };
        p.audio.extend_from_slice(frame);
        p.since_partial = p.since_partial.saturating_add(FRAME as u64);
        if voice {
            p.voiced = p.voiced.saturating_add(FRAME as u64);
            p.voice_end = frame_end;
            p.silent = 0;
        } else {
            p.silent += 1;
        }
        let paused = time_sample(self.cfg.min_pause) <= (p.silent * FRAME) as u64;
        let full = p.audio.len() as u64 >= time_sample(self.cfg.max_phrase);
        if paused || full {
            if let Some(mut p) = self.phrase.take() {
                if paused {
                    // Keep 100 ms after the last voice; the rest of the pause is not the phrase's.
                    let keep = p.voice_end.saturating_sub(p.start).saturating_add(time_sample(Duration::from_millis(100)));
                    p.audio.truncate(usize::try_from(keep).unwrap_or(usize::MAX));
                }
                self.finish(p, events)?;
            }
            return Ok(());
        }
        if p.since_partial >= self.partial_gap && p.voiced >= time_sample(self.cfg.min_voice) {
            p.since_partial = 0;
            let (start, audio) = (p.start, p.audio.clone());
            let text = self.transcribe(&audio)?.text();
            if text != self.last_partial && !text.is_empty() {
                self.last_partial.clone_from(&text);
                events.push(LiveEvent::Partial { start, text });
            }
        }
        Ok(())
    }

    fn finish(&mut self, p: Open, events: &mut Vec<LiveEvent>) -> Result<(), SpeechError> {
        self.last_partial.clear();
        if p.voiced < time_sample(self.cfg.min_voice) {
            return Ok(());
        }
        let mut t = self.transcribe(&p.audio)?;
        t.shift(p.start);
        let end = p.start.saturating_add(p.audio.len() as u64);
        events.push(LiveEvent::Final(Phrase { start: p.start, end, text: t.text(), words: t.words }));
        Ok(())
    }

    /// Transcribe `audio`, and wait longer before the next partial when that took long.
    fn transcribe(&mut self, audio: &[f32]) -> Result<crate::Transcript, SpeechError> {
        if self.stop.load(Ordering::Relaxed) {
            return Err(SpeechError::Canceled);
        }
        let begun = Instant::now();
        let stop = Arc::clone(&self.stop);
        let mut t = self.recognizer.transcribe(audio, &self.opts, &mut |_, _| !stop.load(Ordering::Relaxed))?;
        t.normalize();
        let took = begun.elapsed();
        self.partial_gap = time_sample(self.cfg.partial_every.max(took.saturating_mul(2)));
        Ok(t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ProgressFn, SAMPLE_RATE, Transcript};
    use std::sync::Mutex;

    fn at(seconds: f64) -> usize {
        (seconds * f64::from(SAMPLE_RATE)) as usize
    }

    /// Quiet noise with a tone over each `(from, to)` range.
    fn audio(seconds: f64, tones: &[(f64, f64)]) -> Vec<f32> {
        let mut a: Vec<f32> = (0..at(seconds)).map(|i| if i % 2 == 0 { 0.0005 } else { -0.0005 }).collect();
        for &(from, to) in tones {
            for (i, v) in a.iter_mut().enumerate().take(at(to)).skip(at(from)) {
                *v = 0.3 * (i as f32 * 0.2).sin();
            }
        }
        a
    }

    /// Answers "hello world" for any audio of at least 0.5 s and "hi" for shorter audio, and
    /// records the length of each piece it was given.
    struct Echo {
        calls: Mutex<Vec<usize>>,
    }

    impl Transcriber for Echo {
        fn id(&self) -> String {
            "echo".into()
        }
        fn transcribe(&self, audio: &[f32], _: &Options, progress: ProgressFn) -> Result<Transcript, SpeechError> {
            if !progress(0.0, "") {
                return Err(SpeechError::Canceled);
            }
            if let Ok(mut c) = self.calls.lock() {
                c.push(audio.len());
            }
            let n = audio.len() as u64;
            let words =
                if n >= at(0.5) as u64 { vec![Word::new("hello", 0, n / 2), Word::new("world", n / 2, n)] } else { vec![Word::new("hi", 0, n)] };
            Ok(Transcript { language: "en".into(), source: "echo".into(), words })
        }
    }

    fn captioner(cfg: LiveConfig) -> (LiveCaptioner, Arc<Echo>, Arc<AtomicBool>) {
        let echo = Arc::new(Echo { calls: Mutex::new(Vec::new()) });
        let stop = Arc::new(AtomicBool::new(false));
        (LiveCaptioner::new(echo.clone(), Options::default(), cfg, Arc::clone(&stop)), echo, stop)
    }

    fn finals(events: &[LiveEvent]) -> Vec<&Phrase> {
        events.iter().filter_map(|e| if let LiveEvent::Final(p) = e { Some(p) } else { None }).collect()
    }

    #[test]
    fn speech_between_pauses_is_one_phrase_with_its_pre_roll() {
        let (mut c, _, _) = captioner(LiveConfig::default());
        let events = c.push(&audio(4.0, &[(1.0, 2.5)])).unwrap();
        let f = finals(&events);
        assert_eq!(f.len(), 1, "{events:?}");
        let p = f[0];
        assert_eq!(p.text, "hello world");
        assert_eq!(p.start, at(0.8) as u64, "200 ms before the voice");
        assert_eq!(p.end, at(2.6) as u64, "100 ms after the voice");
        assert_eq!(p.words[0].start, p.start, "word positions count from the stream's start");
        assert_eq!(p.words[1].end, p.end);
        assert!(!c.in_phrase());
        assert_eq!(c.finalized_until(), c.position());
    }

    #[test]
    fn a_long_phrase_gives_partials_before_its_final() {
        let (mut c, _, _) = captioner(LiveConfig::default());
        let events = c.push(&audio(5.0, &[(0.5, 4.0)])).unwrap();
        let partials = events.iter().filter(|e| matches!(e, LiveEvent::Partial { .. })).count();
        // "hello world" repeats as long as the text stays the same, so one partial is given.
        assert_eq!(partials, 1, "{events:?}");
        assert!(matches!(events.first(), Some(LiveEvent::Partial { start, .. }) if *start == at(0.3) as u64));
        assert_eq!(finals(&events).len(), 1);
    }

    #[test]
    fn the_result_is_the_same_however_the_audio_is_cut() {
        let a = audio(6.0, &[(0.5, 1.5), (2.5, 4.0)]);
        let (mut whole, _, _) = captioner(LiveConfig::default());
        let one = whole.push(&a).unwrap();
        let (mut parts, _, _) = captioner(LiveConfig::default());
        let mut many = Vec::new();
        for piece in a.chunks(1023) {
            many.extend(parts.push(piece).unwrap());
        }
        assert_eq!(one, many);
        assert_eq!(finals(&one).len(), 2);
    }

    #[test]
    fn a_phrase_at_its_longest_ends_and_the_speech_goes_on_in_the_next() {
        let cfg = LiveConfig { max_phrase: Duration::from_secs(2), ..LiveConfig::default() };
        let (mut c, _, _) = captioner(cfg);
        let mut events = c.push(&audio(5.0, &[(0.0, 5.0)])).unwrap();
        events.extend(c.flush().unwrap());
        let f = finals(&events);
        assert!(f.len() >= 2, "{events:?}");
        assert!(f.iter().all(|p| p.end - p.start <= at(2.0) as u64));
        assert!(f.windows(2).all(|w| w[0].end <= w[1].start));
    }

    #[test]
    fn steady_background_sound_stops_counting_as_voice() {
        let (mut c, _, _) = captioner(LiveConfig::default());
        // A hum at about -29 dBFS, well above the starting floor.
        let hum = |seconds: f64| -> Vec<f32> { (0..at(seconds)).map(|i| 0.05 * (i as f32 * 0.07).sin()).collect() };
        let events = c.push(&hum(40.0)).unwrap();
        let starts: Vec<u64> = events
            .iter()
            .map(|e| match e {
                LiveEvent::Partial { start, .. } => *start,
                LiveEvent::Final(p) => p.start,
            })
            .collect();
        assert!(!starts.is_empty(), "the sound counts as voice until the floor rises");
        assert!(starts.iter().all(|s| *s < at(16.0) as u64), "{starts:?}");
        assert!(!c.in_phrase(), "no phrase is open after 40 s of the same sound");
        // Speech louder than the background is still heard.
        let mut louder = hum(4.0);
        for (i, v) in louder.iter_mut().enumerate().take(at(2.5)).skip(at(1.0)) {
            *v = 0.3 * (i as f32 * 0.2).sin();
        }
        let f = c.push(&louder).unwrap();
        assert_eq!(finals(&f).len(), 1, "{f:?}");
    }

    #[test]
    fn a_click_gives_no_phrase_and_silence_calls_nothing() {
        let (mut c, echo, _) = captioner(LiveConfig::default());
        assert!(c.push(&audio(3.0, &[(1.0, 1.05)])).unwrap().is_empty());
        assert!(c.push(&vec![0.0; at(10.0)]).unwrap().is_empty());
        assert!(echo.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn flush_and_gap_end_the_open_phrase() {
        let (mut c, _, _) = captioner(LiveConfig::default());
        assert!(finals(&c.push(&audio(1.0, &[(0.2, 1.0)])).unwrap()).is_empty());
        assert!(c.in_phrase());
        assert_eq!(c.finalized_until(), at(0.0) as u64);
        let f = c.gap(at(5.0) as u64).unwrap();
        assert_eq!(finals(&f).len(), 1);
        assert_eq!(c.position(), at(6.0) as u64);
        assert!(c.flush().unwrap().is_empty());
    }

    #[test]
    fn a_stop_cancels_the_transcription() {
        let (mut c, _, stop) = captioner(LiveConfig::default());
        stop.store(true, Ordering::Relaxed);
        assert_eq!(c.push(&audio(4.0, &[(1.0, 2.5)])), Err(SpeechError::Canceled));
    }

    #[test]
    fn hostile_settings_and_samples_never_panic() {
        let cfg = LiveConfig {
            min_pause: Duration::ZERO,
            max_phrase: Duration::MAX,
            partial_every: Duration::ZERO,
            pre_roll: Duration::MAX,
            min_voice: Duration::MAX,
            voice_above_floor_db: f32::NAN,
            min_voice_db: f32::INFINITY,
        };
        let (mut c, _, _) = captioner(cfg);
        let mut a = audio(3.0, &[(0.5, 2.0)]);
        a[100] = f32::NAN;
        a[200] = f32::INFINITY;
        c.push(&a).unwrap();
        c.gap(u64::MAX).unwrap();
        c.push(&a).unwrap();
        c.flush().unwrap();
    }
}
