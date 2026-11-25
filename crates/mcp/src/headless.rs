//! A studio in this process, without a window.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::{Map, Value, json};
use studiocraft_engine::command::Undo;
use studiocraft_engine::{Command, EngineError, SourceRef};
use studiocraft_model::{FieldKind, PathMode, Profile, SceneCollection, Settings, SettingsSchema, SourceConfig};
use studiocraft_render::Gpu;
use studiocraft_studio::capture::{MAX_PNG_SIDE, png, scaled_size};
use studiocraft_studio::files::read_collection_file;
use studiocraft_studio::{
    CaptionBoard, CaptionState, OutputKind, Studio, StudioError, View, builtin_registry, default_collection, pause_media_extensions, recording_folder,
};

use crate::backend::{Backend, McpError};
use crate::roots::{Access, Roots, shown};

/// How long one capture waits for the compositor.
const CAPTURE_WAIT: Duration = Duration::from_secs(2);
/// Captures tried before `render.preview` gives up.
const CAPTURE_TRIES: u32 = 3;
/// Longest `render.preview` waits for a running transition to end and new sources to draw.
const SETTLE_WAIT: Duration = Duration::from_secs(2);
/// How often `render.preview` looks again while it waits.
const SETTLE_STEP: Duration = Duration::from_millis(10);

/// Engine commands that only the window carries out.
const WINDOW_ENGINE_COMMANDS: [&str; 2] = ["app.settings", "app.exit"];

/// How to reach the window, for errors about window-only calls.
pub const BRIDGE_HINT: &str = "start the app with `studiocraft --control <port>` (STUDIOCRAFT_CONTROL_TOKEN set to a 64-character \
                               hexadecimal token) and serve it with `studiocraft-cli mcp --connect <port>`";

type Params = Map<String, Value>;

fn needs_app(what: &str) -> McpError {
    McpError::NeedsApp(format!("{what} needs the app's window; {BRIDGE_HINT}"))
}

/// The path `roots` resolved for `access`, as the text a setting or command holds: the path
/// without Windows' `\\?\` prefix, which is checked with `roots` again and must resolve to
/// `resolved`. Windows reads a path without that prefix by other rules (it drops the dots and
/// spaces that end a name), so a text that resolves elsewhere is an error, as is a path that is
/// not valid Unicode.
pub(crate) fn setting_text(roots: &Roots, resolved: &Path, access: Access) -> Result<String, McpError> {
    if resolved.to_str().is_none() {
        return Err(McpError::Invalid(format!("{} resolves to a path that is not valid Unicode", resolved.display())));
    }
    let text = shown(resolved).to_string_lossy().into_owned();
    if roots.check(Path::new(&text), access)?.as_path() != resolved {
        return Err(McpError::Invalid(format!("{text} does not name the path the roots checked")));
    }
    Ok(text)
}

/// The parameters as an object; a missing one is empty.
fn object(params: Value) -> Result<Params, McpError> {
    match params {
        Value::Object(m) => Ok(m),
        Value::Null => Ok(Map::new()),
        _ => Err(McpError::Invalid("`params` must be an object".into())),
    }
}

fn opt_str<'a>(p: &'a Params, key: &str) -> Result<Option<&'a str>, McpError> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(McpError::Invalid(format!("`{key}` must be a string"))),
    }
}

fn req_str<'a>(p: &'a Params, key: &str) -> Result<&'a str, McpError> {
    opt_str(p, key)?.ok_or_else(|| McpError::Invalid(format!("`{key}` is needed")))
}

/// `document.inspect`'s summary: the state without source, filter and transition settings.
pub(crate) fn summary(mut state: Value) -> Value {
    if let Some(sources) = state.pointer_mut("/collection/sources").and_then(Value::as_array_mut) {
        for s in sources.iter_mut().filter_map(Value::as_object_mut) {
            s.remove("settings");
            if let Some(filters) = s.get_mut("filters").and_then(Value::as_array_mut) {
                for f in filters.iter_mut().filter_map(Value::as_object_mut) {
                    f.remove("settings");
                }
            }
        }
    }
    if let Some(t) = state.pointer_mut("/collection/transition").and_then(Value::as_object_mut) {
        t.remove("settings");
    }
    state
}

/// What the in-process server may do beyond editing the scene collection.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServerOptions {
    /// Starting a stream is allowed (`--allow-streaming`). It publishes to the RTMP server in the
    /// stream settings, so it is off unless asked for.
    pub allow_streaming: bool,
    /// The folders files may be read from and written to; `None` leaves file access as it is.
    pub roots: Option<Roots>,
}

/// What `stream.start` answers when streaming was not allowed.
pub(crate) const STREAMING_REFUSED: &str = "streaming is off in this server: a stream publishes to the RTMP server in Settings › Stream, so the \
                                     in-process server starts one only when it was started with `studiocraft-cli mcp --allow-streaming`";

/// The texts the roots resolved for the profile's path settings a command reaches, each as the
/// setting is to hold it before the command runs.
#[derive(Default)]
struct Held {
    /// The recording folder, when the command writes there.
    recording: Option<String>,
    /// The pause media, when the command starts a pause that shows it.
    pause_media: Option<String>,
}

/// A studio with the built-in plug-ins, a new scene collection and default settings.
pub struct Headless {
    studio: Studio,
    options: ServerOptions,
}

impl Headless {
    /// A studio on the system's graphics device, or without one when none is available (then
    /// `render.preview` reports that nothing is drawn).
    pub fn new() -> Result<Headless, McpError> {
        Headless::with_gpu(Gpu::headless().ok())
    }

    /// A studio that composites on `gpu`, or draws nothing without one.
    pub fn with_gpu(gpu: Option<Gpu>) -> Result<Headless, McpError> {
        Headless::with_options(gpu, ServerOptions::default())
    }

    /// A studio on `gpu` held to `options`.
    pub fn with_options(gpu: Option<Gpu>, options: ServerOptions) -> Result<Headless, McpError> {
        let ffmpeg = Arc::new(RwLock::new(String::new()));
        let captions = Arc::new(CaptionBoard::new());
        let registry = builtin_registry(&ffmpeg, &captions).map_err(|e| McpError::Studio(e.into()))?;
        let mut studio = Studio::new(gpu, default_collection("Untitled"), Profile::default(), Arc::new(registry), ffmpeg, captions);
        // The status looks at speech model files only inside the read roots.
        if let Some(roots) = options.roots.clone() {
            studio.set_model_file_filter(Some(Arc::new(move |p: &Path| roots.check(p, Access::Read).is_ok())));
        }
        Ok(Headless { studio, options })
    }

    pub fn studio(&self) -> &Studio {
        &self.studio
    }

    /// The studio, to change its settings before serving (the recording folder, the ffmpeg
    /// program, the stream server).
    pub fn studio_mut(&mut self) -> &mut Studio {
        &mut self.studio
    }

    pub fn options(&self) -> &ServerOptions {
        &self.options
    }

    /// `path` resolved when the roots allow `access` to it; as given without roots.
    fn allowed(&self, path: &Path, access: Access) -> Result<PathBuf, McpError> {
        match &self.options.roots {
            Some(roots) => Ok(roots.check(path, access)?),
            None => Ok(path.to_path_buf()),
        }
    }

    /// The models folder, and the folder and files of speech model `model` in it, held to the
    /// roots, so a link inside the models folder cannot lead outside them.
    fn speech_model_allowed(&self, model: Option<&str>, access: Access) -> Result<(), McpError> {
        self.allowed(self.studio.models_dir(), access)?;
        for path in self.studio.speech_model_paths(model) {
            self.allowed(&path, access)?;
        }
        Ok(())
    }

    /// The models folder, and the folder and file of face tracking model `model` in it, held to
    /// the roots.
    fn face_model_allowed(&self, model: Option<&str>, access: Access) -> Result<(), McpError> {
        self.allowed(self.studio.models_dir(), access)?;
        for path in self.studio.face_model_paths(model) {
            self.allowed(&path, access)?;
        }
        Ok(())
    }

    /// Every path field of `schema` set in `settings`, held to the roots: `settings` with each such
    /// path replaced by the path the roots resolved, so the source opens the file that was checked.
    fn check_paths(&self, schema: &SettingsSchema, settings: &Settings) -> Result<Settings, McpError> {
        let mut held = settings.clone();
        for field in &schema.fields {
            let FieldKind::Path { mode, .. } = &field.kind else { continue };
            let Some(path) = settings.get(&field.key).and_then(Value::as_str).filter(|p| !p.trim().is_empty()) else { continue };
            let access = if *mode == PathMode::SaveFile { Access::Write } else { Access::Read };
            let resolved = self.allowed(Path::new(path), access)?;
            if let Some(roots) = &self.options.roots {
                held.set(&field.key, setting_text(roots, &resolved, access)?);
            }
        }
        Ok(held)
    }

    /// The path settings of a source and its filters, held to the roots and set to the paths the
    /// roots resolved.
    fn check_source(&self, source: &mut SourceConfig) -> Result<(), McpError> {
        let registry = self.studio.registry();
        // A kind this build lacks is not run, so its settings read no file.
        if let Ok(plugin) = registry.source(&source.kind) {
            source.settings = self.check_paths(&plugin.schema(), &source.settings)?;
        }
        for f in &mut source.filters {
            if let Ok(plugin) = registry.filter(&f.kind) {
                f.settings = self.check_paths(&plugin.schema(), &f.settings)?;
            }
        }
        Ok(())
    }

    /// Every file a scene collection's sources, filters and transition name, held to the read
    /// roots and set to the paths the roots resolved.
    fn check_collection(&self, c: &mut SceneCollection) -> Result<(), McpError> {
        for s in &mut c.sources {
            self.check_source(s)?;
        }
        if let Ok(plugin) = self.studio.registry().transition(&c.transition.kind) {
            c.transition.settings = self.check_paths(&plugin.schema(), &c.transition.settings)?;
        }
        Ok(())
    }

    fn source_config(&self, r: &SourceRef) -> Option<&SourceConfig> {
        let c = self.studio.collection();
        match r {
            SourceRef::Id(id) => c.source(*id),
            SourceRef::Name(n) => c.source_by_name(n),
        }
    }

    fn running(&self, kind: OutputKind) -> bool {
        self.studio.output(kind).is_some_and(|s| s.active || s.stopping)
    }

    /// Whether a stream starts audio tracks with the current settings.
    fn tracks_follow_stream(&self) -> bool {
        let t = &self.studio.profile().audio_tracks;
        t.enabled && matches!(t.when, studiocraft_model::AudioTracksWhen::Streaming | studiocraft_model::AudioTracksWhen::Either)
    }

    /// Refuse a command that would stream without permission or reach a file outside the roots,
    /// before anything runs. The command to run: with roots, each path it names is replaced by the
    /// path the roots resolved, so it acts on the file or folder that was checked. With it, the
    /// texts the profile's path settings are to hold, for those the command reaches.
    fn guard(&self, mut cmd: Command) -> Result<(Command, Held), McpError> {
        let starts_stream = match &cmd {
            Command::StreamStart {} => true,
            Command::StreamToggle {} => !self.running(OutputKind::Streaming),
            _ => false,
        };
        if starts_stream && !self.options.allow_streaming {
            return Err(McpError::NotAllowed(STREAMING_REFUSED.into()));
        }
        let Some(roots) = &self.options.roots else { return Ok((cmd, Held::default())) };
        let registry = self.studio.registry();
        let mut held = Held::default();
        match &mut cmd {
            Command::RecordStart {} | Command::RecordToggle {} if !self.running(OutputKind::Recording) => {
                held.recording = Some(self.checked_recording_folder(roots)?);
            }
            // A saved replay goes to the recording folder too.
            Command::ReplaySave {} => held.recording = Some(self.checked_recording_folder(roots)?),
            // Audio tracks of a stream go to the recording folder.
            Command::StreamStart {} | Command::StreamToggle {} if starts_stream && self.tracks_follow_stream() => {
                held.recording = Some(self.checked_recording_folder(roots)?);
            }
            // A speech model is downloaded into the models folder.
            Command::CaptionsDownload { model } => {
                self.speech_model_allowed(model.as_deref(), Access::Write)?;
            }
            // Starting captions reads a speech model from the models folder.
            Command::CaptionsStart { model, .. } => {
                self.speech_model_allowed(model.as_deref(), Access::Read)?;
            }
            Command::CaptionsToggle {} if !matches!(self.studio.captions_state(), CaptionState::Loading | CaptionState::Running) => {
                self.speech_model_allowed(None, Access::Read)?;
            }
            // A face tracking model is downloaded into the models folder.
            Command::AvatarModelDownload { model } => {
                self.face_model_allowed(model.as_deref(), Access::Write)?;
            }
            // An install reads the user's file and writes it into the models folder.
            Command::AvatarModelInstall { path, model } => {
                let resolved = self.allowed(Path::new(path.as_str()), Access::Read)?;
                *path = setting_text(roots, &resolved, Access::Read)?;
                self.face_model_allowed(model.as_deref(), Access::Write)?;
            }
            // The pause screen reads its picture or video.
            Command::PauseStart { media: Some(media), .. } if !media.trim().is_empty() => {
                *media = self.checked_pause_media(roots, media)?;
            }
            // A pause that starts without its own media shows the profile's, set to the path the
            // roots resolved.
            Command::PauseStart { media: None, .. } | Command::PauseToggle {} if self.studio.paused().is_none() => {
                let media = &self.studio.profile().pause.media;
                if !media.trim().is_empty() {
                    held.pause_media = Some(self.checked_pause_media(roots, media)?);
                }
            }
            // Repair rewrites the files it finds.
            Command::AudioTracksRepair { folder } => {
                let resolved = self.allowed(Path::new(folder.as_str()), Access::Write)?;
                *folder = setting_text(roots, &resolved, Access::Write)?;
            }
            // The editor or the file manager opens the last session.
            Command::AudioTracksOpenLast {} => {
                if let Some(session) = self.studio.audio_tracks_last_session() {
                    self.allowed(session, Access::Read)?;
                }
            }
            // The list names the files in the recording folder.
            Command::RecordingsList {} => {
                self.allowed(&recording_folder(&self.studio.profile().output.folder), Access::Read)?;
            }
            Command::SourceAdd { kind, settings: Some(settings), .. } => {
                if let Ok(plugin) = registry.source(kind) {
                    *settings = self.check_paths(&plugin.schema(), settings)?;
                }
            }
            Command::SourceSettings { source, settings } => {
                if let Some(plugin) = self.source_config(source).and_then(|s| registry.source(&s.kind).ok()) {
                    *settings = self.check_paths(&plugin.schema(), settings)?;
                }
            }
            Command::FilterSettings { source, filter, settings } => {
                let kind = self.source_config(source).and_then(|s| s.filters.iter().find(|f| f.id.0 == *filter)).map(|f| f.kind.as_str());
                if let Some(plugin) = kind.and_then(|k| registry.filter(k).ok()) {
                    *settings = self.check_paths(&plugin.schema(), settings)?;
                }
            }
            Command::TransitionSettings { settings } => {
                if let Ok(plugin) = registry.transition(&self.studio.collection().transition.kind) {
                    *settings = self.check_paths(&plugin.schema(), settings)?;
                }
            }
            _ => {}
        }
        Ok((cmd, held))
    }

    /// The pause media `given`, held to the read roots, as the text the setting or command is to
    /// hold: the path the roots resolved. The pause screen picks how to show a file by its name,
    /// so a name it shows that leads to a name it does not show, through a link, is an error.
    fn checked_pause_media(&self, roots: &Roots, given: &str) -> Result<String, McpError> {
        let resolved = self.allowed(Path::new(given), Access::Read)?;
        let text = setting_text(roots, &resolved, Access::Read)?;
        let shown = pause_media_extensions(self.studio.registry());
        let shows = |p: &str| Path::new(p).extension().and_then(|e| e.to_str()).is_some_and(|e| shown.contains(&e.to_ascii_lowercase()));
        if shows(given) && !shows(&text) {
            return Err(McpError::Invalid(format!("{given} leads to {text}, which is not a picture or video file the pause screen can show")));
        }
        Ok(text)
    }

    /// The recording folder held to the write roots, as the text the setting is to hold: the path
    /// the roots resolved.
    fn checked_recording_folder(&self, roots: &Roots) -> Result<String, McpError> {
        let resolved = self.allowed(&recording_folder(&self.studio.profile().output.folder), Access::Write)?;
        setting_text(roots, &resolved, Access::Write)
    }

    /// Set the recording folder to `text` when the setting holds another text, so a command writes
    /// to the folder the roots checked.
    fn hold_recording_folder(&mut self, text: String) -> Result<(), McpError> {
        if self.studio.profile().output.folder == text {
            return Ok(());
        }
        let mut profile = self.studio.profile().clone();
        profile.output.folder = text;
        self.studio.set_profile(profile)?;
        Ok(())
    }

    /// Set the pause media to `text` when the setting holds another text, so a pause opens the
    /// file the roots checked.
    fn hold_pause_media(&mut self, text: String) -> Result<(), McpError> {
        if self.studio.profile().pause.media == text {
            return Ok(());
        }
        let mut profile = self.studio.profile().clone();
        profile.pause.media = text;
        self.studio.set_profile(profile)?;
        Ok(())
    }

    fn open(&mut self, p: &Params) -> Result<Value, McpError> {
        let path = Headless::path(p)?;
        if self.options.roots.is_none() {
            self.studio.open_collection(path)?;
        } else {
            let resolved = self.allowed(path, Access::Read)?;
            let mut collection = read_collection_file(&resolved)?;
            self.check_collection(&mut collection)?;
            self.studio.load_collection(collection);
        }
        Ok(json!({"opened": true}))
    }

    fn save(&mut self, p: &Params) -> Result<Value, McpError> {
        let path = self.allowed(Headless::path(p)?, Access::Write)?;
        self.studio.save_collection(&path)?;
        Ok(json!({"saved": true}))
    }

    fn execute(&mut self, p: &Params) -> Result<Value, McpError> {
        let id = req_str(p, "command")?;
        let params = match p.get("params") {
            None | Some(Value::Null) => Value::Null,
            Some(v @ Value::Object(_)) => v.clone(),
            Some(_) => return Err(McpError::Invalid("the command's `params` must be an object".into())),
        };
        if WINDOW_ENGINE_COMMANDS.contains(&id) {
            return Err(needs_app(&format!("`{id}`")));
        }
        if !studiocraft_engine::specs().iter().any(|s| s.id == id) {
            return Err(McpError::NeedsApp(format!(
                "`{id}` is not a command the headless studio runs (command_list shows those); window commands need the app's window: \
                 {BRIDGE_HINT}"
            )));
        }
        let cmd = Command::from_parts(id, &params).map_err(|e| McpError::Studio(StudioError::Engine(EngineError::Invalid(e))))?;
        let (cmd, held) = self.guard(cmd)?;
        if let Some(text) = held.recording {
            self.hold_recording_folder(text)?;
        }
        // A pause that is refused leaves the pause media setting as it was.
        let mut pause_media_before = None;
        if let Some(text) = held.pause_media {
            pause_media_before = Some(self.studio.profile().pause.media.clone());
            self.hold_pause_media(text)?;
        }
        match self.studio.execute(cmd) {
            Ok((result, _)) => Ok(result),
            Err(e) => {
                if let Some(text) = pause_media_before {
                    self.hold_pause_media(text)?;
                }
                Err(e.into())
            }
        }
    }

    fn commands(&self, p: &Params) -> Result<Value, McpError> {
        let filter = opt_str(p, "filter")?.map(str::to_lowercase);
        let engine = self.studio.engine();
        let list = studiocraft_engine::specs()
            .iter()
            .filter(|s| filter.as_deref().is_none_or(|f| s.id.to_lowercase().contains(f) || s.label.to_lowercase().contains(f)))
            .map(|s| {
                let enabled = match s.id {
                    "edit.undo" => engine.undo_label().is_some(),
                    "edit.redo" => engine.redo_label().is_some(),
                    id => !WINDOW_ENGINE_COMMANDS.contains(&id),
                };
                json!({
                    "id": s.id,
                    "label": s.label,
                    "params": s.params,
                    "undo": s.undo == Undo::Undoable,
                    "scope": "engine",
                    "enabled": enabled,
                    "shortcut": Value::Null,
                })
            })
            .collect();
        Ok(Value::Array(list))
    }

    fn inspect(&self, p: &Params) -> Result<Value, McpError> {
        match opt_str(p, "detail")? {
            None | Some("summary") => Ok(summary(self.studio.state_json())),
            Some("full") => Ok(self.studio.state_json()),
            Some(other) => Err(McpError::Invalid(format!("`detail` is summary or full, not `{other}`"))),
        }
    }

    fn preview(&mut self, p: &Params) -> Result<Value, McpError> {
        let view = match opt_str(p, "view")? {
            None | Some("program") => View::Program,
            Some("preview") => View::Preview,
            Some(other) => return Err(McpError::Invalid(format!("`view` is program or preview, not `{other}`"))),
        };
        let max_side = match p.get("max_side") {
            None | Some(Value::Null) => 0,
            Some(v) => v
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .filter(|n| *n <= MAX_PNG_SIDE)
                .ok_or_else(|| McpError::Invalid(format!("`max_side` must be a whole number from 0 to {MAX_PNG_SIDE}")))?,
        };
        // A picture taken right after a scene switch or a new source would show a transition
        // half done or the source missing.
        let deadline = Instant::now() + SETTLE_WAIT;
        loop {
            self.studio.tick();
            if self.studio.view_settled(view) || Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(SETTLE_STEP);
        }
        let mut tries = 0;
        let capture = loop {
            tries += 1;
            // The latest changes reach the compositor on a tick.
            self.studio.tick();
            match self.studio.capture(view, CAPTURE_WAIT) {
                Err(StudioError::CaptureTimeout(_)) if tries < CAPTURE_TRIES => continue,
                other => break other?,
            }
        };
        let bytes = png(&capture, max_side)?;
        let (width, height) = scaled_size(capture.width, capture.height, max_side);
        Ok(json!({"width": width, "height": height, "png_base64": base64::engine::general_purpose::STANDARD.encode(bytes)}))
    }

    fn path(p: &Params) -> Result<&Path, McpError> {
        let path = req_str(p, "path")?;
        if path.trim().is_empty() {
            return Err(McpError::Invalid("`path` can't be empty".into()));
        }
        Ok(Path::new(path))
    }

    fn answer(&mut self, method: &str, p: &Params) -> Result<Value, McpError> {
        match method {
            "engine.execute" | "run" => self.execute(p),
            "engine.commands" | "commands" => self.commands(p),
            "document.inspect" => self.inspect(p),
            "state" => Ok(self.studio.state_json()),
            "render.preview" => self.preview(p),
            "app.links" | "app.info" => Ok(self.studio.run(method, &Value::Object(p.clone()))?.0),
            "collection.open" => self.open(p),
            "collection.save" => self.save(p),
            "app.quit" => Err(needs_app("`app.quit`")),
            m if m.starts_with("ui.") => Err(needs_app(&format!("`{m}`"))),
            other => Err(McpError::Invalid(format!(
                "unknown method `{other}`; use engine.execute, engine.commands, document.inspect, render.preview, app.links, app.info, \
                 collection.open or collection.save"
            ))),
        }
    }
}

impl Backend for Headless {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, McpError> {
        let p = object(params)?;
        // Without a window, the studio's housekeeping (a transition's end, the pause screen's
        // countdown and its end at zero) runs before each call and in `idle`.
        self.studio.tick();
        let result = self.answer(method, &p);
        // Without a window nobody reads the status messages; keep them out of memory.
        for n in self.studio.take_notices() {
            log::debug!("{}", n.text());
        }
        result
    }

    fn has_ui(&self) -> bool {
        false
    }

    fn idle(&mut self) {
        self.studio.tick();
        for n in self.studio.take_notices() {
            log::debug!("{}", n.text());
        }
    }

    fn describe(&self) -> String {
        let mut text = String::from("headless (in-process studio, no window)");
        if let Some(roots) = &self.options.roots {
            text.push_str(&format!("; files are confined to {}", roots.describe()));
        }
        text.push_str(if self.options.allow_streaming { "; streaming is allowed" } else { "; streaming is off" });
        text
    }
}
