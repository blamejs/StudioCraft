//! The running studio.
//!
//! [`Studio`] owns the [`Engine`] (the scene collection and its undo history) and everything that
//! runs from it: one thread per source, the audio mixer, the compositor's frame thread, the
//! scene transition in progress, the recording and stream, and live captions. The interface,
//! hotkeys, scripts and the control channel all go through [`Studio::execute`]; after each change
//! the studio brings the running parts in line with the collection.

pub mod audio_editor;
mod audio_tracks;
mod captions;
pub mod capture;
mod events;
pub mod files;
mod golive;
mod monitor;
mod outputs;
mod pause;
mod recording_markers;
mod recordings;
mod wiring;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{RecvTimeoutError, SyncSender, sync_channel};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use studiocraft_engine::command::MAX_SOURCE_SIDE;
use studiocraft_engine::host::SourceSet;
use studiocraft_engine::mixer::{LoudnessReading, Mixer, MixerCmd};
use studiocraft_engine::scheduler::Scheduler;
use studiocraft_engine::{CanvasSize, Command, Engine, EngineError, PauseValues, RuntimeAction, SourceSize};
use studiocraft_media::clock::{MasterClock, duration_of, tick_of};
use studiocraft_media::mix::{BlockLevels, Levels};
use studiocraft_media::queue::Fifo;
use studiocraft_media::stats::{EncoderStats, RenderStats, Snapshot};
use studiocraft_media::video::PooledFrame;
use studiocraft_media::{FrameRate, Tick};
use studiocraft_model::{FilterId, FitMode, ItemId, LoudnessTarget, Profile, SceneCollection, SceneId, Settings, SourceId, Transform, VideoSettings};
use studiocraft_plugin::{PluginError, Registry};
use studiocraft_render::device::{Recovery, RecoveryPolicy};
use studiocraft_render::{CanvasViews, CaptureRequest, Compositor, Gpu, RenderShared, ThumbnailRequest, ThumbnailViews};
use studiocraft_sources::FfmpegSetting;

pub use audio_tracks::{PROGRAM_NAME, clock_text};
pub use captions::{CaptionState, Loader as CaptionLoader, ModelFileFilter, default_models_dir};
pub use files::FileError;
pub use outputs::{HARDWARE_TESTS_FILE, OutputKind, OutputState, recording_folder, recording_path, replay_path};
pub use pause::{countdown_text, media_extensions as pause_media_extensions, paused_text};
pub use studiocraft_audio_tracks::{State as AudioTracksState, Status as AudioTracksStatus, StopReason as AudioTracksStopReason};
pub use studiocraft_outputs::stream::diagnose;
pub use studiocraft_outputs::stream::ingests;
pub use studiocraft_platform::PlatformError;
pub use studiocraft_platform::audio::{AudioSink, Fill, Format, OnError, Playing, SystemSink};
pub use studiocraft_render::{Capture, View};
pub use studiocraft_sources::CaptionBoard;

/// Why something the user asked for did not happen. Shown to the user as is.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum StudioError {
    #[error(transparent)]
    Engine(#[from] EngineError),
    #[error(transparent)]
    Plugin(#[from] PluginError),
    #[error("{0}")]
    Output(String),
    #[error("the graphics device is not available: {0}")]
    Gpu(String),
    #[error("nothing is drawn: the studio runs without a graphics device")]
    NoGpu,
    #[error("the preview is shown only in studio mode")]
    NoPreview,
    #[error("a capture is already waiting")]
    CaptureBusy,
    #[error("no picture arrived within {} ms", .0.as_millis())]
    CaptureTimeout(Duration),
    #[error("the picture could not be encoded: {0}")]
    Image(String),
}

/// What the interface does for a command the studio itself does not carry out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiAction {
    OpenSettings,
    Exit,
}

/// A message for the status bar or a notification: an English template with `{name}`
/// placeholders and their values. The interface translates the template when it shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    pub template: &'static str,
    pub args: Vec<(&'static str, String)>,
    pub error: bool,
}

impl Notice {
    pub fn new(template: &'static str, args: Vec<(&'static str, String)>, error: bool) -> Notice {
        Notice { template, args, error }
    }

    /// A message with no template of its own, such as an error text from another crate.
    pub fn plain(text: impl Into<String>, error: bool) -> Notice {
        Notice::new("{text}", vec![("text", text.into())], error)
    }

    /// The message in English, with the values filled in.
    pub fn text(&self) -> String {
        let mut out = String::with_capacity(self.template.len());
        let mut rest = self.template;
        while let Some(open) = rest.find('{') {
            out.push_str(&rest[..open]);
            let after = &rest[open + 1..];
            let value = after.find('}').and_then(|close| Some((close, self.args.iter().find(|(k, _)| *k == &after[..close])?)));
            match value {
                Some((close, (_, v))) => {
                    out.push_str(v);
                    rest = &after[close + 1..];
                }
                None => {
                    out.push('{');
                    rest = after;
                }
            }
        }
        out.push_str(rest);
        out
    }
}

/// What to tell the user about the graphics adapter choice, as a notice the interface translates.
pub fn gpu_notice(n: &studiocraft_render::device::Notice) -> Notice {
    use studiocraft_render::device::Notice as N;
    match n {
        N::AdapterMissing(key) => {
            Notice::new("The graphics adapter {adapter} is not available; using the automatic choice", vec![("adapter", key.name.clone())], true)
        }
        N::NoSoftwareAdapter => Notice::new("No software graphics adapter is available; using the automatic choice", vec![], true),
        N::SoftwareOnly => Notice::new("No graphics hardware can be used; drawing in software, which is slower", vec![], true),
        N::DeviceFailed { adapter, error } => Notice::new(
            "The graphics adapter {adapter} could not be opened: {error}",
            vec![("adapter", adapter.clone()), ("error", error.clone())],
            true,
        ),
        N::SoftwareAfterLosses(count) => {
            Notice::new("The graphics device stopped working {count} times; drawing in software", vec![("count", count.to_string())], true)
        }
        other => Notice::plain(other.to_string(), true),
    }
}

/// Every plug-in StudioCraft ships with. The Live Captions sources draw the text on `captions`;
/// give the studio that uses the registry the same board.
pub fn builtin_registry(ffmpeg: &FfmpegSetting, captions: &Arc<CaptionBoard>) -> Result<Registry, PluginError> {
    use studiocraft_render::transitions::{Cut, Fade, FadeThroughColor, Swipe};
    let mut reg = Registry::new();
    studiocraft_sources::register_all(&mut reg, ffmpeg, captions)?;
    reg.register_transition(Arc::new(Fade))?;
    reg.register_transition(Arc::new(Cut))?;
    reg.register_transition(Arc::new(FadeThroughColor))?;
    reg.register_transition(Arc::new(Swipe))?;
    studiocraft_outputs::register_all(&mut reg)?;
    Ok(reg)
}

/// A new scene collection: one scene, and the default audio output and input as global audio
/// sources (heard in every scene).
pub fn default_collection(name: &str) -> SceneCollection {
    use studiocraft_model::{AudioState, Settings, SourceConfig};
    let mut c = SceneCollection::new(name, "Scene");
    for (kind, label) in [("audio_output", "Desktop audio"), ("audio_input", "Microphone")] {
        let id = SourceId(c.alloc_id());
        let settings = Settings::new().with("device", studiocraft_platform::audio::DEFAULT_DEVICE);
        c.sources.push(SourceConfig {
            id,
            kind: kind.into(),
            kind_version: 1,
            name: label.into(),
            settings,
            filters: vec![],
            audio: AudioState::default(),
        });
        c.global_audio.push(id);
    }
    c
}

/// The compositor thread and its mailboxes.
struct Video {
    shared: Arc<RenderShared>,
    views: Arc<CanvasViews>,
    /// The textures the scene pictures are drawn into, once a request with scenes made them.
    thumbnail_views: Option<Arc<ThumbnailViews>>,
    /// The last scene pictures the compositor was asked for; `None` when it draws none.
    thumbnails_sent: Option<Arc<ThumbnailRequest>>,
    /// How many times the compositor was sent a thumbnail request or `None`, in tests.
    #[cfg(test)]
    thumbnail_sends: u32,
    _scheduler: Scheduler,
}

/// The scenes `scene.thumbnails` asked for, in slot order, and the pictures a second of each.
#[derive(Clone, Debug, PartialEq)]
struct Thumbnails {
    scenes: Vec<SceneId>,
    /// The compositor's key for each scene's slot: kept while the scene stays in the list, and
    /// never given out again, so a scene of another collection with the same id gets a new one.
    keys: Vec<u64>,
    fps: f32,
}

/// How often [`Studio::tick`] looks for a lost graphics device while nothing else polls it.
const GPU_PROBE: Duration = Duration::from_secs(1);
/// How long after a failed attempt the studio tries again to open a graphics device.
const GPU_RETRY: Duration = Duration::from_secs(10);

/// The graphics device was lost.
struct GpuLoss {
    /// Why, as wgpu said.
    why: String,
    /// When to try opening another device; `None` when the studio cannot (the window's device).
    retry_at: Option<Instant>,
}

/// What a transition in the program starts from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Origin {
    Scene(SceneId),
    /// The pause screen, as the program resumes.
    Pause,
}

/// What [`Studio::tick_pause`] last brought the pause screen in line with: the whole seconds the
/// countdown shows, whether it reached zero, the countdown's length, the canvas, and the sizes
/// of the media and the text.
type PauseSeen = (Option<u64>, bool, u32, (u32, u32), (Option<(u32, u32)>, Option<(u32, u32)>));

/// The transitions whose blend shows the same picture run backward, which a pause or resume turns
/// around where it stands ([`Studio::begin_pause_transition`]).
const TURNS_AROUND: [&str; 2] = ["fade", "fade_color"];

/// The progress, 0 to 1, at which an eased blend `shown` first reaches `target`, found by halving
/// the interval. A blend that never reaches it gives 1, and one that starts past it gives 0.
fn progress_showing(shown: impl Fn(f64) -> f64, target: f64) -> f64 {
    let (mut lo, mut hi) = (0.0_f64, 1.0_f64);
    for _ in 0..40 {
        let mid = (lo + hi) / 2.0;
        if shown(mid) < target {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    hi
}

/// The fade length to send the mixer for a pause transition of `ms` that has the `left` share of
/// its time to run. The mixer moves the program audio at one rate over its whole range, so the
/// length is the one that moves the audio's `distance` share of that range in the time left.
/// Audio with no distance to go keeps `ms`, which the scene sources' own fades also run at.
fn turned_fade_ms(ms: u32, distance: f64, left: f64) -> u32 {
    if distance <= 0.0 {
        return ms;
    }
    (f64::from(ms) * left.clamp(0.0, 1.0) / distance.min(1.0)).round().min(f64::from(u32::MAX)) as u32
}

/// The program audio's pause fade as last sent to the mixer.
#[derive(Clone, Copy, Debug)]
struct HoldFade {
    /// When it started.
    start: Tick,
    /// The level it started from: 1 for the whole program mix, 0 for silence.
    from: f64,
    /// Whether it fades to silence.
    down: bool,
    /// Its length for the whole range, in milliseconds.
    ms: u32,
}

impl HoldFade {
    /// The level at `now`.
    fn level(&self, now: Tick) -> f64 {
        let moved =
            if self.ms == 0 { 1.0 } else { duration_of(Tick(now.0.saturating_sub(self.start.0))).as_secs_f64() * 1000.0 / f64::from(self.ms) };
        if self.down { (self.from - moved).max(0.0) } else { (self.from + moved).min(1.0) }
    }
}

/// A scene transition running in the program.
#[derive(Clone)]
struct Running {
    from: Origin,
    start: Tick,
    duration: Tick,
    kind: String,
    settings: Settings,
}

/// An item with a fit mode whose source showed nothing at a canvas change, with its mode and
/// the transform that change gave it ([`Studio::place_fit_later`]).
struct FitLater {
    item: ItemId,
    source: SourceId,
    fit: FitMode,
    transform: Transform,
}

pub struct Studio {
    engine: Engine,
    clock: MasterClock,
    sources: SourceSet,
    mixer: Mixer,
    /// Meter levels of each audio source, kept across mixer rebuilds.
    levels: HashMap<SourceId, Arc<Levels>>,
    gpu: Option<Gpu>,
    /// When the studio last polled the device for a loss.
    gpu_probed: Instant,
    /// Set while the device is lost.
    gpu_loss: Option<GpuLoss>,
    /// What to open after a loss of a device the studio opened.
    recovery: Option<Recovery>,
    /// When the studio started, the time base of `recovery`.
    started: Instant,
    video: Option<Video>,
    render_stats: Arc<RenderStats>,
    /// The scenes drawn as small pictures for a scene list; `None` while none is shown.
    thumbnails: Option<Thumbnails>,
    /// The last key given to a scene picture's slot.
    thumbnail_key: u64,
    profile: Profile,
    ffmpeg: FfmpegSetting,
    wiring: wiring::Wiring,
    transition: Option<Running>,
    /// The transition and length `studio.transition` asked for, for the switch it causes.
    quick: Option<(String, Option<u32>)>,
    /// The program scene the compositor shows (or is transitioning to).
    shown_program: SceneId,
    seen_revision: u64,
    /// The undo step of the last layout change for a new canvas, whether the collection had
    /// unsaved changes before it, and the collection after that change or after the last
    /// placement that followed it ([`Studio::restore_profile`]).
    relaid_at: Option<(u64, bool, Box<SceneCollection>)>,
    /// The undo steps of the placements [`Studio::place_fit_later`] made after that change.
    fit_placed: Vec<u64>,
    /// Sources with an item placed by a fit mode that showed nothing at the last canvas change
    /// ([`Studio::place_fit_later`]), each with the transform that change gave it.
    fit_later: Vec<FitLater>,
    /// The undo step of the last canvas change's layout, `None` when it moved nothing.
    fit_layout: Option<u64>,
    /// The collection revision, and the count and a digest of the waiting items whose sources
    /// showed a picture with those pictures' sizes, when [`Studio::place_fit_later`] last looked.
    fit_seen: Option<(u64, usize, u64)>,
    outputs: outputs::Outputs,
    /// What the go-live check keeps between calls: how each destination did on earlier streams,
    /// and the free space query.
    golive: golive::GoLive,
    notices: Vec<Notice>,
    /// Where the scopes' copy of the program goes, and the largest size it is drawn at.
    scope_feed: Option<ScopeFeed>,
    /// The device monitored sources play on.
    monitor: monitor::Monitor,
    /// Each recorded source and the program mix written to its own file.
    audio_tracks: audio_tracks::AudioTracks,
    /// Speech recognition of the program audio, and the caption files of recordings.
    captions: captions::Captions,
    /// Markers set during the recording, and the timeline files of recordings that stopped.
    recording_markers: recording_markers::RecordingMarkers,
    /// The scan of the recording folder for `recordings.list`, and its last result.
    recordings: recordings::Recordings,
    /// The running pause, while the program is paused.
    pause: Option<pause::PauseState>,
    /// The threads that draw the pause screen: while paused, and until the transition back to
    /// the program has ended.
    pause_layers: Option<pause::Layers>,
    /// What the pause screen's text and placement were last brought in line with
    /// ([`Studio::tick_pause`]).
    pause_seen: Option<PauseSeen>,
    /// The program audio's last pause fade; `None` before the first, with the whole mix heard.
    hold_fade: Option<HoldFade>,
    /// Scene switches, stream reconnects, pauses and resumes during the session, for
    /// `session.events`.
    events: events::Events,
    /// Faults the tests put into the audio tracks' disk side.
    #[cfg(test)]
    test_faults: studiocraft_audio_tracks::Faults,
    /// Counts as a running stream for the audio tracks, in tests without a server.
    #[cfg(test)]
    test_streaming: bool,
}

/// The queue the compositor sends the scopes' NV12 copy of the program to, and the largest size
/// of that copy. The copy keeps the canvas's shape.
#[derive(Clone)]
pub struct ScopeFeed {
    pub sink: Arc<Fifo<Arc<PooledFrame>>>,
    pub max_width: u32,
    pub max_height: u32,
}

/// The largest even size with the shape of `canvas` that fits in `max_w` × `max_h` (at least 2 × 2).
pub fn scope_size(canvas: (u32, u32), max_w: u32, max_h: u32) -> (u32, u32) {
    let (cw, ch) = (u64::from(canvas.0.max(1)), u64::from(canvas.1.max(1)));
    let (mw, mh) = (u64::from(max_w.max(2)), u64::from(max_h.max(2)));
    let (w, h) = if cw * mh > ch * mw { (mw, ch * mw / cw) } else { (cw * mh / ch, mh) };
    let even = |v: u64| (v.min(u64::from(u32::MAX)) as u32 & !1).max(2);
    (even(w), even(h))
}

/// `frames` sample frames at the mixer's sample rate, in milliseconds rounded to 0.1 ms.
pub fn latency_ms(frames: usize) -> f64 {
    (frames as f64 * 1000.0 / f64::from(studiocraft_media::audio::SAMPLE_RATE) * 10.0).round() / 10.0
}

/// `{"frames", "ms"}`, a delay of `frames` sample frames as the state reports it.
fn latency_json(frames: usize) -> Value {
    json!({"frames": frames, "ms": latency_ms(frames)})
}

/// What `audio.loudness` reports: the readings of `reading`, each `null` while it is −∞ (silence,
/// or too little of the program since the start or a reset), and the band of `target`.
fn loudness_json(reading: &LoudnessReading, target: &LoudnessTarget) -> Value {
    let finite = |v: f64| if v.is_finite() { json!(v) } else { Value::Null };
    json!({
        "momentary_lufs": finite(reading.momentary_lufs),
        "short_term_lufs": finite(reading.short_term_lufs),
        "integrated_lufs": finite(reading.integrated_lufs),
        "range_lu": finite(reading.range_lu),
        "true_peak_dbtp": finite(reading.true_peak_dbtp),
        "generation": reading.generation,
        "target": {"low_lufs": target.low_lufs, "high_lufs": target.high_lufs},
    })
}

impl Studio {
    /// A studio for `collection` with `profile`. Without `gpu` nothing is composited (the
    /// command-line tool and tests); with one, the compositor starts at once. Live captions write
    /// what they hear to `captions`, the board `registry`'s Live Captions sources draw.
    pub fn new(
        gpu: Option<Gpu>,
        collection: SceneCollection,
        profile: Profile,
        registry: Arc<Registry>,
        ffmpeg: FfmpegSetting,
        captions: Arc<CaptionBoard>,
    ) -> Studio {
        let clock = MasterClock::new();
        if let Ok(mut f) = ffmpeg.write() {
            f.clone_from(&profile.output.ffmpeg_path);
        }
        let mixer = match Mixer::spawn(clock) {
            Ok(m) => m,
            Err(e) => {
                log::error!("the audio mixer thread could not start: {e}");
                // Without a thread the mixer still takes commands; nothing is heard.
                Mixer::detached()
            }
        };
        let shown_program = collection.program_scene;
        let recovery = gpu.as_ref().and_then(Gpu::preference).map(|p| Recovery::new(p.clone(), RecoveryPolicy::default()));
        let now = Instant::now();
        // The first tick looks for a loss at once.
        let probed = now.checked_sub(GPU_PROBE).unwrap_or(now);
        let mut s = Studio {
            engine: Engine::new(collection, registry),
            clock,
            sources: SourceSet::new(clock),
            mixer,
            levels: HashMap::new(),
            gpu,
            gpu_probed: probed,
            gpu_loss: None,
            recovery,
            started: now,
            video: None,
            render_stats: Arc::new(RenderStats::default()),
            thumbnails: None,
            thumbnail_key: 0,
            profile,
            ffmpeg,
            wiring: wiring::Wiring::default(),
            transition: None,
            quick: None,
            shown_program,
            seen_revision: 0,
            relaid_at: None,
            fit_placed: Vec::new(),
            fit_later: Vec::new(),
            fit_layout: None,
            fit_seen: None,
            outputs: outputs::Outputs::default(),
            golive: golive::GoLive::default(),
            notices: Vec::new(),
            scope_feed: None,
            monitor: monitor::Monitor::new(Arc::new(studiocraft_platform::audio::SystemSink)),
            audio_tracks: audio_tracks::AudioTracks::default(),
            captions: captions::Captions::new(captions),
            recording_markers: recording_markers::RecordingMarkers::default(),
            recordings: recordings::Recordings::default(),
            pause: None,
            pause_layers: None,
            pause_seen: None,
            hold_fade: None,
            events: events::Events::default(),
            #[cfg(test)]
            test_faults: studiocraft_audio_tracks::Faults::default(),
            #[cfg(test)]
            test_streaming: false,
        };
        s.restart_video();
        s.refresh();
        s.repair_audio_tracks_at_launch();
        s
    }

    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    pub fn collection(&self) -> &SceneCollection {
        self.engine.collection()
    }

    pub fn registry(&self) -> &Arc<Registry> {
        self.engine.registry()
    }

    pub fn profile(&self) -> &Profile {
        &self.profile
    }

    pub fn clock(&self) -> MasterClock {
        self.clock
    }

    /// Run a command from the interface, a hotkey, a script or the control channel.
    pub fn execute(&mut self, cmd: Command) -> Result<(Value, Option<UiAction>), StudioError> {
        if let Command::StudioTransition { kind, ms } = &cmd
            && (kind.is_some() || ms.is_some())
        {
            let kind = kind.clone().unwrap_or_else(|| self.engine.collection().transition.kind.clone());
            self.quick = Some((kind, *ms));
        }
        let transition = matches!(cmd, Command::StudioTransition { .. });
        // Finished caption work is collected first, so a status reads the current state and a
        // start or download does not meet a worker that has already ended.
        self.sync_captions();
        let outcome = self.engine.execute(cmd);
        let outcome = match outcome {
            Ok(o) => o,
            Err(e) => {
                self.quick = None;
                return Err(e.into());
            }
        };
        let mut ui = None;
        let mut result = outcome.result;
        if let Some(action) = outcome.runtime {
            match action {
                RuntimeAction::AudioLoudness => result = loudness_json(&self.master_loudness(), &self.profile.audio.loudness_target),
                RuntimeAction::SceneThumbnails { scenes, fps } => result = self.set_thumbnails(scenes, fps),
                RuntimeAction::AudioTracksStatus => result = self.audio_tracks_json(),
                RuntimeAction::AudioTracksRepair { folder } => result = self.repair_audio_tracks(&folder)?,
                RuntimeAction::AudioTracksOpenLast => result = self.open_last_audio_tracks()?,
                RuntimeAction::AudioTracksMarker { name } => result = self.add_audio_tracks_marker(name)?,
                RuntimeAction::RecordingMarker { name } => result = self.add_recording_marker(name)?,
                RuntimeAction::MarkerAdd { name } => result = self.add_marker(name)?,
                RuntimeAction::MarkerList => result = self.marker_list(),
                RuntimeAction::MarkerRename { index, name } => result = self.rename_marker(index, name)?,
                RuntimeAction::RecordingsList => result = self.list_recordings()?,
                RuntimeAction::StartRecording => self.start_output(OutputKind::Recording)?,
                RuntimeAction::StopRecording => self.stop_output(OutputKind::Recording),
                RuntimeAction::ToggleRecording => self.toggle_output(OutputKind::Recording)?,
                RuntimeAction::StartStreaming => self.start_output(OutputKind::Streaming)?,
                RuntimeAction::StopStreaming => self.stop_output(OutputKind::Streaming),
                RuntimeAction::ToggleStreaming => self.toggle_output(OutputKind::Streaming)?,
                RuntimeAction::GoLiveCheck => result = self.golive_json(),
                RuntimeAction::StartReplay => self.start_output(OutputKind::Replay)?,
                RuntimeAction::StopReplay => self.stop_output(OutputKind::Replay),
                RuntimeAction::ToggleReplay => self.toggle_output(OutputKind::Replay)?,
                RuntimeAction::SaveReplay => self.save_replay()?,
                RuntimeAction::CaptionsStart { model, language } => result = self.start_captions(model, language)?,
                RuntimeAction::CaptionsStop => self.stop_captions(),
                RuntimeAction::CaptionsToggle => result = self.toggle_captions()?,
                RuntimeAction::CaptionsStatus => result = self.captions_json(),
                RuntimeAction::CaptionsDownload { model } => result = self.download_speech_model(model)?,
                RuntimeAction::CaptionsDownloadCancel => self.cancel_speech_model_download(),
                RuntimeAction::RetestHardwareEncoders => self.retest_hardware_encoders()?,
                RuntimeAction::FilterMeter { source, filter } => result = self.filter_meter_json(source, filter),
                RuntimeAction::PauseStart(values) => result = self.start_pause(values)?,
                RuntimeAction::PauseStop => self.stop_pause()?,
                RuntimeAction::PauseToggle => {
                    if self.pause.is_some() {
                        self.stop_pause()?;
                        result = self.pause_json();
                    } else {
                        result = self.start_pause(PauseValues::default())?;
                    }
                }
                RuntimeAction::PauseExtend { seconds } => result = self.extend_pause(seconds)?,
                RuntimeAction::PauseStatus => result = self.pause_json(),
                RuntimeAction::SessionEvents { since } => result = self.events.json(since),
                RuntimeAction::OpenSettings => ui = Some(UiAction::OpenSettings),
                RuntimeAction::Exit => ui = Some(UiAction::Exit),
            }
            // A recording or stream that started or stopped starts or stops the audio tracks now.
            self.sync_audio_tracks();
            self.sync_captions();
            self.sync_recording_markers();
        }
        if outcome.changed {
            self.refresh();
            self.sync_audio_tracks();
        } else if transition {
            // A transition that changed nothing drops its override, so a later switch uses the
            // collection's transition.
            self.quick = None;
        }
        Ok((result, ui))
    }

    /// Run a command by its id and JSON parameters (hotkeys, scripts, the control channel).
    pub fn run(&mut self, id: &str, params: &Value) -> Result<(Value, Option<UiAction>), StudioError> {
        let cmd = Command::from_parts(id, params).map_err(|e| StudioError::Engine(EngineError::Invalid(e)))?;
        self.execute(cmd)
    }

    /// End the current drag or slider gesture in the undo history.
    pub fn seal_history(&mut self) {
        self.engine.seal_history();
    }

    /// Replace the scene collection (a file was opened or a new one made).
    pub fn load_collection(&mut self, collection: SceneCollection) {
        // The mixer drops the old sources' taps before it gets the new sources.
        self.audio_tracks_collection_replaced();
        self.engine.load(collection);
        self.fit_later.clear();
        self.fit_layout = None;
        self.fit_seen = None;
        self.relaid_at = None;
        self.fit_placed.clear();
        self.transition = None;
        // The new collection's scene ids can match the old one's, so pictures asked for the old
        // scenes are not carried over.
        self.thumbnails = None;
        self.shown_program = self.engine.collection().program_scene;
        self.refresh();
        self.sync_audio_tracks();
    }

    pub fn mark_saved(&mut self) {
        self.engine.mark_saved();
    }

    /// Replace the scene collection with the one in the file at `path`. The undo history starts
    /// empty. On an error the current collection stays.
    pub fn open_collection(&mut self, path: &Path) -> Result<(), FileError> {
        let collection = files::read_collection_file(path)?;
        self.load_collection(collection);
        Ok(())
    }

    /// Write the current scene collection to `path` and mark it saved.
    pub fn save_collection(&mut self, path: &Path) -> Result<(), FileError> {
        files::write_collection_file(path, self.engine.collection())?;
        self.mark_saved();
        Ok(())
    }

    /// Housekeeping once per interface frame: finished transitions, output status, new canvas
    /// textures, errors from the compositor.
    pub fn tick(&mut self) {
        self.watch_gpu();
        if self.engine.revision() != self.seen_revision {
            self.refresh();
        }
        self.place_fit_later();
        if let Some(t) = &self.transition
            && self.clock.now().0 >= t.start.0.saturating_add(t.duration.0)
        {
            self.transition = None;
            self.publish();
        }
        self.tick_pause();
        if let Some(v) = &mut self.video {
            if let Some(views) = v.shared.views.take() {
                v.views = views;
            }
            if let Some(views) = v.shared.thumbnail_views.take() {
                v.thumbnail_views = Some(views);
            }
            while let Some(e) = v.shared.errors.pop() {
                self.notices.push(Notice::plain(e.to_string(), true));
            }
        }
        self.monitor.poll(&self.mixer, &mut self.notices);
        self.finish_hardware_tests();
        // Before the poll, which drops a stream that failed.
        self.note_destinations();
        let events = self.outputs.poll();
        // After the poll, which sees the last lost connections of a stream that failed.
        self.note_reconnects();
        for n in events.notices {
            self.notices.push(n);
        }
        if events.sinks_changed {
            self.update_encode_target();
        }
        for id in events.removed {
            self.mixer.send(MixerCmd::RemoveOutput { id });
        }
        self.sync_audio_tracks();
        self.sync_captions();
        self.sync_recording_markers();
        self.mixer.free_retired();
    }

    /// Notice a lost graphics device and stop drawing with it. A device the studio opened is
    /// opened again, with [`Recovery`]'s preference: the same one, and software after repeated
    /// losses. The window's device is left to the window ([`Studio::gpu_lost`]).
    fn watch_gpu(&mut self) {
        let Some(gpu) = &self.gpu else { return };
        if self.gpu_loss.is_none() {
            if self.gpu_probed.elapsed() >= GPU_PROBE {
                gpu.poll();
                self.gpu_probed = Instant::now();
            }
            let Some(why) = gpu.lost() else { return };
            let retry_at = gpu.can_reopen().then(Instant::now);
            self.video = None;
            self.notices.push(Notice::new("The graphics device stopped working: {error}", vec![("error", why.clone())], true));
            if let Some(r) = &mut self.recovery {
                let step = r.lost(self.started.elapsed());
                if let Some(n) = &step.notice {
                    self.notices.push(gpu_notice(n));
                }
            }
            self.gpu_loss = Some(GpuLoss { why, retry_at });
            // Nothing is drawn until a new device replaces it, which may not happen at all, so
            // the outputs stop instead of running on without a picture. A device the studio
            // opened is opened again below.
            for kind in [OutputKind::Recording, OutputKind::Streaming, OutputKind::Replay] {
                self.stop_output(kind);
            }
        }
        let Some(gpu) = &self.gpu else { return };
        let Some(loss) = &mut self.gpu_loss else { return };
        let (Some(at), Some(recovery)) = (loss.retry_at, &self.recovery) else { return };
        if Instant::now() < at {
            return;
        }
        match gpu.reopen(recovery.preference()) {
            Ok((new, notices)) => {
                let name = new.adapter().map_or_else(String::new, |a| a.name.clone());
                self.notices.extend(notices.iter().map(gpu_notice));
                self.notices.push(Notice::new("Drawing again on {adapter}", vec![("adapter", name)], false));
                self.gpu = Some(new);
                self.gpu_loss = None;
                self.outputs.forget_hardware_tests();
                self.restart_video();
            }
            Err(e) => {
                self.notices.push(Notice::new("No graphics device could be opened again: {error}", vec![("error", e.to_string())], true));
                loss.retry_at = Some(Instant::now() + GPU_RETRY);
            }
        }
    }

    /// Why the graphics device the studio draws with was lost, while it is. Nothing is drawn
    /// until a new device arrives: the studio opens one itself when it opened the lost one, and
    /// the window has to replace its own with [`Studio::set_gpu`].
    pub fn gpu_lost(&self) -> Option<&str> {
        self.gpu_loss.as_ref().map(|l| l.why.as_str())
    }

    /// The graphics device the studio draws with.
    pub fn gpu(&self) -> Option<&Gpu> {
        self.gpu.as_ref()
    }

    /// Draw with `gpu` from now on (or nothing without one), restarting the compositor.
    pub fn set_gpu(&mut self, gpu: Option<Gpu>) {
        self.recovery = gpu.as_ref().and_then(Gpu::preference).map(|p| Recovery::new(p.clone(), RecoveryPolicy::default()));
        self.gpu = gpu;
        self.gpu_loss = None;
        self.outputs.forget_hardware_tests();
        self.restart_video();
    }

    /// Bring sources, the mixer and the compositor in line with the collection, and start a
    /// transition when the program scene changed.
    fn refresh(&mut self) {
        self.seen_revision = self.engine.revision();
        let c = self.engine.collection();
        for (id, problem) in self.sources.sync(c, self.engine.registry()) {
            let name = c.source(id).map_or_else(|| id.to_string(), |s| s.name.clone());
            self.notices.push(Notice::new("{name}: {problem}", vec![("name", name), ("problem", problem.to_string())], true));
        }
        let program = c.program_scene;
        let quick = self.quick.take();
        let resuming = matches!(&self.transition, Some(t) if t.from == Origin::Pause);
        let switched = (program != self.shown_program).then(|| {
            let named = |id| (id, c.scene(id).map(|s| s.name.clone()));
            events::Kind::Scene { from: named(self.shown_program), to: named(program) }
        });
        if program != self.shown_program && (self.pause.is_some() || resuming) {
            // While paused, the scene changes under the pause screen; the program shows it when
            // it resumes. A running fade back from the pause screen ends on the new scene.
            self.shown_program = program;
        } else if program != self.shown_program {
            let from = self.shown_program;
            self.shown_program = program;
            let t = &c.transition;
            let (kind, ms, settings) = match quick {
                Some((kind, ms)) => {
                    let settings = if kind == t.kind { t.settings.clone() } else { Settings::default() };
                    (kind, ms.unwrap_or(t.duration_ms), settings)
                }
                None => (t.kind.clone(), t.duration_ms, t.settings.clone()),
            };
            let instant = self.engine.registry().transition(&kind).map_or(true, |p| p.instant());
            let still_there = c.scene(from).is_some();
            self.transition = (!instant && ms > 0 && still_there).then(|| Running {
                from: Origin::Scene(from),
                start: self.clock.now(),
                duration: tick_of(std::time::Duration::from_millis(u64::from(ms))),
                kind,
                settings,
            });
            let fade = if self.transition.is_some() { ms } else { 0 };
            self.mixer.send(MixerCmd::FadeMs(fade));
        }
        if let Some(kind) = switched {
            self.note_event(kind);
        }
        let mix = self.wiring.mixer_inputs(self.engine.collection(), self.engine.registry(), &self.sources, &mut self.levels, &mut self.notices);
        if let Some(cmds) = mix {
            for cmd in cmds {
                self.mixer.send(cmd);
            }
        }
        self.sync_monitor();
        self.publish();
    }

    /// Open the monitoring device while a source with audio is monitored, and close it when none
    /// is.
    fn sync_monitor(&mut self) {
        let c = self.engine.collection();
        let registry = self.engine.registry();
        let wanted = c.sources.iter().any(|s| s.audio.monitor.monitored() && registry.source(&s.kind).is_ok_and(|p| p.caps().audio));
        self.monitor.sync(wanted, &self.profile.audio.monitoring_device, &self.mixer, &mut self.notices);
    }

    /// Play monitored sources through `sink` instead of the system's output devices.
    pub fn set_audio_sink(&mut self, sink: Arc<dyn studiocraft_platform::audio::AudioSink>) {
        self.monitor.set_sink(sink, &self.mixer);
        self.sync_monitor();
    }

    /// Whether monitored sources are playing on the monitoring device, and its format.
    pub fn monitoring(&self) -> Option<studiocraft_platform::audio::Format> {
        self.monitor.format()
    }

    /// Send the compositor what to draw now.
    fn publish(&mut self) {
        self.prune_thumbnails();
        if self.video.is_none() {
            return;
        }
        let plan = self.plan();
        let thumbnails = self.thumbnail_request().map(Arc::new);
        if let Some(v) = &mut self.video {
            v.shared.plan.publish(Arc::new(plan));
            // With no scene pictures asked for, the compositor hears nothing more after the first
            // `None`, so a list view costs nothing.
            if thumbnails.is_some() || v.thumbnails_sent.is_some() {
                v.shared.thumbnails.publish(thumbnails.clone());
                v.thumbnails_sent = thumbnails;
                #[cfg(test)]
                {
                    v.thumbnail_sends += 1;
                }
            }
        }
    }

    /// Draw small pictures of `scenes` (known and distinct, at most
    /// [`studiocraft_engine::command::MAX_THUMBNAIL_SCENES`]), in this order, `fps` times a
    /// second; none when the list is empty. Returns the list and rate now drawn.
    fn set_thumbnails(&mut self, scenes: Vec<SceneId>, fps: f32) -> Value {
        let unchanged = match &self.thumbnails {
            Some(t) => t.scenes == scenes && t.fps == fps,
            None => scenes.is_empty(),
        };
        if !unchanged {
            let old = self.thumbnails.take();
            let mut keys = Vec::with_capacity(scenes.len());
            for id in &scenes {
                let kept = old.as_ref().and_then(|t| t.scenes.iter().zip(&t.keys).find(|(s, _)| *s == id).map(|(_, &k)| k));
                keys.push(kept.unwrap_or_else(|| {
                    self.thumbnail_key = self.thumbnail_key.wrapping_add(1);
                    self.thumbnail_key
                }));
            }
            self.thumbnails = (!scenes.is_empty()).then_some(Thumbnails { scenes, keys, fps });
            self.publish();
        }
        self.thumbnails_json()
    }

    /// The scenes drawn as small pictures, in slot order, and the pictures a second of each.
    fn thumbnails_json(&self) -> Value {
        match &self.thumbnails {
            Some(t) => json!({"scenes": t.scenes, "fps": t.fps}),
            None => json!({"scenes": [], "fps": null}),
        }
    }

    /// Drop the scene pictures of scenes that left the collection, and stop drawing pictures once
    /// no scene is left.
    fn prune_thumbnails(&mut self) {
        let collection = self.engine.collection();
        if let Some(t) = &mut self.thumbnails {
            let (scenes, keys) = t.scenes.iter().zip(&t.keys).filter(|(id, _)| collection.scene(**id).is_some()).map(|(&id, &k)| (id, k)).unzip();
            (t.scenes, t.keys) = (scenes, keys);
            if t.scenes.is_empty() {
                self.thumbnails = None;
            }
        }
    }

    /// The scene pictures to ask the compositor for; `None` while none is shown.
    fn thumbnail_request(&self) -> Option<ThumbnailRequest> {
        let t = self.thumbnails.as_ref()?;
        Some(self.wiring.thumbnails(self.engine.collection(), &self.sources, &t.scenes, &t.keys, t.fps))
    }

    /// The scenes drawn as small pictures for a scene list, in slot order: the picture of
    /// `thumbnail_scenes()[i]` is slot `i` of [`Studio::thumbnail_views`]. Empty while none is
    /// shown.
    pub fn thumbnail_scenes(&self) -> &[SceneId] {
        self.thumbnails.as_ref().map_or(&[], |t| &t.scenes)
    }

    /// The textures the scene pictures are drawn into, once the compositor has made them with a
    /// slot for each of [`Studio::thumbnail_scenes`]; `None` while no scene picture is shown, and
    /// while a longer list waits for its new textures. The compositor keeps the textures between
    /// requests, so the views stay valid when the pictures are asked for again. After the list
    /// changes, a slot whose scene changed shows its last picture until the compositor draws it
    /// again: it draws two such slots a frame, before any other, so 16 changed slots take 8 frames.
    pub fn thumbnail_views(&self) -> Option<Arc<ThumbnailViews>> {
        let wanted = self.thumbnails.as_ref()?.scenes.len();
        self.video.as_ref().and_then(|v| v.thumbnail_views.clone()).filter(|views| views.slots.len() >= wanted)
    }

    /// What the compositor draws now: the program scene, or the pause screen while paused, and
    /// the transition that runs.
    fn plan(&mut self) -> studiocraft_render::Plan {
        let canvas = self.canvas();
        let layers = self.pause_layers.as_ref();
        let collection = self.engine.collection();
        let taken = |id| collection.source(id).is_some();
        let pause = self.pause.as_ref().and(layers).map(|l| l.scene(canvas, taken));
        let transition = self.transition.as_ref().map(|t| {
            let from = match t.from {
                Origin::Scene(id) => wiring::Shown::Scene(id),
                Origin::Pause => wiring::Shown::Pause(layers.map(|l| l.scene(canvas, taken)).unwrap_or_default()),
            };
            (from, t.start, t.duration, t.kind.as_str(), &t.settings)
        });
        self.wiring.plan(
            self.engine.collection(),
            self.engine.studio_mode(),
            self.engine.registry(),
            &self.sources,
            pause,
            transition,
            &mut self.notices,
        )
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.clock.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Keep `kind` in the session's events, with the time now and how far into the running
    /// recording and stream it is.
    fn note_event(&mut self, kind: events::Kind) {
        let stream = self.outputs.start_of(OutputKind::Streaming);
        self.note_event_in(kind, stream);
    }

    /// [`Studio::note_event`] for an event of the stream that started at `stream`, which may have
    /// stopped since.
    fn note_event_in(&mut self, kind: events::Kind, stream: Option<Tick>) {
        let now = self.clock.now();
        let into = |start: Option<Tick>| start.map(|start| duration_of(Tick(now.0.saturating_sub(start.0))).as_secs_f64());
        let (recording_s, stream_s) = (into(self.outputs.start_of(OutputKind::Recording)), into(stream));
        let time_ms = self.now_ms();
        self.events.push(time_ms, recording_s, stream_s, kind);
    }

    /// Keep a reconnect event for each stream destination that lost its connection since the
    /// last look, and show a notice for a lost connection and for a reconnect
    /// ([`events::ReconnectNotices`] decides which).
    fn note_reconnects(&mut self) {
        let now = self.clock.now();
        for (kind, stream) in self.outputs.reconnects_now() {
            if let events::Kind::Reconnect { destination, .. } = &kind
                && self.outputs.loss_notice(destination, now)
            {
                let name = vec![("name", self.outputs.stream_destination_name(destination))];
                self.notices.push(Notice::new("Lost the connection to {name}; reconnecting", name, true));
            }
            self.note_event_in(kind, Some(stream));
        }
        for id in self.outputs.recovered_now() {
            let name = vec![("name", self.outputs.stream_destination_name(&id))];
            self.notices.push(Notice::new("Reconnected to {name}", name, false));
        }
    }

    /// Start a transition in the program from `from` to what the program shows now, with the
    /// collection's transition, and fade the program mix in or out with it when the pause holds
    /// the scene's audio.
    fn begin_pause_transition(&mut self, from: Origin, hold: Option<bool>) {
        let t = &self.engine.collection().transition;
        let (kind, ms, settings) = (t.kind.clone(), t.duration_ms, t.settings.clone());
        let instant = self.engine.registry().transition(&kind).map_or(true, |p| p.instant());
        let still_there = match from {
            Origin::Scene(id) => self.engine.collection().scene(id).is_some(),
            Origin::Pause => self.pause_layers.is_some(),
        };
        let now = self.clock.now();
        let duration = tick_of(std::time::Duration::from_millis(u64::from(ms)));
        // A fade the other way that is still running is turned around where it stands, so the
        // picture does not jump: the new one starts where its eased blend shows what the old one
        // shows now. Other transitions are not blends that run the same backward, and start over.
        let registry = self.engine.registry();
        let shown = |kind: &str, settings: &Settings, p: f64| registry.transition(kind).map_or(p, |e| e.easing(settings).apply(p));
        let turns = |t: &&Running| (t.from == Origin::Pause) != (from == Origin::Pause) && t.kind == kind && TURNS_AROUND.contains(&kind.as_str());
        let q = self.transition.as_ref().filter(turns).map_or(0.0, |t| {
            let total = t.duration.0.max(1);
            let p = now.0.saturating_sub(t.start.0).clamp(0, total) as f64 / total as f64;
            let target = (1.0 - shown(&t.kind, &t.settings, p)).clamp(0.0, 1.0);
            progress_showing(|x| shown(&kind, &settings, x), target)
        });
        let start = Tick(now.0.saturating_sub((duration.0 as f64 * q).round() as i64));
        self.transition = (!instant && ms > 0 && still_there).then_some(Running { from, start, duration, kind, settings });
        if let Some(on) = hold {
            // The program audio goes the rest of its way from where it is now, and ends its fade
            // when the picture ends its transition.
            let running = self.transition.is_some();
            self.hold_scene_audio(on, |distance| if running { turned_fade_ms(ms, distance, 1.0 - q) } else { 0 });
        }
    }

    /// Fade the program mix out (`on`) or back in, from the level it has now. `fade` gives the
    /// fade's length for the whole range from the share of the range still to go.
    fn hold_scene_audio(&mut self, on: bool, fade: impl FnOnce(f64) -> u32) {
        let now = self.clock.now();
        let level = self.hold_fade.map_or(1.0, |h| h.level(now));
        let distance = if on { level } else { 1.0 - level };
        let fade = fade(distance);
        self.hold_fade = Some(HoldFade { start: now, from: level, down: on, ms: fade });
        self.mixer.send(MixerCmd::FadeMs(fade));
        self.mixer.send(MixerCmd::Hold(on));
    }

    /// Pause the program, or while paused apply the given values ([`Command::PauseStart`]).
    fn start_pause(&mut self, values: PauseValues) -> Result<Value, StudioError> {
        if let Some(m) = values.media.as_deref().filter(|m| !m.is_empty()) {
            pause::media_kind(self.engine.registry(), m).map_err(|e| StudioError::Engine(EngineError::Invalid(e)))?;
        }
        let now_ms = self.now_ms();
        let canvas = self.canvas();
        if let Some(state) = &mut self.pause {
            if let Some(s) = values.countdown_s {
                state.set_countdown(now_ms, s);
            }
            let held = state.settings.mute_scene;
            values.apply_to(&mut state.settings);
            let hold = state.settings.mute_scene;
            if values.media.is_some()
                && let Some(l) = &mut self.pause_layers
            {
                let mut problems = Vec::new();
                l.set_media(self.engine.registry(), self.clock, &state.settings.media, &mut problems);
                l.look.clone_from(&state.settings);
                for p in problems {
                    self.notices.push(Notice::new(pause::MEDIA_NOTICE, vec![("error", p)], true));
                }
            }
            if hold != held {
                // The program mix fades out or back in over the collection's transition length,
                // as it does when a pause starts or ends.
                let t = &self.engine.collection().transition;
                let instant = self.engine.registry().transition(&t.kind).map_or(true, |p| p.instant());
                let ms = if instant { 0 } else { t.duration_ms };
                self.hold_scene_audio(hold, |distance| turned_fade_ms(ms, distance, 1.0));
            }
            self.pause_seen = None;
            self.tick_pause();
            self.publish();
            return Ok(self.pause_json());
        }
        let mut settings = self.profile.pause.clone().sanitized();
        values.apply_to(&mut settings);
        let mut state = pause::PauseState::new(now_ms, settings);
        if let Some(s) = values.countdown_s {
            state.set_countdown(now_ms, s);
        }
        // The layers of a pause whose fade back still runs keep drawing; only what changed starts
        // again, so the screen does not go blank while new threads start.
        let (layers, problems) = match self.pause_layers.take() {
            Some(mut l) => {
                let mut problems = Vec::new();
                l.set_media(self.engine.registry(), self.clock, &state.settings.media, &mut problems);
                l.set_canvas(&state.settings, canvas);
                l.set_text(self.engine.registry(), self.clock, &state.settings, canvas, state.text(now_ms).as_deref());
                l.look.clone_from(&state.settings);
                (Some(l), problems)
            }
            None => pause::Layers::start(self.engine.registry(), self.clock, &state.settings, canvas, state.text(now_ms).as_deref()),
        };
        let Some(layers) = layers else {
            return Err(StudioError::Output(format!("the pause screen could not start: {}", problems.join("; "))));
        };
        for p in problems {
            self.notices.push(Notice::new(pause::MEDIA_NOTICE, vec![("error", p)], true));
        }
        let hold = state.settings.mute_scene;
        self.pause_layers = Some(layers);
        self.pause = Some(state);
        self.pause_seen = None;
        self.begin_pause_transition(Origin::Scene(self.shown_program), hold.then_some(true));
        self.notices.push(Notice::new("The program is paused", vec![], false));
        self.note_event(events::Kind::Pause);
        self.publish();
        Ok(self.pause_json())
    }

    /// Resume the program, with the collection's transition. An error when it is not paused.
    fn stop_pause(&mut self) -> Result<(), StudioError> {
        if self.pause.take().is_none() {
            return Err(StudioError::Output("the program is not paused".into()));
        }
        // The hold is released whether or not this pause set it, so it never outlives a pause.
        self.begin_pause_transition(Origin::Pause, Some(false));
        if self.transition.is_none() {
            self.pause_layers = None;
        }
        self.pause_seen = None;
        self.notices.push(Notice::new("The program resumed", vec![], false));
        self.note_event(events::Kind::Resume);
        self.publish();
        Ok(())
    }

    /// Add `seconds` to the running pause's countdown. An error when it is not paused.
    fn extend_pause(&mut self, seconds: u32) -> Result<Value, StudioError> {
        let now_ms = self.now_ms();
        let Some(state) = &mut self.pause else { return Err(StudioError::Output("the program is not paused".into())) };
        state.extend(now_ms, seconds);
        self.tick_pause();
        Ok(self.pause_json())
    }

    /// `{"gain_reduction_db"}`, the gain reduction `filter` of `source` measured in the mixer's
    /// last block, rounded to 0.01 dB, or `null` when the mixer does not run that filter or the
    /// filter has no meter.
    pub fn filter_meter_json(&self, source: SourceId, filter: FilterId) -> Value {
        self.wiring.filter_meter(source, filter).map_or(Value::Null, |db| json!({"gain_reduction_db": (f64::from(db) * 100.0).round() / 100.0}))
    }

    /// The sample frames, at the mixer's 48 kHz, that the audio filters the mixer runs on `source`
    /// delay its audio by, in total: 0 for a source the mixer does not run or whose filters add no
    /// delay.
    pub fn filter_latency_frames(&self, source: SourceId) -> usize {
        self.wiring.filter_latency(source)
    }

    /// Whether the program is paused, since when, and the countdown.
    pub fn pause_json(&self) -> Value {
        let layers = self.pause_layers.as_ref();
        let (media_error, text_error) = (layers.and_then(pause::Layers::media_error), layers.and_then(pause::Layers::text_error));
        self.pause.as_ref().map_or_else(|| json!({"paused": false}), |p| p.json(self.now_ms(), media_error.as_deref(), text_error.as_deref()))
    }

    /// Whether the program is paused, and the countdown's time left in milliseconds (`None`
    /// without a countdown).
    pub fn paused(&self) -> Option<Option<u64>> {
        let now = self.now_ms();
        self.pause.as_ref().map(|p| p.remaining_ms(now))
    }

    /// How long the program has been paused, in milliseconds.
    pub fn paused_for_ms(&self) -> Option<u64> {
        let now = self.now_ms();
        self.pause.as_ref().map(|p| now.saturating_sub(p.since_ms))
    }

    /// Once per interface frame while the pause screen runs: the countdown's zero step, the text
    /// when its whole seconds change, a new canvas, a media error, and new placements once the
    /// media and the text show something. The threads of a pause that ended stop once the
    /// transition back to the program has ended.
    fn tick_pause(&mut self) {
        let now_ms = self.now_ms();
        if let Some(state) = &mut self.pause {
            match state.step(now_ms) {
                Some(pause::Zero::Keep) => self.notices.push(Notice::new("The countdown ended", vec![], false)),
                Some(pause::Zero::Resume) => {
                    self.notices.push(Notice::new("The countdown ended", vec![], false));
                    // Only a pause can be stopped here, so this does not fail.
                    let _ = self.stop_pause();
                    return;
                }
                None => {}
            }
        }
        let leaving = !matches!(&self.transition, Some(t) if t.from == Origin::Pause);
        if self.pause.is_none() {
            if self.pause_layers.is_some() && leaving {
                self.pause_layers = None;
            }
            return;
        }
        let canvas = self.canvas();
        let (Some(state), Some(layers)) = (&self.pause, &mut self.pause_layers) else { return };
        let seen: PauseSeen = (state.remaining_ms(now_ms).map(|ms| ms.div_ceil(1000)), state.reached, state.total_s, canvas, layers.sizes());
        if self.pause_seen.as_ref() == Some(&seen) {
            for (template, e) in layers.untold_errors() {
                self.notices.push(Notice::new(template, vec![("error", e)], true));
            }
            return;
        }
        let placed_changed = self.pause_seen.as_ref().is_none_or(|old| old.3 != seen.3 || old.4 != seen.4);
        layers.look.clone_from(&state.settings);
        layers.set_canvas(&state.settings, canvas);
        layers.set_text(self.engine.registry(), self.clock, &state.settings, canvas, state.text(now_ms).as_deref());
        for (template, e) in layers.untold_errors() {
            self.notices.push(Notice::new(template, vec![("error", e)], true));
        }
        self.pause_seen = Some(seen);
        if placed_changed {
            self.publish();
        }
    }

    /// (Re)start the compositor at the profile's canvas size and frame rate.
    fn restart_video(&mut self) {
        self.video = None;
        // A lost device draws nothing until a new one replaces it.
        let Some(gpu) = self.gpu.clone().filter(|g| !g.is_lost()) else { return };
        let vs = self.profile.video.sanitized();
        let compositor = match Compositor::new(gpu, vs.base_width, vs.base_height) {
            Ok(c) => c,
            Err(e) => {
                self.notices.push(Notice::plain(StudioError::Gpu(e.to_string()).to_string(), true));
                return;
            }
        };
        let shared = Arc::new(RenderShared::default());
        let views = compositor.views();
        let rate = FrameRate::new(i64::from(vs.fps_num), i64::from(vs.fps_den));
        let (sh, mut comp) = (Arc::clone(&shared), compositor);
        match Scheduler::spawn("compositor", self.clock, rate, Arc::clone(&self.render_stats), move |t| comp.run(&sh, t.pts)) {
            Ok(scheduler) => {
                self.video = Some(Video {
                    shared,
                    views,
                    thumbnail_views: None,
                    thumbnails_sent: None,
                    #[cfg(test)]
                    thumbnail_sends: 0,
                    _scheduler: scheduler,
                });
                self.publish();
                self.update_encode_target();
                self.update_scope_target();
            }
            Err(e) => self.notices.push(Notice::new("The compositor thread could not start: {error}", vec![("error", e.to_string())], true)),
        }
    }

    /// The canvas textures to show, once the compositor runs.
    pub fn views(&self) -> Option<Arc<CanvasViews>> {
        self.video.as_ref().map(|v| Arc::clone(&v.views))
    }

    /// A still picture of the program or, in studio mode, the preview, taken after the
    /// compositor's next frame. Waits at most `timeout` for it.
    ///
    /// The compositor's frame thread answers the request, so this works while that thread runs at
    /// the profile's frame rate. A caller that drives the studio itself (the command-line tool, a
    /// test) calls [`Studio::tick`] before capturing so the latest changes reach the compositor.
    pub fn capture(&self, view: View, timeout: Duration) -> Result<Capture, StudioError> {
        if self.gpu.is_none() {
            return Err(StudioError::NoGpu);
        }
        if view == View::Preview && !self.engine.studio_mode() {
            return Err(StudioError::NoPreview);
        }
        let Some(v) = &self.video else {
            return Err(StudioError::Gpu("the compositor is not running".into()));
        };
        let (reply, answer) = sync_channel(1);
        if v.shared.captures.push(CaptureRequest { view, reply }).is_err() {
            return Err(StudioError::CaptureBusy);
        }
        match answer.recv_timeout(timeout) {
            Ok(Ok(capture)) => Ok(capture),
            Ok(Err(e)) => Err(StudioError::Gpu(e.to_string())),
            Err(RecvTimeoutError::Timeout) => Err(StudioError::CaptureTimeout(timeout)),
            Err(RecvTimeoutError::Disconnected) => Err(StudioError::Gpu("the compositor stopped".into())),
        }
    }

    /// The canvas size scenes are laid out on.
    pub fn canvas(&self) -> (u32, u32) {
        let v = self.profile.video.sanitized();
        (v.base_width, v.base_height)
    }

    /// The size a source shows at, once it has shown something: its picture's size after its
    /// enabled video filters, which can crop or pad it. `None` while the filters leave nothing.
    pub fn source_size(&self, id: SourceId) -> Option<(u32, u32)> {
        let native = self.sources.size(id)?;
        let size = self.wiring.filtered_size(self.engine.collection(), id, native);
        (size.0 > 0 && size.1 > 0).then_some(size)
    }

    /// Sends `()` on `tx` once a running source has published its first content, or at once when
    /// it already has. The call does not wait. Returns `false` when the source is not running or
    /// was already given a channel.
    pub fn notify_source_shown(&self, id: SourceId, tx: SyncSender<()>) -> bool {
        self.sources.io(id).is_some_and(|io| io.video.notify_published(tx))
    }

    /// Why a source is not working, if it is not.
    pub fn source_error(&self, id: SourceId) -> Option<String> {
        self.sources.error(id)
    }

    /// A note a working source shows beside its picture, in English. It is not an error.
    pub fn source_notice(&self, id: SourceId) -> Option<&'static str> {
        self.sources.notice(id)
    }

    /// Meter levels of an audio source.
    pub fn levels(&self, id: SourceId) -> Option<BlockLevels> {
        self.levels.get(&id).map(|l| l.load())
    }

    /// Meter levels of the final mix.
    pub fn master_levels(&self) -> BlockLevels {
        self.mixer.master().load()
    }

    /// Loudness of the final mix (ITU-R BS.1770-4, EBU R 128) since the mixer started or the
    /// last [`Studio::reset_loudness`].
    pub fn master_loudness(&self) -> LoudnessReading {
        self.mixer.loudness().load()
    }

    /// Start every loudness reading of the final mix again from the next block: momentary,
    /// short-term, integrated, loudness range and true peak. The momentary and short-term
    /// readings are −∞ until 400 ms and 3 s of the new program have played.
    pub fn reset_loudness(&self) {
        self.mixer.loudness().request_reset();
    }

    /// Whether a transition is running, and how far it is (0 to 1).
    pub fn transition_progress(&self) -> Option<f32> {
        let t = self.transition.as_ref()?;
        let p = (self.clock.now().0.saturating_sub(t.start.0)) as f64 / t.duration.0.max(1) as f64;
        Some(p.clamp(0.0, 1.0) as f32)
    }

    /// Whether a picture of `view` would show the scene as it now stands: for the program, no
    /// transition is running, and every visible item of the scene `view` shows whose source draws
    /// has published its first content or has failed. The preview outside studio mode is always
    /// settled.
    pub fn view_settled(&self, view: View) -> bool {
        let c = self.engine.collection();
        let scene = match view {
            View::Program => {
                if self.transition.is_some() {
                    return false;
                }
                c.program_scene
            }
            View::Preview if self.engine.studio_mode() => c.edit_scene(true),
            View::Preview => return true,
        };
        let Some(scene) = c.scene(scene) else { return true };
        scene.items.iter().filter(|i| i.visible).all(|i| {
            let draws = c.source(i.source).and_then(|s| self.engine.registry().source(&s.kind).ok()).is_some_and(|p| p.caps().video);
            !draws || self.sources.published(i.source) || self.sources.error(i.source).is_some()
        })
    }

    /// Frame times, dropped frames and encoder counters for the stats window.
    pub fn stats(&self) -> Snapshot {
        let encoders: Vec<Arc<EncoderStats>> = self.outputs.stats();
        let refs: Vec<&EncoderStats> = encoders.iter().map(Arc::as_ref).collect();
        let mut s = Snapshot::read(&self.render_stats, &refs);
        if let Some(v) = &self.video {
            s.encoder_dropped += v.shared.readback_lost.load(std::sync::atomic::Ordering::Relaxed);
        }
        s
    }

    pub fn output(&self, kind: OutputKind) -> Option<OutputState> {
        self.outputs.state(kind)
    }

    /// Messages since the last call.
    pub fn take_notices(&mut self) -> Vec<Notice> {
        std::mem::take(&mut self.notices)
    }

    pub fn notify(&mut self, text: impl Into<String>, error: bool) {
        self.notices.push(Notice::plain(text, error));
    }

    /// Apply new settings. Video changes restart the compositor; they are refused while a
    /// recording or stream runs. A new canvas size lays out every scene for it
    /// (`canvas.relayout`), as one undo step.
    pub fn set_profile(&mut self, profile: Profile) -> Result<(), StudioError> {
        let (from, to) = self.apply_profile(profile)?;
        if from != to {
            self.relayout(from, to);
        }
        Ok(())
    }

    /// Put back the settings that a [`Studio::set_profile`] call replaced, for example after the
    /// new profile could not be saved. When that call laid out the scenes for a new canvas and
    /// nothing changed the collection since but the placements of items that waited for a
    /// picture, those placements and the layout change are taken back and dropped, so neither
    /// undo nor redo offers them. Otherwise this works as `set_profile`.
    pub fn restore_profile(&mut self, previous: Profile) -> Result<(), StudioError> {
        let relaid_at = self.relaid_at.take();
        let placed = std::mem::take(&mut self.fit_placed);
        let (from, to) = self.apply_profile(previous)?;
        if from == to {
            return Ok(());
        }
        if let Some((step, was_dirty, after)) = relaid_at
            && *after == *self.engine.collection()
            && self.engine.steps_after(step).is_some_and(|after| after.iter().all(|s| placed.contains(s)))
            && self.discard_down_to(step)
        {
            if !was_dirty {
                self.engine.mark_saved();
            }
            // The items waited to be placed for the layout that was just taken back.
            self.fit_later.clear();
            self.fit_layout = None;
            self.refresh();
            self.sync_audio_tracks();
        } else {
            self.relayout(from, to);
        }
        Ok(())
    }

    /// Take back and drop the undo steps down to and including `step`. Returns whether `step`
    /// was dropped.
    fn discard_down_to(&mut self, step: u64) -> bool {
        while self.engine.step_holds(step) {
            let top = self.engine.last_step();
            if !self.engine.discard_last_step() {
                return false;
            }
            if top == Some(step) {
                return true;
            }
        }
        false
    }

    /// Apply new settings without laying out the scenes. Returns the old and the new canvas size.
    fn apply_profile(&mut self, profile: Profile) -> Result<((u32, u32), (u32, u32)), StudioError> {
        profile.audio_tracks.check().map_err(StudioError::Output)?;
        profile.pause.check().map_err(StudioError::Output)?;
        profile.audio.loudness_target.check().map_err(StudioError::Output)?;
        let video_changed = profile.video.sanitized() != self.profile.video.sanitized();
        if video_changed && self.outputs.any_active() {
            return Err(StudioError::Output("stop recording, streaming and the replay buffer before changing the video settings".into()));
        }
        if let Ok(mut f) = self.ffmpeg.write() {
            f.clone_from(&profile.output.ffmpeg_path);
        }
        let tracks_on = profile.audio_tracks.enabled && !self.profile.audio_tracks.enabled;
        let old_canvas = self.canvas();
        self.profile = profile;
        if video_changed {
            self.restart_video();
        }
        self.sync_monitor();
        if tracks_on {
            self.repair_audio_tracks_at_launch();
        }
        self.sync_audio_tracks();
        Ok((old_canvas, self.canvas()))
    }

    /// The sizes of the sources `ids` that show a picture now, for `canvas.relayout`.
    fn shown_sizes(&self, ids: impl Iterator<Item = SourceId>) -> Vec<SourceSize> {
        ids.filter_map(|id| self.source_size(id).map(|(width, height)| SourceSize { source: id, width, height }))
            .filter(|s| s.width <= MAX_SOURCE_SIDE && s.height <= MAX_SOURCE_SIDE)
            .collect()
    }

    /// Lay out every scene for a canvas that changed from `from` to `to`, as one undo step. The
    /// sources that show a picture are placed by their size. An item with a fit mode whose source
    /// shows nothing now is placed by its mode once the source shows a picture
    /// ([`Studio::place_fit_later`]). A failure is shown as a notice.
    fn relayout(&mut self, from: (u32, u32), to: (u32, u32)) {
        let sizes = self.shown_sizes(self.engine.collection().sources.iter().map(|s| s.id));
        let size = |(width, height): (u32, u32)| CanvasSize { width, height };
        self.engine.seal_history();
        let before = self.engine.revision();
        let was_dirty = self.engine.is_dirty();
        self.relaid_at = None;
        self.fit_layout = None;
        self.fit_seen = None;
        self.fit_placed.clear();
        match self.execute(Command::CanvasRelayout { from: size(from), to: size(to), sizes: sizes.clone(), items: None }) {
            Ok(_) if self.engine.revision() != before => {
                self.relaid_at = self.engine.last_step().map(|step| (step, was_dirty, Box::new(self.engine.collection().clone())));
                self.fit_layout = self.engine.last_step();
            }
            Ok(_) => {}
            Err(e) => {
                self.notices.push(Notice::new("The scenes could not be laid out for the new canvas: {error}", vec![("error", e.to_string())], true));
            }
        }
        self.engine.seal_history();
        self.fit_later = self
            .engine
            .collection()
            .scenes
            .iter()
            .flat_map(|s| &s.items)
            .filter(|i| !sizes.iter().any(|s| s.source == i.source))
            .filter_map(|i| i.fit.map(|fit| FitLater { item: i.id, source: i.source, fit, transform: i.transform }))
            .collect();
    }

    /// Place the items with a fit mode whose sources showed nothing at the last canvas change,
    /// once those sources show a picture: by their mode on the current canvas, as one undo step.
    /// An item is placed only while the undo step of that change's layout is in effect and the
    /// item still has its mode and the transform that change gave it. An undo that takes that
    /// layout back, or a later edit, leaves it where it is; a redo, or an undo that brings back a
    /// removed item, makes it wait again. Once the layout step can no longer be undone or redone
    /// (a new edit after its undo, or the oldest steps dropped), nothing waits.
    fn place_fit_later(&mut self) {
        // This runs every frame. Until a waiting source starts to show a picture, its picture
        // changes size or the collection changes, it allocates nothing.
        // An FNV-1a digest over the list, which keeps its order.
        let mut shown = 0;
        let mut ids = 0xcbf2_9ce4_8422_2325_u64;
        for (i, w) in self.fit_later.iter().enumerate() {
            if let Some((width, height)) = self.sources.size(w.source) {
                shown += 1;
                for v in [i as u64, u64::from(width), u64::from(height)] {
                    ids = (ids ^ v).wrapping_mul(0x0100_0000_01b3);
                }
            }
        }
        let seen = (self.engine.revision(), shown, ids);
        if shown == 0 || self.fit_seen == Some(seen) {
            return;
        }
        self.fit_seen = Some(seen);
        if let Some(step) = self.fit_layout
            && !self.engine.step_holds(step)
        {
            return;
        }
        let c = self.engine.collection();
        let waiting: Vec<&FitLater> = self
            .fit_later
            .iter()
            .filter(|w| {
                c.scenes
                    .iter()
                    .flat_map(|s| &s.items)
                    .any(|i| i.id == w.item && i.source == w.source && i.fit == Some(w.fit) && i.transform == w.transform)
            })
            .collect();
        let mut sources: Vec<SourceId> = waiting.iter().map(|w| w.source).collect();
        sources.sort_unstable();
        sources.dedup();
        let ready = self.shown_sizes(sources.into_iter());
        let items: Vec<ItemId> = waiting.iter().filter(|w| ready.iter().any(|s| s.source == w.source)).map(|w| w.item).collect();
        if items.is_empty() {
            return;
        }
        self.fit_later.retain(|w| !items.contains(&w.item));
        let (width, height) = self.canvas();
        let canvas = CanvasSize { width, height };
        self.engine.seal_history();
        let before = self.engine.revision();
        let untouched = self.relaid_at.as_ref().is_some_and(|(_, _, after)| **after == *self.engine.collection());
        match self.execute(Command::CanvasRelayout { from: canvas, to: canvas, sizes: ready, items: Some(items) }) {
            Ok(_) if self.engine.revision() != before => {
                self.fit_placed.extend(self.engine.last_step());
                // A placement right after the layout change keeps it reversible.
                if untouched && let Some((_, _, after)) = &mut self.relaid_at {
                    **after = self.engine.collection().clone();
                }
            }
            Ok(_) => {}
            Err(e) => {
                self.notices.push(Notice::new("The scenes could not be laid out for the new canvas: {error}", vec![("error", e.to_string())], true));
            }
        }
        self.engine.seal_history();
    }

    pub fn video_settings(&self) -> VideoSettings {
        self.profile.video.sanitized()
    }

    /// The state scripts and the control channel read: the collection, the studio mode, and
    /// what is running.
    pub fn state_json(&self) -> Value {
        let mut v = self.engine.state_json();
        let output = |k: OutputKind| {
            self.output(k).map_or(
                json!({"active": false}),
                |s| json!({"active": s.active, "status": s.status_text(), "target": s.target, "seconds": s.elapsed.as_secs_f64()}),
            )
        };
        let stats = self.stats();
        if let Some(o) = v.as_object_mut() {
            o.insert("recording".into(), output(OutputKind::Recording));
            o.insert("streaming".into(), output(OutputKind::Streaming));
            let mut replay = output(OutputKind::Replay);
            if let Some(r) = replay.as_object_mut() {
                r.insert("saving".into(), json!(self.outputs.saving()));
                if let Some(s) = self.output(OutputKind::Replay).filter(|s| s.active) {
                    r.insert("bytes".into(), json!(s.bytes));
                }
            }
            o.insert("replay".into(), replay);
            o.insert("transitioning".into(), json!(self.transition.is_some()));
            let mut monitoring = json!({"device": self.profile.audio.monitoring_device, "playing": self.monitor.playing()});
            if let (Some(m), Some(f)) = (monitoring.as_object_mut(), self.monitor.format()) {
                m.insert("rate".into(), json!(f.rate));
                m.insert("channels".into(), json!(f.channels));
            }
            o.insert("monitoring".into(), monitoring);
            o.insert("audio_tracks".into(), self.audio_tracks_json());
            o.insert("captions".into(), self.captions_json());
            o.insert("pause".into(), self.pause_json());
            let latencies: serde_json::Map<String, Value> =
                self.wiring.filter_latencies().map(|(id, frames)| (id.0.to_string(), latency_json(frames))).collect();
            o.insert("filter_latency".into(), Value::Object(latencies));
            o.insert(
                "stats".into(),
                json!({
                    "frames": stats.frames, "lagged": stats.lagged, "render_ms": stats.render_ms, "render_avg_ms": stats.render_avg_ms,
                    "encoder_frames": stats.encoder_frames, "encoder_dropped": stats.encoder_dropped, "encoder_queued": stats.encoder_queued,
                }),
            );
            let canvas = self.canvas();
            o.insert("canvas".into(), json!({"width": canvas.0, "height": canvas.1, "fps": self.profile.video.sanitized().fps()}));
        }
        v
    }

    fn update_encode_target(&mut self) {
        let Some(v) = &self.video else { return };
        let vs = self.profile.video.sanitized();
        let sinks = self.outputs.video_sinks();
        let target = (!sinks.is_empty()).then(|| studiocraft_render::EncodeTarget { width: vs.output_width, height: vs.output_height, sinks });
        v.shared.encode.publish(target);
    }

    /// Send the compositor's small NV12 copy of the program to `feed`, or stop it with `None`.
    /// Without a graphics device no copy is made.
    pub fn set_scope_feed(&mut self, feed: Option<ScopeFeed>) {
        self.scope_feed = feed;
        self.update_scope_target();
    }

    /// Program copies the scopes never received because the compositor's read-back for them fell
    /// behind (0 without a graphics device).
    pub fn scope_skipped(&self) -> u64 {
        self.video.as_ref().map_or(0, |v| v.shared.scope_skipped.load(std::sync::atomic::Ordering::Relaxed))
    }

    fn update_scope_target(&mut self) {
        let Some(v) = &self.video else { return };
        let canvas = self.canvas();
        let target = self.scope_feed.as_ref().map(|f| {
            let (width, height) = scope_size(canvas, f.max_width, f.max_height);
            studiocraft_render::EncodeTarget { width, height, sinks: vec![Arc::clone(&f.sink)] }
        });
        v.shared.scope.publish(target);
    }
}

impl Drop for Studio {
    fn drop(&mut self) {
        // Recordings and audio tracks are finished, not cut off, when the app closes. Both are
        // asked to stop before either is waited for, so neither runs on while the other ends.
        self.stop_audio_tracks();
        let closed = self.clock.now();
        self.outputs.stop_all_blocking();
        self.finish_audio_tracks_blocking();
        // Caption files of a recording that just stopped are written before the studio closes.
        self.finish_captions_blocking();
        // So is the marker file, with the recording ending when the app closed.
        self.finish_recording_markers_blocking(closed);
    }
}

#[cfg(test)]
mod tests;
