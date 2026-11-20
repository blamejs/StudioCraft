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
use studiocraft_studio::{OutputKind, Studio, StudioError, View, builtin_registry, default_collection, recording_folder};

use crate::backend::{Backend, McpError};
use crate::roots::{Access, Roots};

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
        let registry = builtin_registry(&ffmpeg).map_err(|e| McpError::Studio(e.into()))?;
        let studio = Studio::new(gpu, default_collection("Untitled"), Profile::default(), Arc::new(registry), ffmpeg);
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

    /// Every path field of `schema` set in `settings`, held to the roots.
    fn check_paths(&self, schema: &SettingsSchema, settings: &Settings) -> Result<(), McpError> {
        for field in &schema.fields {
            let FieldKind::Path { mode, .. } = &field.kind else { continue };
            let Some(path) = settings.get(&field.key).and_then(Value::as_str).filter(|p| !p.trim().is_empty()) else { continue };
            let access = if *mode == PathMode::SaveFile { Access::Write } else { Access::Read };
            self.allowed(Path::new(path), access)?;
        }
        Ok(())
    }

    /// The path settings of a source and its filters, held to the roots.
    fn check_source(&self, source: &SourceConfig) -> Result<(), McpError> {
        let registry = self.studio.registry();
        // A kind this build lacks is not run, so its settings read no file.
        if let Ok(plugin) = registry.source(&source.kind) {
            self.check_paths(&plugin.schema(), &source.settings)?;
        }
        for f in &source.filters {
            if let Ok(plugin) = registry.filter(&f.kind) {
                self.check_paths(&plugin.schema(), &f.settings)?;
            }
        }
        Ok(())
    }

    /// Every file a scene collection's sources, filters and transition name, held to the read
    /// roots.
    fn check_collection(&self, c: &SceneCollection) -> Result<(), McpError> {
        for s in &c.sources {
            self.check_source(s)?;
        }
        if let Ok(plugin) = self.studio.registry().transition(&c.transition.kind) {
            self.check_paths(&plugin.schema(), &c.transition.settings)?;
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
    /// before anything runs.
    fn guard(&self, cmd: &Command) -> Result<(), McpError> {
        let starts_stream = match cmd {
            Command::StreamStart {} => true,
            Command::StreamToggle {} => !self.running(OutputKind::Streaming),
            _ => false,
        };
        if starts_stream && !self.options.allow_streaming {
            return Err(McpError::NotAllowed(STREAMING_REFUSED.into()));
        }
        if self.options.roots.is_none() {
            return Ok(());
        }
        let registry = self.studio.registry();
        match cmd {
            Command::RecordStart {} | Command::RecordToggle {} if !self.running(OutputKind::Recording) => {
                let folder = recording_folder(&self.studio.profile().output.folder);
                self.allowed(&folder, Access::Write)?;
            }
            // A saved replay goes to the recording folder too.
            Command::ReplaySave {} => {
                let folder = recording_folder(&self.studio.profile().output.folder);
                self.allowed(&folder, Access::Write)?;
            }
            // Audio tracks of a stream go to the recording folder.
            Command::StreamStart {} | Command::StreamToggle {} if starts_stream && self.tracks_follow_stream() => {
                let folder = recording_folder(&self.studio.profile().output.folder);
                self.allowed(&folder, Access::Write)?;
            }
            // Repair rewrites the files it finds.
            Command::AudioTracksRepair { folder } => {
                self.allowed(Path::new(folder), Access::Write)?;
            }
            // The editor or the file manager opens the last session.
            Command::AudioTracksOpenLast {} => {
                if let Some(session) = self.studio.audio_tracks_last_session() {
                    self.allowed(session, Access::Read)?;
                }
            }
            Command::SourceAdd { kind, settings: Some(settings), .. } => {
                if let Ok(plugin) = registry.source(kind) {
                    self.check_paths(&plugin.schema(), settings)?;
                }
            }
            Command::SourceSettings { source, settings } => {
                if let Some(plugin) = self.source_config(source).and_then(|s| registry.source(&s.kind).ok()) {
                    self.check_paths(&plugin.schema(), settings)?;
                }
            }
            Command::FilterSettings { source, filter, settings } => {
                let kind = self.source_config(source).and_then(|s| s.filters.iter().find(|f| f.id.0 == *filter)).map(|f| f.kind.as_str());
                if let Some(plugin) = kind.and_then(|k| registry.filter(k).ok()) {
                    self.check_paths(&plugin.schema(), settings)?;
                }
            }
            Command::TransitionSettings { settings } => {
                if let Ok(plugin) = registry.transition(&self.studio.collection().transition.kind) {
                    self.check_paths(&plugin.schema(), settings)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn open(&mut self, p: &Params) -> Result<Value, McpError> {
        let path = Headless::path(p)?;
        if self.options.roots.is_none() {
            self.studio.open_collection(path)?;
        } else {
            let resolved = self.allowed(path, Access::Read)?;
            let collection = read_collection_file(&resolved)?;
            self.check_collection(&collection)?;
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
        self.guard(&cmd)?;
        let (result, _) = self.studio.execute(cmd)?;
        Ok(result)
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

    fn describe(&self) -> String {
        let mut text = String::from("headless (in-process studio, no window)");
        if let Some(roots) = &self.options.roots {
            text.push_str(&format!("; files are confined to {}", roots.describe()));
        }
        text.push_str(if self.options.allow_streaming { "; streaming is allowed" } else { "; streaming is off" });
        text
    }
}
