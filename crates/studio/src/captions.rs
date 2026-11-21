//! Live captions of the program audio.
//!
//! `captions.start` adds an output queue to the mixer, so the captions get the program mix the
//! recordings get. Two threads run while captions run:
//!
//! - The reader takes each 48 kHz stereo block from the queue within about 10 ms, converts it to
//!   mono 16 kHz, and keeps it in a buffer of at most [`BUFFER_SECONDS`]. A block whose time does
//!   not follow the block before it, and audio pushed out of a full buffer, are kept as gaps.
//! - The recognizer loads the speech model, then gives the buffered audio to a
//!   [`LiveCaptioner`]. Its partial phrases and finished phrases go to the [`CaptionBoard`] the
//!   Live Captions sources draw, and each finished phrase is kept as a cue timed on the studio
//!   clock.
//!
//! `captions.stop` removes the queue. The recognizer finishes what it was given, ends the open
//! phrase and stops. A recording that ran while captions ran gets caption files beside it when
//! every phrase spoken before it stopped is final, when captions stop, or 30 s after the recording
//! stopped, whichever comes first: `<recording name>.srt`, `.vtt` and `.txt`, written on a
//! `captions-files` thread. A file of that name that already exists is not replaced.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use craft_speech::live::{LiveCaptioner, LiveConfig, LiveEvent};
use craft_speech::resample::Downsampler;
use craft_speech::{Options, SpeechError, Transcriber, models};
use serde_json::{Value, json};
use studiocraft_engine::mixer::MixerCmd;
use studiocraft_media::Tick;
use studiocraft_media::audio::{AudioBlock, BLOCK_FRAMES, CHANNELS, PooledBlock, SAMPLE_RATE};
use studiocraft_media::queue::Fifo;
use studiocraft_sources::CaptionBoard;
use studiocraft_sources::captions::CAPTIONS_ID;

use crate::outputs::OutputKind;
use crate::{Notice, Studio, StudioError};

/// The mixer output id of the captions' queue; recordings and streams count up from 1.
pub(crate) const CAPTIONS_OUTPUT: u64 = u64::MAX;
/// Blocks the captions' queue holds (about 1.4 s); the reader empties it every 10 ms.
const QUEUE_BLOCKS: usize = 64;
/// The most audio the reader keeps for the recognizer, in seconds.
pub(crate) const BUFFER_SECONDS: u64 = 60;
/// Cues kept for caption files; older ones are dropped first.
const MAX_CUES: usize = 20_000;
/// How long after a recording stops its caption files wait for its last phrase.
const SIDECAR_WAIT: Duration = Duration::from_secs(30);
/// How long closing the studio waits for the recognizer to finish the audio it holds.
const CLOSE_WAIT: Duration = Duration::from_secs(10);
/// How long closing the studio then waits for a stopped recognizer to end.
const CANCEL_WAIT: Duration = Duration::from_secs(2);
/// How long the reader keeps reading after captions stop while the mixer still holds the queue
/// (a mixer without a thread never lets it go).
const DRAIN_WAIT: Duration = Duration::from_millis(500);
/// How long the reader and the recognizer wait when they have nothing to do.
const READ_EVERY: Duration = Duration::from_millis(10);
const IDLE: Duration = Duration::from_millis(20);

/// Loads the recognizer for a model id from the models folder.
pub type Loader = Arc<dyn Fn(&Path, &str) -> Result<Arc<dyn Transcriber>, SpeechError> + Send + Sync>;

fn speech_loader() -> Loader {
    Arc::new(|dir: &Path, id: &str| craft_speech::load(dir, id))
}

/// Whether a path in the models folder may be looked at; see [`Studio::set_model_file_filter`].
pub type ModelFileFilter = Arc<dyn Fn(&Path) -> bool + Send + Sync>;

/// The folder and the files of model `m` under `models_dir`, the folder last.
fn model_paths(models_dir: &Path, m: &models::ModelInfo) -> Vec<PathBuf> {
    let dir = models::model_dir(models_dir, m);
    let mut paths: Vec<PathBuf> = m.files.iter().map(|f| dir.join(f.name)).collect();
    paths.push(dir);
    paths
}

/// Where speech models are kept unless [`Studio::set_models_dir`] says otherwise.
pub fn default_models_dir() -> PathBuf {
    dirs::data_local_dir().or_else(dirs::home_dir).unwrap_or_else(|| PathBuf::from(".")).join("StudioCraft").join("models")
}

/// Where live captions are.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaptionState {
    Loading,
    Running,
    /// Finishing the audio it was given after a stop.
    Stopping,
    Stopped,
    Failed(String),
}

impl CaptionState {
    fn text(&self) -> String {
        match self {
            CaptionState::Loading => "loading".into(),
            CaptionState::Running => "running".into(),
            CaptionState::Stopping => "stopping".into(),
            CaptionState::Stopped => "stopped".into(),
            CaptionState::Failed(e) => format!("failed: {e}"),
        }
    }
}

/// One piece of the reader's buffer.
#[derive(Debug, PartialEq)]
enum Chunk {
    Audio(Vec<f32>),
    /// Samples (16 kHz) of audio that never arrived or were pushed out.
    Gap(u64),
}

#[derive(Default)]
struct Buffer {
    chunks: VecDeque<Chunk>,
    samples: u64,
}

impl Buffer {
    /// Add a chunk. A gap next to a gap joins it, so the buffer holds at most one gap between two
    /// pieces of audio.
    fn push(&mut self, chunk: Chunk) {
        match (chunk, self.chunks.back_mut()) {
            (Chunk::Gap(n), Some(Chunk::Gap(m))) => *m = m.saturating_add(n),
            (chunk, _) => {
                if let Chunk::Audio(a) = &chunk {
                    self.samples = self.samples.saturating_add(a.len() as u64);
                }
                self.chunks.push_back(chunk);
            }
        }
        let cap = BUFFER_SECONDS * u64::from(craft_speech::SAMPLE_RATE);
        // The oldest audio becomes one gap at the front, so later audio keeps its time.
        let mut lead = 0u64;
        while self.samples > cap {
            match self.chunks.pop_front() {
                Some(Chunk::Audio(a)) => {
                    let n = a.len() as u64;
                    self.samples = self.samples.saturating_sub(n);
                    lead = lead.saturating_add(n);
                }
                Some(Chunk::Gap(n)) => lead = lead.saturating_add(n),
                None => break,
            }
        }
        if lead > 0 {
            match self.chunks.front_mut() {
                Some(Chunk::Gap(m)) => *m = m.saturating_add(lead),
                _ => self.chunks.push_front(Chunk::Gap(lead)),
            }
        }
    }

    fn take(&mut self) -> Vec<Chunk> {
        self.samples = 0;
        self.chunks.drain(..).collect()
    }
}

/// What the threads share with the studio.
struct Shared {
    state: Mutex<CaptionState>,
    buffer: Mutex<Buffer>,
    /// The studio clock's time of the first sample the reader kept, in ticks.
    base: Mutex<Option<Tick>>,
    /// The time before which every phrase has been given as final, in ticks.
    final_until: AtomicI64,
    /// Phrases finished since the studio last took them.
    cues: Mutex<VecDeque<craft_captions::Cue>>,
    /// The language setting of the run: a code, or `auto`.
    language: Mutex<String>,
    /// Set by a stop: the reader ends, and the recognizer ends once its buffer is empty.
    ending: AtomicBool,
    /// Set when the reader has ended.
    read_all: AtomicBool,
    /// Set when the studio closes and the recognizer has not finished in time: a running
    /// transcription stops at once.
    cancel: Arc<AtomicBool>,
    /// Blocks the reader found out of step with the block before them.
    gaps: AtomicU64,
}

impl Shared {
    fn set_state(&self, s: CaptionState) {
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = s;
    }
    fn state(&self) -> CaptionState {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// Mono 16 kHz audio from the mixer's blocks, with gaps where blocks are missing.
struct Reader {
    down: Downsampler,
    /// The time the next block is expected at.
    next: Option<Tick>,
    out: Vec<f32>,
}

impl Reader {
    fn new() -> Reader {
        Reader { down: Downsampler::new(SAMPLE_RATE, CHANNELS), next: None, out: Vec::new() }
    }

    fn block(&mut self, b: &AudioBlock, shared: &Shared) {
        let step = Tick::from_units(BLOCK_FRAMES as i64, i64::from(SAMPLE_RATE));
        let per_sample = Tick::from_units(1, i64::from(craft_speech::SAMPLE_RATE)).0.max(1);
        let mut buffer = shared.buffer.lock().unwrap_or_else(|e| e.into_inner());
        match self.next {
            None => *shared.base.lock().unwrap_or_else(|e| e.into_inner()) = Some(b.pts),
            Some(next) if b.pts.0 > next.0.saturating_add(step.0 / 2) => {
                buffer.push(Chunk::Gap(u64::try_from(b.pts.0.saturating_sub(next.0) / per_sample).unwrap_or(0)));
                shared.gaps.fetch_add(1, Ordering::Relaxed);
            }
            // A block from before the one expected (the mixer's clock does not go back, so this
            // is a repeat): it is skipped.
            Some(next) if b.pts.0 < next.0.saturating_sub(step.0 / 2) => return,
            Some(_) => {}
        }
        self.next = Some(b.pts.saturating_add(step));
        self.out.clear();
        self.down.push(&b.samples, &mut self.out);
        if !self.out.is_empty() {
            buffer.push(Chunk::Audio(std::mem::take(&mut self.out)));
        }
    }
}

/// Whether the reader stops: at once when canceled; after captions stop, once the mixer has let
/// go of the queue (it adds no block after that) or `waited` reaches [`DRAIN_WAIT`].
fn reader_done(canceled: bool, ending: bool, mixer_holds_queue: bool, waited: Duration) -> bool {
    canceled || (ending && (!mixer_holds_queue || waited >= DRAIN_WAIT))
}

fn read_loop(queue: Arc<Fifo<PooledBlock>>, shared: Arc<Shared>) {
    let mut reader = Reader::new();
    let mut ending_since: Option<std::time::Instant> = None;
    loop {
        while let Some(b) = queue.pop() {
            reader.block(&b, &shared);
        }
        let ending = shared.ending.load(Ordering::Relaxed);
        let waited = if ending { ending_since.get_or_insert_with(std::time::Instant::now).elapsed() } else { Duration::ZERO };
        // The mixer drops its handle when it removes the output.
        let mixer_holds_queue = Arc::strong_count(&queue) > 1;
        if reader_done(shared.cancel.load(Ordering::Relaxed), ending, mixer_holds_queue, waited) {
            // A block the mixer added just before it let go.
            while let Some(b) = queue.pop() {
                reader.block(&b, &shared);
            }
            break;
        }
        std::thread::sleep(READ_EVERY);
    }
    shared.read_all.store(true, Ordering::Release);
}

/// The time of 16 kHz sample `n` after `base`.
fn sample_tick(base: Tick, n: u64) -> Tick {
    base.saturating_add(Tick::from_units(i64::try_from(n).unwrap_or(i64::MAX), i64::from(craft_speech::SAMPLE_RATE)))
}

/// Pass the captioner's events to the board and the cues.
fn publish(events: Vec<LiveEvent>, base: Tick, board: &CaptionBoard, shared: &Shared, now: Tick) {
    for e in events {
        match e {
            LiveEvent::Partial { text, .. } => board.set_partial(&text, now),
            LiveEvent::Final(p) => {
                board.push_final(&p.text, now);
                if p.text.trim().is_empty() {
                    continue;
                }
                let mut cues = shared.cues.lock().unwrap_or_else(|e| e.into_inner());
                cues.push_back(craft_captions::Cue {
                    start: sample_tick(base, p.start),
                    end: sample_tick(base, p.end),
                    text: p.text,
                    ..Default::default()
                });
                while cues.len() > MAX_CUES {
                    cues.pop_front();
                }
            }
        }
    }
}

struct RecognizeJob {
    loader: Loader,
    models_dir: PathBuf,
    model: String,
    language: Option<String>,
    board: Arc<CaptionBoard>,
    clock: studiocraft_media::clock::MasterClock,
}

fn recognize_loop(job: RecognizeJob, shared: Arc<Shared>) {
    let recognizer = match (job.loader)(&job.models_dir, &job.model) {
        Ok(r) => r,
        Err(e) => {
            shared.set_state(CaptionState::Failed(e.to_string()));
            shared.ending.store(true, Ordering::Relaxed);
            return;
        }
    };
    if shared.state() == CaptionState::Loading {
        shared.set_state(CaptionState::Running);
    }
    let opts = Options { language: job.language.clone() };
    let mut captioner = LiveCaptioner::new(recognizer, opts, LiveConfig::default(), Arc::clone(&shared.cancel));
    let result = (|| -> Result<(), SpeechError> {
        loop {
            let read_all = shared.read_all.load(Ordering::Acquire);
            let chunks = shared.buffer.lock().unwrap_or_else(|e| e.into_inner()).take();
            let base = (*shared.base.lock().unwrap_or_else(|e| e.into_inner())).unwrap_or(Tick(0));
            if chunks.is_empty() {
                if read_all {
                    let events = captioner.flush()?;
                    publish(events, base, &job.board, &shared, job.clock.now());
                    return Ok(());
                }
                std::thread::sleep(IDLE);
                continue;
            }
            for c in chunks {
                let events = match c {
                    Chunk::Audio(a) => captioner.push(&a)?,
                    Chunk::Gap(n) => captioner.gap(n)?,
                };
                publish(events, base, &job.board, &shared, job.clock.now());
                shared.final_until.store(sample_tick(base, captioner.finalized_until()).0, Ordering::Release);
            }
        }
    })();
    shared.final_until.store(i64::MAX, Ordering::Release);
    match result {
        Ok(()) | Err(SpeechError::Canceled) => shared.set_state(CaptionState::Stopped),
        Err(e) => {
            // The phrase that was being heard is not finished, so it is not left on screen.
            job.board.set_partial("", job.clock.now());
            shared.ending.store(true, Ordering::Relaxed);
            shared.set_state(CaptionState::Failed(e.to_string()));
        }
    }
}

struct Worker {
    shared: Arc<Shared>,
    model: String,
    handles: Vec<JoinHandle<()>>,
    /// Whether the end of the run was told to the user.
    reported: bool,
}

impl Worker {
    fn finished(&self) -> bool {
        self.handles.iter().all(JoinHandle::is_finished)
    }
    fn join(&mut self) {
        for h in self.handles.drain(..) {
            let _ = h.join();
        }
    }
}

struct Download {
    model: String,
    cancel: Arc<AtomicBool>,
    done: Arc<AtomicU64>,
    total: Arc<AtomicU64>,
    handle: JoinHandle<Result<(), SpeechError>>,
}

/// A recording that ran while captions ran, waiting for its caption files.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Sidecar {
    pub recording: PathBuf,
    pub start: Tick,
    pub end: Tick,
}

/// Which recordings get caption files: [`SidecarWatch::observe`] follows the recording and
/// whether captions run, and returns each recording that stopped after captions ran during it.
#[derive(Default)]
pub(crate) struct SidecarWatch {
    recording: Option<(Tick, PathBuf)>,
    captioned: bool,
}

impl SidecarWatch {
    /// Follow the recording that started at `start` (a recording is told apart by its start).
    /// `target` gives its file and is called only when a new recording is seen, so a frame that
    /// sees the same recording makes no copies.
    pub(crate) fn observe(
        &mut self,
        start: Option<Tick>,
        target: impl FnOnce() -> Option<PathBuf>,
        captions_running: bool,
        now: Tick,
    ) -> Option<Sidecar> {
        let ended = match (&self.recording, start) {
            (Some(a), Some(b)) if a.0 == b => None,
            (Some(_), _) => self.recording.take(),
            (None, _) => None,
        };
        let captioned = self.captioned;
        if ended.is_some() {
            self.captioned = false;
        }
        let done = ended.filter(|_| captioned).map(|(start, path)| Sidecar { recording: path, start, end: now });
        if self.recording.is_none()
            && let Some(start) = start
            && let Some(path) = target()
        {
            self.recording = Some((start, path));
            self.captioned = false;
        }
        if self.recording.is_some() && captions_running {
            self.captioned = true;
        }
        done
    }

    /// The start of the recording being watched.
    fn start(&self) -> Option<Tick> {
        self.recording.as_ref().map(|r| r.0)
    }
}

/// The cues of `[start, end)` counted from `start`, as a caption document.
pub(crate) fn sidecar_document(cues: &[craft_captions::Cue], start: Tick, end: Tick, language: &str) -> craft_captions::Document {
    let mut doc =
        craft_captions::Document { language: (!language.is_empty() && language != "auto").then(|| language.to_string()), ..Default::default() };
    for c in cues {
        if c.end.0 <= start.0 || c.start.0 >= end.0 {
            continue;
        }
        let s = Tick(c.start.0.max(start.0).saturating_sub(start.0));
        let e = Tick(c.end.0.min(end.0).saturating_sub(start.0));
        doc.cues.push(craft_captions::Cue { start: s, end: e, text: c.text.clone(), ..Default::default() });
    }
    doc.normalize();
    doc
}

/// Write `<recording>.srt`, `.vtt` and `.txt` for `doc`; the paths written. A file that exists is
/// left as it is and reported as an error, and a file that could not be written in full is removed.
pub(crate) fn write_sidecars(recording: &Path, doc: &craft_captions::Document) -> Result<Vec<PathBuf>, String> {
    use std::io::Write;
    let text: String = doc.cues.iter().map(|c| c.text.replace('\n', " ")).collect::<Vec<_>>().join("\n");
    let files = [
        ("srt", craft_captions::write(doc, craft_captions::Format::Srt, craft_captions::WriteOptions::default())),
        ("vtt", craft_captions::write(doc, craft_captions::Format::WebVtt, craft_captions::WriteOptions::default())),
        ("txt", if text.is_empty() { Vec::new() } else { format!("{text}\n").into_bytes() }),
    ];
    let mut written = Vec::new();
    let mut errors = Vec::new();
    for (ext, bytes) in files {
        let path = recording.with_extension(ext);
        let mut file = match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(f) => f,
            Err(e) => {
                errors.push(format!("{}: {e}", path.display()));
                continue;
            }
        };
        match file.write_all(&bytes).and_then(|()| file.sync_all()) {
            Ok(()) => written.push(path),
            Err(e) => {
                drop(file);
                let _ = std::fs::remove_file(&path);
                errors.push(format!("{}: {e}", path.display()));
            }
        }
    }
    if errors.is_empty() { Ok(written) } else { Err(errors.join("; ")) }
}

/// A language setting as given: `auto` or one of the codes the speech models recognize.
fn valid_language(l: &str) -> bool {
    l == "auto" || craft_speech::WHISPER_LANGUAGES.contains(&l)
}

pub(crate) struct Captions {
    board: Arc<CaptionBoard>,
    models_dir: PathBuf,
    loader: Loader,
    /// Set when [`Studio::set_caption_loader`] gave the loader.
    own_loader: bool,
    worker: Option<Worker>,
    download: Option<Download>,
    watch: SidecarWatch,
    pending: Vec<Sidecar>,
    /// Finished phrases a recording may still need, kept after the run that heard them ends.
    cues: VecDeque<craft_captions::Cue>,
    /// The language setting of the last run.
    language: String,
    /// Why the last run failed, kept until the next start.
    failure: Option<String>,
    /// Caption files being written.
    writes: Vec<JoinHandle<Result<Vec<PathBuf>, String>>>,
    /// When set, the status reports whether a model is downloaded only for models whose paths it passes.
    model_filter: Option<ModelFileFilter>,
}

impl Captions {
    /// Live captions that write what they hear to `board`.
    pub(crate) fn new(board: Arc<CaptionBoard>) -> Self {
        Captions {
            board,
            models_dir: default_models_dir(),
            loader: speech_loader(),
            own_loader: false,
            worker: None,
            download: None,
            watch: SidecarWatch::default(),
            pending: Vec::new(),
            cues: VecDeque::new(),
            language: String::new(),
            failure: None,
            writes: Vec::new(),
            model_filter: None,
        }
    }
}

impl Captions {
    fn running(&self) -> bool {
        self.worker.as_ref().is_some_and(|w| matches!(w.shared.state(), CaptionState::Loading | CaptionState::Running))
    }
}

impl Studio {
    /// The folder speech models are kept in.
    pub fn models_dir(&self) -> &Path {
        &self.captions.models_dir
    }

    /// The folder and the files of speech model `model`, or of the model `captions.start` and
    /// `captions.download` pick when none is named. Empty when there is no such model.
    pub fn speech_model_paths(&self, model: Option<&str>) -> Vec<PathBuf> {
        let id = model.map(str::to_string).or_else(|| self.caption_source_settings().0).unwrap_or_else(|| models::DEFAULT_MODEL.to_string());
        models::find(&id).map(|info| model_paths(&self.captions.models_dir, info)).unwrap_or_default()
    }

    /// Look at a model's files for the status only when `filter` passes the models folder and
    /// every path of that model; otherwise the status reports `downloaded` as null. `None` looks
    /// at every model.
    pub fn set_model_file_filter(&mut self, filter: Option<ModelFileFilter>) {
        self.captions.model_filter = filter;
    }

    /// Keep speech models in `dir` from now on.
    pub fn set_models_dir(&mut self, dir: PathBuf) {
        self.captions.models_dir = dir;
    }

    /// Load speech recognizers with `loader`, which checks its own files (tests give a recognizer
    /// that needs no model).
    pub fn set_caption_loader(&mut self, loader: Loader) {
        self.captions.loader = loader;
        self.captions.own_loader = true;
    }

    /// Where live captions are.
    pub fn captions_state(&self) -> CaptionState {
        match (&self.captions.worker, &self.captions.failure) {
            (Some(w), _) => w.shared.state(),
            (None, Some(why)) => CaptionState::Failed(why.clone()),
            (None, None) => CaptionState::Stopped,
        }
    }

    /// The board the Live Captions sources draw from.
    pub fn caption_board(&self) -> Arc<CaptionBoard> {
        Arc::clone(&self.captions.board)
    }

    /// The model and language of the first Live Captions source in the collection.
    fn caption_source_settings(&self) -> (Option<String>, Option<String>) {
        let source = self.engine.collection().sources.iter().find(|s| s.kind == CAPTIONS_ID);
        let get = |key: &str| source.and_then(|s| s.settings.str(key)).map(str::to_string).filter(|v| !v.trim().is_empty());
        (get("model"), get("language"))
    }

    pub(crate) fn start_captions(&mut self, model: Option<String>, language: Option<String>) -> Result<Value, StudioError> {
        if let Some(w) = &self.captions.worker {
            let ending = w.shared.ending.load(Ordering::Relaxed);
            return Err(StudioError::Output(if ending { "live captions are still stopping" } else { "live captions are already running" }.into()));
        }
        let (source_model, source_language) = self.caption_source_settings();
        let model = model.or(source_model).unwrap_or_else(|| models::DEFAULT_MODEL.to_string());
        let language = language.or(source_language).unwrap_or_else(|| "auto".into());
        let info = models::find(&model).ok_or_else(|| StudioError::Output(format!("there is no speech model `{model}`")))?;
        if !valid_language(&language) {
            return Err(StudioError::Output(format!(
                "`{language}` is not a language the speech models recognize; use a code such as `en`, or `auto`"
            )));
        }
        if !self.captions.own_loader && !models::present(&self.captions.models_dir, info) {
            return Err(StudioError::Output(format!(
                "the speech model `{model}` is not downloaded; use Tools › Download Speech Model or captions.download first"
            )));
        }
        self.captions.failure = None;
        let shared = Arc::new(Shared {
            state: Mutex::new(CaptionState::Loading),
            buffer: Mutex::new(Buffer::default()),
            base: Mutex::new(None),
            final_until: AtomicI64::new(i64::MIN),
            cues: Mutex::new(VecDeque::new()),
            language: Mutex::new(language.clone()),
            ending: AtomicBool::new(false),
            read_all: AtomicBool::new(false),
            cancel: Arc::new(AtomicBool::new(false)),
            gaps: AtomicU64::new(0),
        });
        let queue = Arc::new(Fifo::new(QUEUE_BLOCKS));
        let job = RecognizeJob {
            loader: Arc::clone(&self.captions.loader),
            models_dir: self.captions.models_dir.clone(),
            model: model.clone(),
            language: (language != "auto").then(|| language.clone()),
            board: Arc::clone(&self.captions.board),
            clock: self.clock,
        };
        let spawn = |name: &str, f: Box<dyn FnOnce() + Send>| std::thread::Builder::new().name(name.into()).spawn(f);
        let reader = {
            let (q, s) = (Arc::clone(&queue), Arc::clone(&shared));
            spawn("captions-reader", Box::new(move || read_loop(q, s)))
        }
        .map_err(|e| StudioError::Output(format!("the captions thread could not start: {e}")))?;
        let s = Arc::clone(&shared);
        let recognizer = match spawn("captions-recognizer", Box::new(move || recognize_loop(job, s))) {
            Ok(h) => h,
            Err(e) => {
                shared.ending.store(true, Ordering::Relaxed);
                let _ = reader.join();
                return Err(StudioError::Output(format!("the captions thread could not start: {e}")));
            }
        };
        self.mixer.send(MixerCmd::AddOutput { id: CAPTIONS_OUTPUT, queue });
        self.captions.board.clear();
        self.captions.worker = Some(Worker { shared, model: model.clone(), handles: vec![reader, recognizer], reported: false });
        self.notices.push(Notice::new("Live captions started with {model}", vec![("model", info.name.to_string())], false));
        self.sync_captions();
        Ok(json!({"model": model, "language": language}))
    }

    pub(crate) fn stop_captions(&mut self) {
        let Some(w) = &self.captions.worker else { return };
        if !w.shared.ending.swap(true, Ordering::Relaxed) {
            self.mixer.send(MixerCmd::RemoveOutput { id: CAPTIONS_OUTPUT });
            if matches!(w.shared.state(), CaptionState::Loading | CaptionState::Running) {
                w.shared.set_state(CaptionState::Stopping);
            }
        }
        self.sync_captions();
    }

    pub(crate) fn toggle_captions(&mut self) -> Result<Value, StudioError> {
        let ending = self.captions.worker.as_ref().is_some_and(|w| w.shared.ending.load(Ordering::Relaxed));
        if self.captions.worker.is_some() && !ending {
            self.stop_captions();
            Ok(json!({"running": false}))
        } else if ending {
            Err(StudioError::Output("live captions are still stopping".into()))
        } else {
            self.start_captions(None, None)
        }
    }

    /// Download `model`, or the model of the first Live Captions source, or the default one.
    pub(crate) fn download_speech_model(&mut self, model: Option<String>) -> Result<Value, StudioError> {
        let model = model.or_else(|| self.caption_source_settings().0).unwrap_or_else(|| models::DEFAULT_MODEL.to_string());
        let model = model.as_str();
        let info = models::find(model).ok_or_else(|| StudioError::Output(format!("there is no speech model `{model}`")))?;
        if let Some(d) = &self.captions.download {
            return Err(StudioError::Output(format!("the speech model `{}` is already being downloaded", d.model)));
        }
        // The worker checks the files already there, so a large model is not hashed on this thread.
        let (cancel, done, total) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(info.size())));
        let (c, d, t, dir) = (Arc::clone(&cancel), Arc::clone(&done), Arc::clone(&total), self.captions.models_dir.clone());
        let handle = std::thread::Builder::new()
            .name("speech-model-download".into())
            .spawn(move || {
                models::download(&dir, info, &mut |got, all, _| {
                    d.store(got, Ordering::Relaxed);
                    t.store(all, Ordering::Relaxed);
                    !c.load(Ordering::Relaxed)
                })
            })
            .map_err(|e| StudioError::Output(format!("the download could not start: {e}")))?;
        self.captions.download = Some(Download { model: model.to_string(), cancel, done, total, handle });
        Ok(json!({"model": model, "downloading": true, "bytes": info.size()}))
    }

    pub(crate) fn cancel_speech_model_download(&mut self) {
        if let Some(d) = &self.captions.download {
            d.cancel.store(true, Ordering::Relaxed);
        }
    }

    /// Follow the recording for caption files, end finished runs and downloads, and write the
    /// caption files that are ready. Runs each tick and after each runtime command.
    pub(crate) fn sync_captions(&mut self) {
        let now = self.clock.now();
        if let Some(d) = self.captions.download.take_if(|d| d.handle.is_finished()) {
            let result = d.handle.join().unwrap_or_else(|_| Err(SpeechError::Download("the download stopped unexpectedly".into())));
            let name = models::find(&d.model).map_or(d.model.clone(), |m| m.name.to_string());
            self.notices.push(match result {
                Ok(()) => Notice::new("Speech model {model} downloaded", vec![("model", name)], false),
                Err(SpeechError::Canceled) => Notice::new("The download of speech model {model} was canceled", vec![("model", name)], false),
                Err(e) => Notice::new("Speech model {model} could not be downloaded: {error}", vec![("model", name), ("error", e.to_string())], true),
            });
        }
        let start = self.outputs.start_of(OutputKind::Recording);
        let running = self.captions.running();
        let outputs = &self.outputs;
        let target = || outputs.state(OutputKind::Recording).map(|s| PathBuf::from(s.target));
        if let Some(s) = self.captions.watch.observe(start, target, running, now) {
            self.captions.pending.push(s);
        }
        // Report a run that ended by itself, and forget a run whose threads have ended.
        if let Some(w) = &mut self.captions.worker
            && !w.reported
            && let CaptionState::Failed(e) = w.shared.state()
        {
            w.reported = true;
            self.mixer.send(MixerCmd::RemoveOutput { id: CAPTIONS_OUTPUT });
            self.notices.push(Notice::new("Live captions stopped: {error}", vec![("error", e)], true));
        }
        // Read `finished` before the cues, so a run that ended has given every cue it made.
        let finished = self.captions.worker.as_ref().is_some_and(Worker::finished);
        let final_until = self.captions.worker.as_ref().map(|w| (w.shared.final_until.load(Ordering::Acquire), finished));
        let c = &mut self.captions;
        if let Some(w) = &c.worker {
            c.cues.extend(w.shared.cues.lock().unwrap_or_else(|e| e.into_inner()).drain(..));
            while c.cues.len() > MAX_CUES {
                c.cues.pop_front();
            }
            c.language = w.shared.language.lock().unwrap_or_else(|e| e.into_inner()).clone();
        }
        let mut still = Vec::new();
        for s in std::mem::take(&mut self.captions.pending) {
            let ready = match final_until {
                None => true,
                Some((until, finished)) => {
                    finished || until >= s.end.0 || now.0.saturating_sub(s.end.0) >= Tick::from_units(SIDECAR_WAIT.as_secs() as i64, 1).0
                }
            };
            if !ready {
                still.push(s);
                continue;
            }
            let doc = sidecar_document(self.captions.cues.make_contiguous(), s.start, s.end, &self.captions.language);
            if doc.cues.is_empty() {
                continue;
            }
            // The files are written on their own thread, so a slow disk does not hold up a frame.
            let recording = s.recording.clone();
            match std::thread::Builder::new().name("captions-files".into()).spawn(move || write_sidecars(&recording, &doc)) {
                Ok(h) => self.captions.writes.push(h),
                Err(e) => self.notices.push(Notice::new("The captions could not be saved: {error}", vec![("error", e.to_string())], true)),
            }
        }
        self.captions.pending = still;
        self.report_caption_files(false);
        if finished && let Some(mut w) = self.captions.worker.take() {
            w.join();
            match w.shared.state() {
                CaptionState::Stopped if !w.reported => self.notices.push(Notice::new("Live captions stopped", Vec::new(), false)),
                CaptionState::Failed(why) => self.captions.failure = Some(why),
                _ => {}
            }
            self.mixer.send(MixerCmd::RemoveOutput { id: CAPTIONS_OUTPUT });
        }
        // Cues no recording can still need are dropped.
        let c = &mut self.captions;
        match c.pending.iter().map(|s| s.start.0).chain(c.watch.start().map(|t| t.0)).min() {
            Some(t) => c.cues.retain(|cue| cue.end.0 > t),
            None => c.cues.clear(),
        }
    }

    /// Report the caption files that have been written: those finished, or with `wait` all of them.
    fn report_caption_files(&mut self, wait: bool) {
        let (done, running): (Vec<_>, Vec<_>) = std::mem::take(&mut self.captions.writes).into_iter().partition(|h| wait || h.is_finished());
        self.captions.writes = running;
        for h in done {
            match h.join().unwrap_or_else(|_| Err("the caption files could not be written".into())) {
                Ok(paths) => {
                    let first = paths.first().map(|p| p.display().to_string()).unwrap_or_default();
                    self.notices.push(Notice::new("Captions saved to {path}", vec![("path", first)], false));
                }
                Err(e) => self.notices.push(Notice::new("The captions could not be saved: {error}", vec![("error", e)], true)),
            }
        }
    }

    /// Stop captions and downloads (the studio is closing). The recognizer gets up to
    /// [`CLOSE_WAIT`] to finish the audio it holds, so a recording that just stopped keeps its last
    /// phrases; after that it is told to stop and gets [`CANCEL_WAIT`] more, and then it is no
    /// longer waited for.
    pub(crate) fn finish_captions_blocking(&mut self) {
        self.stop_captions();
        if let Some(d) = &self.captions.download {
            d.cancel.store(true, Ordering::Relaxed);
        }
        if let Some(w) = &mut self.captions.worker {
            let wait = |w: &Worker, d: Duration| {
                let deadline = std::time::Instant::now() + d;
                while !w.finished() && std::time::Instant::now() < deadline {
                    std::thread::sleep(IDLE);
                }
            };
            wait(w, CLOSE_WAIT);
            w.shared.cancel.store(true, Ordering::Relaxed);
            wait(w, CANCEL_WAIT);
            if w.finished() {
                w.join();
            } else {
                // A model still loading does not see the stop; its threads are left to end with
                // the process, and the phrases it finished go to the caption files now.
                w.handles.clear();
                let c = &mut self.captions;
                if let Some(w) = c.worker.take() {
                    c.cues.extend(w.shared.cues.lock().unwrap_or_else(|e| e.into_inner()).drain(..));
                    c.language = w.shared.language.lock().unwrap_or_else(|e| e.into_inner()).clone();
                    self.mixer.send(MixerCmd::RemoveOutput { id: CAPTIONS_OUTPUT });
                }
            }
        }
        // A download waiting on the network is not waited for; it stops at its next read, and the
        // next download of that model starts its part file again.
        self.captions.download = None;
        self.sync_captions();
        // The caption files of a recording that just stopped are written before the studio goes.
        self.report_caption_files(true);
    }

    /// What `captions.status` and the state JSON report.
    pub(crate) fn captions_json(&self) -> Value {
        let c = &self.captions;
        let dir = &c.models_dir;
        let shown = |m: &models::ModelInfo| c.model_filter.as_ref().is_none_or(|pass| pass(dir) && model_paths(dir, m).iter().all(|p| pass(p)));
        let model_rows: Vec<Value> = models::catalog()
            .iter()
            .map(|m| {
                let downloaded = if shown(m) { Value::Bool(models::present(dir, m)) } else { Value::Null };
                json!({"id": m.id, "name": m.name, "bytes": m.size(), "downloaded": downloaded, "license": m.license, "source": m.source})
            })
            .collect();
        let (state, model, language, gaps) = match &c.worker {
            Some(w) => (
                w.shared.state().text(),
                Some(w.model.clone()),
                Some(w.shared.language.lock().unwrap_or_else(|e| e.into_inner()).clone()),
                w.shared.gaps.load(Ordering::Relaxed),
            ),
            None => (self.captions_state().text(), None, None, 0),
        };
        let cues = c.cues.len();
        let download =
            c.download.as_ref().map(|d| json!({"model": d.model, "done": d.done.load(Ordering::Relaxed), "total": d.total.load(Ordering::Relaxed)}));
        json!({
            "state": state,
            "model": model,
            "language": language,
            "available": craft_speech::available(),
            "models_dir": dir.display().to_string(),
            "models": model_rows,
            "download": download,
            "cues": cues,
            "gaps": gaps,
            "pending_files": c.pending.len(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use craft_speech::{ProgressFn, Transcript, Word};

    fn t(ms: i64) -> Tick {
        Tick::from_units(ms, 1000)
    }

    fn cue(from: i64, to: i64, text: &str) -> craft_captions::Cue {
        craft_captions::Cue { start: t(from), end: t(to), text: text.into(), ..Default::default() }
    }

    #[test]
    fn a_recording_gets_files_only_when_captions_ran_during_it() {
        let mut w = SidecarWatch::default();
        let calls = std::cell::Cell::new(0);
        let see = |w: &mut SidecarWatch, rec: Option<(i64, &str)>, running: bool, now: i64| {
            w.observe(
                rec.map(|r| t(r.0)),
                || {
                    calls.set(calls.get() + 1);
                    rec.map(|r| PathBuf::from(r.1))
                },
                running,
                t(now),
            )
        };
        let a = |ms: i64| Some((ms, "a.mkv"));
        assert_eq!(see(&mut w, a(1000), false, 1000), None);
        assert_eq!(see(&mut w, a(1000), false, 2000), None);
        assert_eq!(see(&mut w, None, false, 3000), None, "captions never ran");
        assert_eq!(see(&mut w, a(4000), false, 4000), None);
        assert_eq!(see(&mut w, a(4000), true, 5000), None);
        assert_eq!(see(&mut w, a(4000), false, 6000), None);
        assert_eq!(see(&mut w, None, false, 7000), Some(Sidecar { recording: "a.mkv".into(), start: t(4000), end: t(7000) }));
        // A new recording right after the last one is watched on its own.
        assert_eq!(see(&mut w, a(8000), true, 8000), None);
        assert_eq!(see(&mut w, Some((9000, "b.mkv")), true, 9000), Some(Sidecar { recording: "a.mkv".into(), start: t(8000), end: t(9000) }));
        assert_eq!(see(&mut w, None, false, 9500).map(|s| s.recording), Some(PathBuf::from("b.mkv")));
        assert_eq!(calls.get(), 4, "the file is asked for once a recording");
    }

    #[test]
    fn caption_files_hold_the_recording_s_cues_counted_from_its_start() {
        let cues = [
            cue(0, 900, "before"),
            cue(500, 1500, "across the start"),
            cue(2000, 3000, "inside"),
            cue(4500, 6000, "across the end"),
            cue(7000, 8000, "after"),
        ];
        let doc = sidecar_document(&cues, t(1000), t(5000), "en");
        let got: Vec<(i64, i64, &str)> = doc.cues.iter().map(|c| (c.start.0, c.end.0, c.text.as_str())).collect();
        assert_eq!(got, [(0, t(500).0, "across the start"), (t(1000).0, t(2000).0, "inside"), (t(3500).0, t(4000).0, "across the end")]);
        assert_eq!(doc.language.as_deref(), Some("en"));
        assert_eq!(sidecar_document(&cues, t(1000), t(5000), "auto").language, None);
        assert!(sidecar_document(&[], t(0), t(10), "en").cues.is_empty());

        let dir = tempfile::tempdir().unwrap();
        let rec = dir.path().join("2026-11-20 10-00-00.mkv");
        let paths = write_sidecars(&rec, &doc).unwrap();
        assert_eq!(paths.len(), 3);
        let srt = std::fs::read_to_string(rec.with_extension("srt")).unwrap();
        assert!(srt.starts_with("1\n00:00:00,000 --> 00:00:00,500\nacross the start"), "{srt}");
        let vtt = std::fs::read_to_string(rec.with_extension("vtt")).unwrap();
        assert!(vtt.starts_with("WEBVTT") && vtt.contains("00:00:01.000 --> 00:00:02.000\ninside"), "{vtt}");
        assert_eq!(std::fs::read_to_string(rec.with_extension("txt")).unwrap(), "across the start\ninside\nacross the end\n");
        // Files that exist are not replaced.
        std::fs::write(rec.with_extension("srt"), "mine").unwrap();
        assert!(write_sidecars(&rec, &doc).is_err());
        assert_eq!(std::fs::read_to_string(rec.with_extension("srt")).unwrap(), "mine");
    }

    fn shared() -> Arc<Shared> {
        Arc::new(Shared {
            state: Mutex::new(CaptionState::Running),
            buffer: Mutex::new(Buffer::default()),
            base: Mutex::new(None),
            final_until: AtomicI64::new(i64::MIN),
            cues: Mutex::new(VecDeque::new()),
            language: Mutex::new("en".into()),
            ending: AtomicBool::new(false),
            read_all: AtomicBool::new(false),
            cancel: Arc::new(AtomicBool::new(false)),
            gaps: AtomicU64::new(0),
        })
    }

    fn block(pts: Tick, level: f32) -> AudioBlock {
        AudioBlock { samples: vec![level; BLOCK_FRAMES * CHANNELS], pts }
    }

    #[test]
    fn the_reader_waits_for_the_mixer_to_let_go_after_a_stop() {
        let short = Duration::from_millis(1);
        assert!(!reader_done(false, false, true, Duration::ZERO));
        assert!(!reader_done(false, false, false, DRAIN_WAIT), "captions still run");
        assert!(!reader_done(false, true, true, short), "the mixer can still add a block");
        assert!(reader_done(false, true, false, short));
        assert!(reader_done(false, true, true, DRAIN_WAIT), "a mixer that never lets go is not waited for");
        assert!(reader_done(true, false, true, Duration::ZERO));
    }

    #[test]
    fn the_reader_keeps_time_across_missing_blocks() {
        let s = shared();
        let mut r = Reader::new();
        let step = Tick::from_units(BLOCK_FRAMES as i64, i64::from(SAMPLE_RATE));
        let start = t(10_000);
        r.block(&block(start, 0.1), &s);
        r.block(&block(start.saturating_add(step), 0.1), &s);
        // Two blocks are missing.
        r.block(&block(start.saturating_add(Tick(step.0 * 4)), 0.1), &s);
        // A repeat of an earlier block is skipped.
        r.block(&block(start, 0.1), &s);
        let chunks = s.buffer.lock().unwrap().take();
        let audio: u64 = chunks.iter().map(|c| if let Chunk::Audio(a) = c { a.len() as u64 } else { 0 }).sum();
        let gaps: Vec<u64> = chunks.iter().filter_map(|c| if let Chunk::Gap(n) = c { Some(*n) } else { None }).collect();
        // 1024 frames at 48 kHz are 341.33 samples at 16 kHz.
        assert_eq!(audio, 3 * 1024 / 3);
        assert_eq!(gaps, [682]);
        assert_eq!(*s.base.lock().unwrap(), Some(start));
        assert_eq!(s.gaps.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_full_buffer_turns_its_oldest_audio_into_a_gap() {
        let mut b = Buffer::default();
        let second = craft_speech::SAMPLE_RATE as usize;
        for _ in 0..BUFFER_SECONDS + 5 {
            b.push(Chunk::Audio(vec![0.0; second]));
        }
        assert_eq!(b.samples, BUFFER_SECONDS * second as u64);
        // Gaps that meet are joined, so a buffer that keeps falling behind holds one gap.
        for _ in 0..1000 {
            b.push(Chunk::Gap(1));
            b.push(Chunk::Audio(vec![0.0; second]));
        }
        assert_eq!(b.samples, BUFFER_SECONDS * second as u64);
        let chunks = b.take();
        assert!(chunks.len() as u64 <= 2 * BUFFER_SECONDS + 1, "{} chunks", chunks.len());
        // The 59 one-sample gaps between the 60 seconds kept stay where they are.
        assert_eq!(chunks.first(), Some(&Chunk::Gap(1005 * second as u64 + 1000 - 59)), "every second keeps its place");
        let held: u64 = chunks
            .iter()
            .map(|c| match c {
                Chunk::Audio(a) => a.len() as u64,
                Chunk::Gap(n) => *n,
            })
            .sum();
        assert_eq!(held, 1065 * second as u64 + 1000, "the buffer still spans all the time pushed");
        assert_eq!(b.samples, 0);
    }

    /// "hello" for every phrase.
    struct Hello;
    impl Transcriber for Hello {
        fn id(&self) -> String {
            "hello".into()
        }
        fn transcribe(&self, audio: &[f32], _: &Options, progress: ProgressFn) -> Result<Transcript, SpeechError> {
            if !progress(0.0, "") {
                return Err(SpeechError::Canceled);
            }
            Ok(Transcript { words: vec![Word::new("hello", 0, audio.len() as u64)], ..Default::default() })
        }
    }

    #[test]
    fn phrases_become_cues_on_the_studio_clock_and_lines_on_the_board() {
        let s = shared();
        let board = Arc::new(CaptionBoard::new());
        // 1 s of quiet, 1.5 s of tone, 2 s of quiet, from 10 s on the studio clock.
        let mut audio = vec![0.0005f32; 16_000];
        audio.extend((0..24_000).map(|i| 0.3 * (i as f32 * 0.2).sin()));
        audio.extend(vec![0.0005f32; 32_000]);
        s.buffer.lock().unwrap().push(Chunk::Audio(audio));
        *s.base.lock().unwrap() = Some(t(10_000));
        s.read_all.store(true, Ordering::Release);
        let job = RecognizeJob {
            loader: Arc::new(|_: &Path, _: &str| Ok(Arc::new(Hello) as Arc<dyn Transcriber>)),
            models_dir: PathBuf::new(),
            model: "test".into(),
            language: None,
            board: Arc::clone(&board),
            clock: studiocraft_media::clock::MasterClock::new(),
        };
        recognize_loop(job, Arc::clone(&s));
        assert_eq!(s.state(), CaptionState::Stopped);
        let cues: Vec<_> = s.cues.lock().unwrap().iter().cloned().collect();
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "hello");
        // The voice runs from 11 s to 12.5 s on the studio clock.
        assert!(cues[0].start.0 >= t(10_600).0 && cues[0].start.0 <= t(11_000).0, "{:?}", cues[0].start);
        assert!(cues[0].end.0 >= t(12_400).0 && cues[0].end.0 <= t(13_000).0, "{:?}", cues[0].end);
        assert_eq!(board.snapshot().lines.iter().map(|l| l.0.as_str()).collect::<Vec<_>>(), ["hello"]);
        assert_eq!(s.final_until.load(Ordering::Acquire), i64::MAX);
    }

    /// Answers once, then fails.
    struct FailsSecond(std::sync::atomic::AtomicUsize);
    impl Transcriber for FailsSecond {
        fn id(&self) -> String {
            "fails".into()
        }
        fn transcribe(&self, audio: &[f32], _: &Options, _: ProgressFn) -> Result<Transcript, SpeechError> {
            if self.0.fetch_add(1, Ordering::Relaxed) == 0 {
                Ok(Transcript { words: vec![Word::new("hel", 0, audio.len() as u64)], ..Default::default() })
            } else {
                Err(SpeechError::Model("the decoder failed".into()))
            }
        }
    }

    #[test]
    fn a_failed_transcription_takes_its_partial_off_the_board() {
        let s = shared();
        let board = Arc::new(CaptionBoard::new());
        let mut audio = vec![0.0005f32; 16_000];
        audio.extend((0..64_000).map(|i| 0.3 * (i as f32 * 0.2).sin()));
        // Pieces of 0.1 s, as the reader hands them over, so the first answer is shown before the
        // second fails.
        for piece in audio.chunks(1600) {
            s.buffer.lock().unwrap().push(Chunk::Audio(piece.to_vec()));
        }
        *s.base.lock().unwrap() = Some(t(0));
        s.read_all.store(true, Ordering::Release);
        let job = RecognizeJob {
            loader: Arc::new(|_: &Path, _: &str| Ok(Arc::new(FailsSecond(Default::default())) as Arc<dyn Transcriber>)),
            models_dir: PathBuf::new(),
            model: "test".into(),
            language: None,
            board: Arc::clone(&board),
            clock: studiocraft_media::clock::MasterClock::new(),
        };
        recognize_loop(job, Arc::clone(&s));
        assert!(matches!(s.state(), CaptionState::Failed(e) if e.contains("decoder")));
        let snap = board.snapshot();
        assert!(snap.generation >= 2, "the first answer was shown, then taken off");
        assert!(snap.partial.is_none() && snap.lines.is_empty(), "{snap:?}");
    }

    #[test]
    fn a_model_that_cannot_load_fails_the_run() {
        let s = shared();
        *s.state.lock().unwrap() = CaptionState::Loading;
        let job = RecognizeJob {
            loader: Arc::new(|_: &Path, id: &str| Err(SpeechError::NotInstalled(id.into()))),
            models_dir: PathBuf::new(),
            model: "whisper-tiny".into(),
            language: None,
            board: Arc::new(CaptionBoard::new()),
            clock: studiocraft_media::clock::MasterClock::new(),
        };
        recognize_loop(job, Arc::clone(&s));
        assert!(matches!(s.state(), CaptionState::Failed(e) if e.contains("whisper-tiny")));
        assert!(s.ending.load(Ordering::Relaxed));
    }

    #[test]
    fn language_codes_are_checked() {
        for good in ["auto", "en", "zh", "haw"] {
            assert!(valid_language(good), "{good}");
        }
        for bad in ["", "EN", "english", "e", "e1", "../", "zz", "xyz"] {
            assert!(!valid_language(bad), "{bad}");
        }
    }
}
