//! The running studio.
//!
//! [`Studio`] owns the [`Engine`] (the scene collection and its undo history) and everything that
//! runs from it: one thread per source, the audio mixer, the compositor's frame thread, the
//! scene transition in progress, and the recording and stream. The interface, hotkeys, scripts
//! and the control channel all go through [`Studio::execute`]; after each change the studio
//! brings the running parts in line with the collection.

pub mod capture;
pub mod files;
mod outputs;
mod wiring;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{RecvTimeoutError, sync_channel};
use std::time::Duration;

use serde_json::{Value, json};
use studiocraft_engine::host::SourceSet;
use studiocraft_engine::mixer::{LoudnessReading, Mixer, MixerCmd};
use studiocraft_engine::scheduler::Scheduler;
use studiocraft_engine::{Command, Engine, EngineError, RuntimeAction};
use studiocraft_media::clock::{MasterClock, tick_of};
use studiocraft_media::mix::{BlockLevels, Levels};
use studiocraft_media::queue::Fifo;
use studiocraft_media::stats::{EncoderStats, RenderStats, Snapshot};
use studiocraft_media::video::PooledFrame;
use studiocraft_media::{FrameRate, Tick};
use studiocraft_model::{Profile, SceneCollection, SceneId, Settings, SourceId, VideoSettings};
use studiocraft_plugin::{PluginError, Registry};
use studiocraft_render::{CanvasViews, CaptureRequest, Compositor, Gpu, RenderShared};
use studiocraft_sources::FfmpegSetting;

pub use files::FileError;
pub use outputs::{OutputKind, OutputState, recording_folder, recording_path};
pub use studiocraft_render::{Capture, View};

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

/// Every plug-in StudioCraft ships with.
pub fn builtin_registry(ffmpeg: &FfmpegSetting) -> Result<Registry, PluginError> {
    use studiocraft_render::transitions::{Cut, Fade, FadeThroughColor, Swipe};
    let mut reg = Registry::new();
    studiocraft_sources::register_all(&mut reg, ffmpeg)?;
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
    _scheduler: Scheduler,
}

/// A scene transition running in the program.
#[derive(Clone)]
struct Running {
    from: SceneId,
    start: Tick,
    duration: Tick,
    kind: String,
    settings: Settings,
}

pub struct Studio {
    engine: Engine,
    clock: MasterClock,
    sources: SourceSet,
    mixer: Mixer,
    /// Meter levels of each audio source, kept across mixer rebuilds.
    levels: HashMap<SourceId, Arc<Levels>>,
    gpu: Option<Gpu>,
    video: Option<Video>,
    render_stats: Arc<RenderStats>,
    profile: Profile,
    ffmpeg: FfmpegSetting,
    wiring: wiring::Wiring,
    transition: Option<Running>,
    /// The transition and length `studio.transition` asked for, for the switch it causes.
    quick: Option<(String, Option<u32>)>,
    /// The program scene the compositor shows (or is transitioning to).
    shown_program: SceneId,
    seen_revision: u64,
    outputs: outputs::Outputs,
    notices: Vec<Notice>,
    /// Where the scopes' copy of the program goes, and the largest size it is drawn at.
    scope_feed: Option<ScopeFeed>,
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

impl Studio {
    /// A studio for `collection` with `profile`. Without `gpu` nothing is composited (the
    /// command-line tool and tests); with one, the compositor starts at once.
    pub fn new(gpu: Option<Gpu>, collection: SceneCollection, profile: Profile, registry: Arc<Registry>, ffmpeg: FfmpegSetting) -> Studio {
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
        let mut s = Studio {
            engine: Engine::new(collection, registry),
            clock,
            sources: SourceSet::new(clock),
            mixer,
            levels: HashMap::new(),
            gpu,
            video: None,
            render_stats: Arc::new(RenderStats::default()),
            profile,
            ffmpeg,
            wiring: wiring::Wiring::default(),
            transition: None,
            quick: None,
            shown_program,
            seen_revision: 0,
            outputs: outputs::Outputs::default(),
            notices: Vec::new(),
            scope_feed: None,
        };
        s.restart_video();
        s.refresh();
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
        let outcome = self.engine.execute(cmd);
        let outcome = match outcome {
            Ok(o) => o,
            Err(e) => {
                self.quick = None;
                return Err(e.into());
            }
        };
        let mut ui = None;
        if let Some(action) = outcome.runtime {
            match action {
                RuntimeAction::StartRecording => self.start_output(OutputKind::Recording)?,
                RuntimeAction::StopRecording => self.stop_output(OutputKind::Recording),
                RuntimeAction::ToggleRecording => self.toggle_output(OutputKind::Recording)?,
                RuntimeAction::StartStreaming => self.start_output(OutputKind::Streaming)?,
                RuntimeAction::StopStreaming => self.stop_output(OutputKind::Streaming),
                RuntimeAction::ToggleStreaming => self.toggle_output(OutputKind::Streaming)?,
                RuntimeAction::OpenSettings => ui = Some(UiAction::OpenSettings),
                RuntimeAction::Exit => ui = Some(UiAction::Exit),
            }
        }
        if outcome.changed {
            self.refresh();
        } else if transition {
            // A transition that changed nothing drops its override, so a later switch uses the
            // collection's transition.
            self.quick = None;
        }
        Ok((outcome.result, ui))
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
        self.engine.load(collection);
        self.transition = None;
        self.shown_program = self.engine.collection().program_scene;
        self.refresh();
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
        if self.engine.revision() != self.seen_revision {
            self.refresh();
        }
        if let Some(t) = &self.transition
            && self.clock.now().0 >= t.start.0.saturating_add(t.duration.0)
        {
            self.transition = None;
            self.publish();
        }
        if let Some(v) = &mut self.video {
            if let Some(views) = v.shared.views.take() {
                v.views = views;
            }
            while let Some(e) = v.shared.errors.pop() {
                self.notices.push(Notice::plain(e.to_string(), true));
            }
        }
        let events = self.outputs.poll();
        for n in events.notices {
            self.notices.push(n);
        }
        if events.sinks_changed {
            self.update_encode_target();
        }
        for id in events.removed {
            self.mixer.send(MixerCmd::RemoveOutput { id });
        }
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
        if program != self.shown_program {
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
                from,
                start: self.clock.now(),
                duration: tick_of(std::time::Duration::from_millis(u64::from(ms))),
                kind,
                settings,
            });
            let fade = if self.transition.is_some() { ms } else { 0 };
            self.mixer.send(MixerCmd::FadeMs(fade));
        }
        let mix = self.wiring.mixer_inputs(self.engine.collection(), self.engine.registry(), &self.sources, &mut self.levels, &mut self.notices);
        if let Some(cmds) = mix {
            for cmd in cmds {
                self.mixer.send(cmd);
            }
        }
        self.publish();
    }

    /// Send the compositor what to draw now.
    fn publish(&mut self) {
        let Some(v) = &self.video else { return };
        let transition = self.transition.as_ref().map(|t| (t.from, t.start, t.duration, t.kind.as_str(), &t.settings));
        let plan = self.wiring.plan(
            self.engine.collection(),
            self.engine.studio_mode(),
            self.engine.registry(),
            &self.sources,
            transition,
            &mut self.notices,
        );
        v.shared.plan.publish(Arc::new(plan));
    }

    /// (Re)start the compositor at the profile's canvas size and frame rate.
    fn restart_video(&mut self) {
        self.video = None;
        let Some(gpu) = self.gpu.clone() else { return };
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
                self.video = Some(Video { shared, views, _scheduler: scheduler });
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

    /// The size a source shows at, once it has shown something.
    pub fn source_size(&self, id: SourceId) -> Option<(u32, u32)> {
        self.sources.size(id)
    }

    /// Why a source is not working, if it is not.
    pub fn source_error(&self, id: SourceId) -> Option<String> {
        self.sources.error(id)
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

    /// Start the integrated loudness, loudness range and true peak of the final mix again from
    /// the next block.
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
    /// recording or stream runs.
    pub fn set_profile(&mut self, profile: Profile) -> Result<(), StudioError> {
        let video_changed = profile.video.sanitized() != self.profile.video.sanitized();
        if video_changed && self.outputs.any_active() {
            return Err(StudioError::Output("stop recording and streaming before changing the video settings".into()));
        }
        if let Ok(mut f) = self.ffmpeg.write() {
            f.clone_from(&profile.output.ffmpeg_path);
        }
        self.profile = profile;
        if video_changed {
            self.restart_video();
        }
        Ok(())
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
            o.insert("transitioning".into(), json!(self.transition.is_some()));
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
        // Recordings are finished, not cut off, when the app closes.
        self.outputs.stop_all_blocking();
    }
}

#[cfg(test)]
mod tests;
