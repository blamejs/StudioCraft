//! The engine: the scene collection, its history, and [`Engine::execute`].

use std::sync::Arc;

use craft_app_audio::Discovery;
use serde_json::{Value, json};
use studiocraft_model::collection::MAX_TRANSITION_MS;
use studiocraft_model::staged;
use studiocraft_model::{
    FilterConfig, FilterId, ItemId, PauseAtZero, PauseSettings, Scene, SceneCollection, SceneId, SceneItem, SourceConfig, SourceId,
};
use studiocraft_plugin::{PluginError, Registry, Settings, SettingsSchema};
use thiserror::Error;

use crate::command::{CanvasSize, Command, ItemRef, MAX_SOURCE_SIDE, Move, RuntimeAction, SceneRef, SourceRef, Undo};
use crate::history::History;
use crate::links;

/// Characters a marker's name holds at most (`audio_tracks.marker` and `record.marker`).
pub const MAX_MARKER_NAME_CHARS: usize = 256;

/// An error unless `name` is absent or 1 to [`MAX_MARKER_NAME_CHARS`] characters, not only
/// spaces, with no control characters.
fn check_marker_name(name: Option<&str>) -> Result<(), EngineError> {
    match name {
        Some(n) if n.trim().is_empty() || n.chars().count() > MAX_MARKER_NAME_CHARS || n.chars().any(char::is_control) => Err(EngineError::Invalid(
            format!("a marker's name is 1 to {MAX_MARKER_NAME_CHARS} characters, not only spaces, with no control characters"),
        )),
        _ => Ok(()),
    }
}

/// Which slot a `present.*` command presents.
#[derive(Clone, Copy)]
enum Present {
    Slot(usize),
    Next,
    Previous,
}

/// Why a command did not run. Shown to the user as is.
#[derive(Debug, Error, Clone, PartialEq)]
pub enum EngineError {
    #[error("there is no scene {0}")]
    NoScene(String),
    #[error("there is no source {0}")]
    NoSource(String),
    #[error("the scene has no item {0}")]
    NoItem(String),
    #[error("the source has no filter {0}")]
    NoFilter(u64),
    #[error("the name `{0}` is already in use")]
    NameInUse(String),
    #[error("a name can't be empty or longer than {MAX_NAME} characters")]
    BadName,
    #[error("the last scene can't be removed")]
    LastScene,
    #[error("`{0}` is locked; unlock it first")]
    Locked(String),
    #[error("a {0} source can't be filtered with a {1} filter")]
    FilterMismatch(&'static str, &'static str),
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Plugin(#[from] PluginError),
}

/// Longest scene, source or filter name, in characters.
pub const MAX_NAME: usize = 256;

/// How long after an `app_audio.list` the application meters are read.
const APP_AUDIO_PEAK_TIME: std::time::Duration = std::time::Duration::from_secs(2);
/// How long after an `app_audio.list` the application list is kept current.
const APP_AUDIO_LEASE: std::time::Duration = std::time::Duration::from_secs(60);

/// What a command did.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Outcome {
    /// The scene collection changed (the runtime and the interface should refresh).
    pub changed: bool,
    /// Something for the app to do.
    pub runtime: Option<RuntimeAction>,
    /// Ids the command created, for scripts.
    pub result: Value,
}

impl Outcome {
    fn changed(result: Value) -> Self {
        Self { changed: true, runtime: None, result }
    }

    fn runtime(action: RuntimeAction) -> Self {
        Self { changed: false, runtime: Some(action), result: Value::Null }
    }
}

/// The scene collection and everything that changes it.
pub struct Engine {
    collection: SceneCollection,
    history: History,
    registry: Arc<Registry>,
    studio_mode: bool,
    /// Goes up with every change, so observers can tell when to refresh.
    revision: u64,
    /// Changes since the collection was last saved.
    dirty: bool,
    /// The running applications with audio, for `app_audio.list` and `app_audio.status`.
    app_audio: Discovery,
}

impl Engine {
    pub fn new(collection: SceneCollection, registry: Arc<Registry>) -> Self {
        let studio_mode = false;
        Self { collection, history: History::default(), registry, studio_mode, revision: 1, dirty: false, app_audio: Discovery::system() }
    }

    /// List applications through `discovery` instead of the system's.
    pub fn set_app_audio(&mut self, discovery: Discovery) {
        self.app_audio = discovery;
    }

    /// Answers at once and never waits for the system. Until the running discovery thread has
    /// scanned, the answer has `listing: true` and the last list read, if any; before any scan it
    /// has no applications.
    fn app_audio_list(&self) -> Value {
        let support = self.app_audio.support();
        let Some((list, listing, error)) = self.read_apps() else {
            return json!({"supported": false, "listing": false, "error": support.reason, "apps": []});
        };
        let apps: Vec<Value> = list
            .apps
            .iter()
            .map(|a| {
                json!({
                    "app": a.choice(),
                    "name": a.name,
                    "path": a.path,
                    "pids": a.pids,
                    "target_pid": a.target_pid,
                    "playing": a.playing,
                    "peak": self.app_audio.peak(a),
                    "within": a.within,
                })
            })
            .collect();
        json!({"supported": true, "listing": listing, "error": error, "apps": apps})
    }

    /// The newest application list, whether it is still being read, and why it could not be read;
    /// `None` when this system does not capture one application. Never waits for the system.
    fn read_apps(&self) -> Option<(Arc<craft_app_audio::AppList>, bool, Option<String>)> {
        if !self.app_audio.support().supported {
            return None;
        }
        // The list stays current for a while, and a caller that asks again within the peak time
        // sees the meters' levels.
        self.app_audio.lease(APP_AUDIO_LEASE);
        self.app_audio.read_peaks_for(APP_AUDIO_PEAK_TIME);
        // Read before a stuck thread is replaced: the replacement has not scanned yet.
        let stalled = self.app_audio.stalled();
        self.app_audio.recover();
        let (list, listing) = match self.app_audio.fresh_list(std::time::Duration::ZERO) {
            Some(list) => (list, false),
            None => (self.app_audio.snapshot(), true),
        };
        let error = if stalled || self.app_audio.stalled() { Some(craft_app_audio::LIST_STALLED.to_string()) } else { list.error.clone() };
        Some((list, listing, error))
    }

    /// The lanes of the Application Audio panel. Like `app_audio.list` it answers at once, and
    /// asking keeps the list current and the application meters read. Without application capture
    /// the lanes are the microphones and any application sources the collection holds.
    pub fn app_audio_lanes(&self) -> crate::lanes::Lanes {
        let support = self.app_audio.support();
        let empty = Arc::new(craft_app_audio::AppList::default());
        let (list, listing, error) = self.read_apps().unwrap_or((empty, false, None));
        let lanes = crate::lanes::lanes(&self.collection, &list, |a| self.app_audio.peak(a));
        let desktop_heard = crate::lanes::desktop_heard(&self.collection);
        crate::lanes::Lanes { supported: support.supported, reason: support.reason, listing, error, lanes, desktop_heard }
    }

    /// The newest level of the listed application `key` names, 0 to 1, as `app_audio.lanes`
    /// reports it in `peak`. It reads the list and the meter without allocating.
    pub fn app_audio_peak(&self, key: &craft_app_audio::AppKey) -> Option<f32> {
        let list = self.app_audio.snapshot();
        list.find(key).and_then(|a| self.app_audio.peak(a))
    }

    fn app_audio_status(&self) -> Value {
        let captures = self.app_audio.captures();
        json!({
            "support": self.app_audio.support(),
            "discovery_stalled": self.app_audio.stalled(),
            "captures": captures.len(),
            "max_captures": craft_app_audio::MAX_CAPTURES,
            "dropped_blocks": captures.iter().map(|c| c.dropped_blocks).sum::<u64>(),
            "pool_exhausted": captures.iter().map(|c| c.pool_exhausted).sum::<u64>(),
            "discontinuities": captures.iter().map(|c| c.discontinuities).sum::<u64>(),
            "running": captures,
        })
    }

    pub fn collection(&self) -> &SceneCollection {
        &self.collection
    }

    pub fn registry(&self) -> &Arc<Registry> {
        &self.registry
    }

    pub fn studio_mode(&self) -> bool {
        self.studio_mode
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn mark_saved(&mut self) {
        self.dirty = false;
    }

    /// The scene being edited: the preview scene in studio mode, else the program scene.
    pub fn edit_scene(&self) -> SceneId {
        self.collection.edit_scene(self.studio_mode)
    }

    pub fn undo_label(&self) -> Option<&'static str> {
        self.history.undo_label()
    }

    pub fn redo_label(&self) -> Option<&'static str> {
        self.history.redo_label()
    }

    /// The id of the newest undo step, which undo and redo keep.
    pub fn last_step(&self) -> Option<u64> {
        self.history.last_step()
    }

    /// Whether the undo step `step` is in effect and can still be undone.
    pub fn step_holds(&self, step: u64) -> bool {
        self.history.holds(step)
    }

    /// The undo steps newer than `step`, oldest first; `None` when `step` is not in effect.
    pub fn steps_after(&self, step: u64) -> Option<Vec<u64>> {
        self.history.steps_after(step)
    }

    /// Take back the newest undo step and drop it, so redo does not offer it. Returns whether
    /// there was one.
    pub fn discard_last_step(&mut self) -> bool {
        self.restore(|h, _| h.discard()).changed
    }

    /// End the current drag or slider gesture: the next change is its own undo step.
    pub fn seal_history(&mut self) {
        self.history.seal();
    }

    /// Replace the whole collection (a file was opened); history starts over.
    pub fn load(&mut self, collection: SceneCollection) {
        self.collection = collection;
        self.history.clear();
        self.studio_mode = false;
        self.revision += 1;
        self.dirty = false;
    }

    /// Run a command.
    pub fn execute(&mut self, cmd: Command) -> Result<Outcome, EngineError> {
        match cmd {
            Command::Undo {} => Ok(self.restore(|h, c| h.undo(c))),
            Command::Redo {} => Ok(self.restore(|h, c| h.redo(c))),
            Command::StudioToggle {} => {
                self.studio_mode = !self.studio_mode;
                self.collection.preview_scene = self.studio_mode.then_some(self.collection.program_scene);
                self.bump();
                Ok(Outcome::changed(json!({"studio_mode": self.studio_mode})))
            }
            Command::StudioTransition { kind, ms } => {
                if !self.studio_mode {
                    return Err(EngineError::Invalid("transitions to the program need studio mode".into()));
                }
                if let Some(kind) = &kind {
                    self.registry.transition(kind)?;
                }
                if ms.is_some_and(|ms| ms > MAX_TRANSITION_MS) {
                    return Err(EngineError::Invalid(format!("a transition lasts at most {MAX_TRANSITION_MS} ms")));
                }
                let target = self.edit_scene();
                let changed = target != self.collection.program_scene;
                self.collection.program_scene = target;
                if changed {
                    self.bump();
                }
                Ok(Outcome { changed, runtime: None, result: json!({"program_scene": target}) })
            }
            Command::SceneSelect { scene } => {
                let id = self.scene_id(&scene)?;
                Ok(self.select(id))
            }
            Command::SceneSelectIndex { index } => {
                let id = index
                    .checked_sub(1)
                    .and_then(|i| self.collection.scenes.get(i))
                    .map(|s| s.id)
                    .ok_or_else(|| EngineError::NoScene(format!("at position {index}")))?;
                Ok(self.select(id))
            }
            Command::RecordStart {} => Ok(Outcome::runtime(RuntimeAction::StartRecording)),
            Command::RecordStop {} => Ok(Outcome::runtime(RuntimeAction::StopRecording)),
            Command::RecordToggle {} => Ok(Outcome::runtime(RuntimeAction::ToggleRecording)),
            Command::RecordMarker { name } => {
                check_marker_name(name.as_deref())?;
                Ok(Outcome::runtime(RuntimeAction::RecordingMarker { name }))
            }
            Command::StreamStart {} => Ok(Outcome::runtime(RuntimeAction::StartStreaming)),
            Command::StreamStop {} => Ok(Outcome::runtime(RuntimeAction::StopStreaming)),
            Command::StreamToggle {} => Ok(Outcome::runtime(RuntimeAction::ToggleStreaming)),
            Command::ReplayStart {} => Ok(Outcome::runtime(RuntimeAction::StartReplay)),
            Command::ReplayStop {} => Ok(Outcome::runtime(RuntimeAction::StopReplay)),
            Command::ReplayToggle {} => Ok(Outcome::runtime(RuntimeAction::ToggleReplay)),
            Command::ReplaySave {} => Ok(Outcome::runtime(RuntimeAction::SaveReplay)),
            Command::AudioTracksStatus {} => Ok(Outcome::runtime(RuntimeAction::AudioTracksStatus)),
            Command::AudioTracksRepair { folder } => {
                if folder.trim().is_empty() {
                    return Err(EngineError::Invalid("the folder to repair is empty".into()));
                }
                Ok(Outcome::runtime(RuntimeAction::AudioTracksRepair { folder }))
            }
            Command::CaptionsStart { model, language } => {
                if model.as_deref().is_some_and(|m| m.trim().is_empty()) || language.as_deref().is_some_and(|l| l.trim().is_empty()) {
                    return Err(EngineError::Invalid("the speech model or language is empty".into()));
                }
                Ok(Outcome::runtime(RuntimeAction::CaptionsStart { model, language }))
            }
            Command::CaptionsStop {} => Ok(Outcome::runtime(RuntimeAction::CaptionsStop)),
            Command::CaptionsToggle {} => Ok(Outcome::runtime(RuntimeAction::CaptionsToggle)),
            Command::CaptionsStatus {} => Ok(Outcome::runtime(RuntimeAction::CaptionsStatus)),
            Command::CaptionsDownload { model } => {
                if model.as_deref().is_some_and(|m| m.trim().is_empty()) {
                    return Err(EngineError::Invalid("the speech model is empty".into()));
                }
                Ok(Outcome::runtime(RuntimeAction::CaptionsDownload { model }))
            }
            Command::CaptionsDownloadCancel {} => Ok(Outcome::runtime(RuntimeAction::CaptionsDownloadCancel)),
            Command::AudioTracksOpenLast {} => Ok(Outcome::runtime(RuntimeAction::AudioTracksOpenLast)),
            Command::AudioTracksMarker { name } => {
                check_marker_name(name.as_deref())?;
                Ok(Outcome::runtime(RuntimeAction::AudioTracksMarker { name }))
            }
            Command::RetestHardwareEncoders {} => Ok(Outcome::runtime(RuntimeAction::RetestHardwareEncoders)),
            Command::PauseStart { media, countdown_s, at_zero } => {
                if let Some(m) = &media {
                    PauseSettings::check_media(m).map_err(EngineError::Invalid)?;
                }
                if let Some(s) = countdown_s
                    && !PauseSettings::COUNTDOWN_S.contains(&s)
                {
                    return Err(EngineError::Invalid(format!("countdown_s is {s}; it must be from 0 to {}", PauseSettings::COUNTDOWN_S.end())));
                }
                let at_zero = match at_zero.as_deref() {
                    None => None,
                    Some(id) => Some(
                        PauseAtZero::from_id(id).ok_or_else(|| EngineError::Invalid(format!("at_zero is `{id}`; it must be `keep` or `resume`")))?,
                    ),
                };
                Ok(Outcome::runtime(RuntimeAction::PauseStart { media, countdown_s, at_zero }))
            }
            Command::PauseStop {} => Ok(Outcome::runtime(RuntimeAction::PauseStop)),
            Command::PauseToggle {} => Ok(Outcome::runtime(RuntimeAction::PauseToggle)),
            Command::PauseExtend { seconds } => {
                if seconds == 0 || seconds > *PauseSettings::COUNTDOWN_S.end() {
                    return Err(EngineError::Invalid(format!("seconds is {seconds}; it must be from 1 to {}", PauseSettings::COUNTDOWN_S.end())));
                }
                Ok(Outcome::runtime(RuntimeAction::PauseExtend { seconds }))
            }
            Command::PauseStatus {} => Ok(Outcome::runtime(RuntimeAction::PauseStatus)),
            Command::OpenSettings {} => Ok(Outcome::runtime(RuntimeAction::OpenSettings)),
            Command::Exit {} => Ok(Outcome::runtime(RuntimeAction::Exit)),
            // Queries: no history step, no revision bump, no change to the dirty flag.
            Command::AppLinks { link } => {
                let result = match link {
                    Some(key) => {
                        let url = links::get(&key)
                            .ok_or_else(|| EngineError::Invalid(format!("there is no link `{key}`; use one of {}", links::KEYS.join(", "))))?;
                        json!({"url": url})
                    }
                    None => links::all(),
                };
                Ok(Outcome { changed: false, runtime: None, result })
            }
            Command::AppInfo {} => Ok(Outcome { changed: false, runtime: None, result: links::info() }),
            Command::PluginList {} => Ok(Outcome { changed: false, runtime: None, result: self.registry.describe() }),
            Command::SourceSchema { kind, settings } => {
                let schema = self.registry.source(&kind)?.schema_for(&settings.unwrap_or_default());
                let result = serde_json::to_value(schema).map_err(|e| EngineError::Invalid(e.to_string()))?;
                Ok(Outcome { changed: false, runtime: None, result: json!({"schema": result}) })
            }
            Command::AppAudioList {} => Ok(Outcome { changed: false, runtime: None, result: self.app_audio_list() }),
            Command::AppAudioStatus {} => Ok(Outcome { changed: false, runtime: None, result: self.app_audio_status() }),
            Command::AppAudioLanes {} => {
                let result = serde_json::to_value(self.app_audio_lanes()).map_err(|e| EngineError::Invalid(e.to_string()))?;
                Ok(Outcome { changed: false, runtime: None, result })
            }
            cmd => self.edit(cmd),
        }
    }

    /// Note a change. Scene switches count too: the program scene is saved with the collection.
    fn bump(&mut self) {
        self.revision += 1;
        self.dirty = true;
    }

    fn restore(&mut self, step: impl FnOnce(&mut History, &SceneCollection) -> Option<SceneCollection>) -> Outcome {
        match step(&mut self.history, &self.collection) {
            Some(mut c) => {
                // What is on the program and in the preview is not part of undo.
                c.program_scene = if c.scene(self.collection.program_scene).is_some() { self.collection.program_scene } else { c.program_scene };
                c.preview_scene = self.collection.preview_scene.filter(|p| c.scene(*p).is_some()).or(c.preview_scene);
                // Neither is the window each Staged windows source presents.
                for s in c.sources.iter_mut().filter(|s| s.kind == staged::KIND) {
                    if let Some(n) =
                        self.collection.source(s.id).filter(|now| now.kind == staged::KIND).and_then(|now| now.settings.get(staged::PRESENTED))
                    {
                        s.settings.set(staged::PRESENTED, n.clone());
                    }
                }
                c.repair("Scene");
                self.collection = c;
                self.bump();
                Outcome::changed(Value::Null)
            }
            None => Outcome::default(),
        }
    }

    fn select(&mut self, id: SceneId) -> Outcome {
        let c = &mut self.collection;
        let changed = if self.studio_mode {
            let old = c.preview_scene.replace(id);
            old != Some(id)
        } else {
            let old = std::mem::replace(&mut c.program_scene, id);
            old != id
        };
        if changed {
            self.bump();
        }
        Outcome { changed, runtime: None, result: json!({"scene": id}) }
    }

    /// An undoable change: applied to a copy, recorded, then swapped in.
    fn edit(&mut self, cmd: Command) -> Result<Outcome, EngineError> {
        let label = cmd.spec().map_or("Edit", |s| s.label);
        let mut undoable = cmd.spec().is_none_or(|s| s.undo == Undo::Undoable);
        let mut c = self.collection.clone();
        let (result, key) = self.apply(&mut c, cmd)?;
        if c == self.collection {
            return Ok(Outcome { changed: false, runtime: None, result });
        }
        // Undo leaves the presented windows alone, so a change to them alone is no undo step.
        undoable &= !only_presented_differs(&self.collection, &c);
        let before = std::mem::replace(&mut self.collection, c);
        if undoable {
            self.history.record(label, before, key);
        }
        self.bump();
        Ok(Outcome::changed(result))
    }

    fn scene_id(&self, r: &SceneRef) -> Result<SceneId, EngineError> {
        let c = &self.collection;
        match r {
            SceneRef::Id(id) => c.scene(*id).map(|s| s.id).ok_or_else(|| EngineError::NoScene(id.to_string())),
            SceneRef::Name(n) => c.scene_by_name(n).map(|s| s.id).ok_or_else(|| EngineError::NoScene(format!("`{n}`"))),
        }
    }

    fn scene_or_edit(&self, r: &Option<SceneRef>) -> Result<SceneId, EngineError> {
        r.as_ref().map_or(Ok(self.edit_scene()), |r| self.scene_id(r))
    }

    fn source_id(&self, r: &SourceRef) -> Result<SourceId, EngineError> {
        let c = &self.collection;
        match r {
            SourceRef::Id(id) => c.source(*id).map(|s| s.id).ok_or_else(|| EngineError::NoSource(id.to_string())),
            SourceRef::Name(n) => c.source_by_name(n).map(|s| s.id).ok_or_else(|| EngineError::NoSource(format!("`{n}`"))),
        }
    }

    fn item_id(&self, scene: SceneId, r: &ItemRef) -> Result<ItemId, EngineError> {
        let s = self.collection.scene(scene).ok_or_else(|| EngineError::NoScene(scene.to_string()))?;
        match r {
            ItemRef::Id(id) => s.item(*id).map(|i| i.id).ok_or_else(|| EngineError::NoItem(id.to_string())),
            ItemRef::Source(name) => {
                let src = self.collection.source_by_name(name).ok_or_else(|| EngineError::NoSource(format!("`{name}`")))?;
                s.items.iter().rev().find(|i| i.source == src.id).map(|i| i.id).ok_or_else(|| EngineError::NoItem(format!("showing `{name}`")))
            }
        }
    }

    /// Change the presented slot of the Staged windows source `source`, or of every one when it is
    /// `None`. A source whose slot is empty is left as it is; it is an error when none changes.
    fn present(&self, c: &mut SceneCollection, source: Option<&SourceRef>, how: Present) -> Result<(Value, Option<String>), EngineError> {
        let targets: Vec<SourceId> = match source {
            Some(r) => {
                let id = self.source_id(r)?;
                match c.source(id) {
                    Some(s) if s.kind == staged::KIND => vec![id],
                    Some(s) => return Err(EngineError::Invalid(format!("`{}` is not a Staged windows source", s.name))),
                    None => return Err(EngineError::NoSource(id.to_string())),
                }
            }
            None => c.sources.iter().filter(|s| s.kind == staged::KIND).map(|s| s.id).collect(),
        };
        if targets.is_empty() {
            return Err(EngineError::Invalid("there is no Staged windows source".into()));
        }
        let mut presented = Vec::new();
        let mut found = false;
        for id in targets {
            let Some(s) = c.source_mut(id) else { continue };
            let slot = match how {
                Present::Slot(n) => staged::staged(&s.settings, n).map(|_| n),
                Present::Next => staged::step(&s.settings, true),
                Present::Previous => staged::step(&s.settings, false),
            };
            if let Some(n) = slot {
                found = true;
                if staged::presented(&s.settings) != n {
                    s.settings.set(staged::PRESENTED, n);
                    presented.push(json!({"source": id, "slot": n}));
                }
            }
        }
        if !found {
            return Err(EngineError::Invalid(match how {
                Present::Slot(n) => format!("no window is staged in slot {n}"),
                Present::Next | Present::Previous => "no window is staged".into(),
            }));
        }
        Ok((json!({"presented": presented}), None))
    }

    /// Apply an undoable command to `c`; returns the result and the coalescing key.
    fn apply(&self, c: &mut SceneCollection, cmd: Command) -> Result<(Value, Option<String>), EngineError> {
        match cmd {
            Command::SceneAdd { name } => {
                let name = match name {
                    Some(n) => unique_name(&n, |n| c.scene_by_name(n).is_some())?,
                    None => c.unique_scene_name("Scene"),
                };
                let id = SceneId(c.alloc_id());
                c.scenes.push(Scene { id, name, items: Vec::new() });
                if self.studio_mode {
                    c.preview_scene = Some(id);
                } else {
                    c.program_scene = id;
                }
                Ok((json!({"scene": id}), None))
            }
            Command::SceneRemove { scene } => {
                let id = self.scene_id(&scene)?;
                if c.scenes.len() <= 1 {
                    return Err(EngineError::LastScene);
                }
                let idx = c.scene_index(id).ok_or_else(|| EngineError::NoScene(id.to_string()))?;
                c.scenes.remove(idx);
                let neighbor = c.scenes.get(idx.saturating_sub(1)).or(c.scenes.first()).map(|s| s.id).unwrap_or(id);
                if c.program_scene == id {
                    c.program_scene = neighbor;
                }
                if c.preview_scene == Some(id) {
                    c.preview_scene = Some(neighbor);
                }
                drop_unused_sources(c);
                Ok((Value::Null, None))
            }
            Command::SceneRename { scene, name } => {
                let id = self.scene_id(&scene)?;
                let name = checked_name(&name)?;
                if c.scenes.iter().any(|s| s.name == name && s.id != id) {
                    return Err(EngineError::NameInUse(name));
                }
                if let Some(s) = c.scene_mut(id) {
                    s.name = name;
                }
                Ok((Value::Null, None))
            }
            Command::SceneDuplicate { scene, name } => {
                let id = self.scene_id(&scene)?;
                let src = c.scene(id).cloned().ok_or_else(|| EngineError::NoScene(id.to_string()))?;
                let name = match name {
                    Some(n) => unique_name(&n, |n| c.scene_by_name(n).is_some())?,
                    None => c.unique_scene_name(&src.name),
                };
                let new_id = SceneId(c.alloc_id());
                let items = src.items.iter().map(|i| SceneItem { id: ItemId(c.alloc_id()), ..i.clone() }).collect();
                let idx = c.scene_index(id).map_or(c.scenes.len(), |i| i + 1);
                c.scenes.insert(idx, Scene { id: new_id, name, items });
                Ok((json!({"scene": new_id}), None))
            }
            Command::SceneMove { scene, to } => {
                let id = self.scene_id(&scene)?;
                let from = c.scene_index(id).ok_or_else(|| EngineError::NoScene(id.to_string()))?;
                let last = c.scenes.len().saturating_sub(1);
                let dest = match to {
                    Move::Up => from.saturating_sub(1),
                    Move::Down => (from + 1).min(last),
                    Move::Top => 0,
                    Move::Bottom => last,
                    Move::To(i) => i.min(last),
                };
                let s = c.scenes.remove(from);
                c.scenes.insert(dest, s);
                Ok((Value::Null, None))
            }
            Command::SourceAdd { scene, kind, name, settings, transform } => {
                if let Some(t) = &transform {
                    t.check().map_err(EngineError::Invalid)?;
                }
                let scene = self.scene_or_edit(&scene)?;
                let plugin = self.registry.source(&kind)?;
                let name = match name {
                    Some(n) => unique_name(&n, |n| c.source_by_name(n).is_some())?,
                    None => c.unique_source_name(plugin.name()),
                };
                let settings = settings.unwrap_or_default();
                let schema = plugin.schema_for(&settings);
                check_settings(&schema, &settings, None)?;
                let settings = schema.normalize(&settings);
                let source = SourceId(c.alloc_id());
                c.sources.push(SourceConfig {
                    id: source,
                    kind,
                    kind_version: plugin.version(),
                    name,
                    settings,
                    filters: Vec::new(),
                    audio: Default::default(),
                });
                let item = ItemId(c.alloc_id());
                let s = c.scene_mut(scene).ok_or_else(|| EngineError::NoScene(scene.to_string()))?;
                s.items.push(SceneItem {
                    id: item,
                    source,
                    transform: transform.unwrap_or_default().sanitized(),
                    visible: true,
                    locked: false,
                    fit: None,
                });
                Ok((json!({"source": source, "item": item, "scene": scene}), None))
            }
            Command::SourceAddExisting { scene, source } => {
                let scene = self.scene_or_edit(&scene)?;
                let source = self.source_id(&source)?;
                let item = ItemId(c.alloc_id());
                let s = c.scene_mut(scene).ok_or_else(|| EngineError::NoScene(scene.to_string()))?;
                s.items.push(SceneItem { id: item, source, transform: Default::default(), visible: true, locked: false, fit: None });
                Ok((json!({"item": item}), None))
            }
            Command::SourceRename { source, name } => {
                let id = self.source_id(&source)?;
                let name = checked_name(&name)?;
                if c.sources.iter().any(|s| s.name == name && s.id != id) {
                    return Err(EngineError::NameInUse(name));
                }
                if let Some(s) = c.source_mut(id) {
                    s.name = name;
                }
                Ok((Value::Null, None))
            }
            Command::SourceSettings { source, settings } => {
                let id = self.source_id(&source)?;
                let s = c.source_mut(id).ok_or_else(|| EngineError::NoSource(id.to_string()))?;
                // Settings of a plug-in this build lacks are kept as given.
                s.settings = match self.registry.source(&s.kind) {
                    Ok(p) => {
                        let schema = p.schema_for(&settings);
                        check_settings(&schema, &settings, Some(&s.settings))?;
                        schema.normalize(&settings)
                    }
                    Err(_) => settings,
                };
                Ok((Value::Null, Some(format!("source.settings:{id}"))))
            }
            Command::PresentWindow { source, index } => {
                if !(1..=staged::SLOTS).contains(&index) {
                    return Err(EngineError::Invalid(format!("the slot must be a number from 1 to {}", staged::SLOTS)));
                }
                self.present(c, source.as_ref(), Present::Slot(index))
            }
            Command::PresentNext { source } => self.present(c, source.as_ref(), Present::Next),
            Command::PresentPrevious { source } => self.present(c, source.as_ref(), Present::Previous),
            Command::SourceVolume { source, gain } => {
                let id = self.source_id(&source)?;
                let s = c.source_mut(id).ok_or_else(|| EngineError::NoSource(id.to_string()))?;
                s.audio.gain = if gain.is_finite() && (0.0..=1.0).contains(&gain) {
                    gain
                } else {
                    return Err(EngineError::Invalid("the volume must be a number from 0 to 1".into()));
                };
                Ok((Value::Null, Some(format!("source.volume:{id}"))))
            }
            Command::SourceSyncOffset { source, ms } => {
                let id = self.source_id(&source)?;
                let s = c.source_mut(id).ok_or_else(|| EngineError::NoSource(id.to_string()))?;
                if self.registry.source(&s.kind).is_ok_and(|p| !p.caps().audio) {
                    return Err(EngineError::Invalid(format!("`{}` has no audio", s.name)));
                }
                let max = studiocraft_model::collection::MAX_SYNC_OFFSET_MS;
                s.audio.sync_offset_ms = match u32::try_from(ms) {
                    Ok(ms) if ms <= max => ms,
                    _ => return Err(EngineError::Invalid(format!("the sync offset must be a whole number of milliseconds from 0 to {max}"))),
                };
                Ok((Value::Null, Some(format!("source.sync_offset:{id}"))))
            }
            Command::SourceMonitor { source, mode } => {
                use studiocraft_model::MonitorMode;
                let id = self.source_id(&source)?;
                let s = c.source_mut(id).ok_or_else(|| EngineError::NoSource(id.to_string()))?;
                if self.registry.source(&s.kind).is_ok_and(|p| !p.caps().audio) {
                    return Err(EngineError::Invalid(format!("`{}` has no audio", s.name)));
                }
                s.audio.monitor = MonitorMode::ALL
                    .into_iter()
                    .find(|m| m.id() == mode)
                    .ok_or_else(|| EngineError::Invalid("the monitoring mode must be off, monitor_only or monitor_and_output".into()))?;
                Ok((Value::Null, None))
            }
            Command::SourceSetRecordTrack { source, on } => {
                let id = self.source_id(&source)?;
                let s = c.source_mut(id).ok_or_else(|| EngineError::NoSource(id.to_string()))?;
                if self.registry.source(&s.kind).is_ok_and(|p| !p.caps().audio) {
                    return Err(EngineError::Invalid(format!("`{}` has no audio", s.name)));
                }
                // A source that already records or not by its monitoring keeps following it: the
                // command changes nothing and adds no undo step.
                if s.audio.record_track.is_some() || s.audio.records_track() != on {
                    s.audio.record_track = Some(on);
                }
                Ok((Value::Null, None))
            }
            Command::SourceMute { source, muted } => {
                let id = self.source_id(&source)?;
                if let Some(s) = c.source_mut(id) {
                    s.audio.muted = muted;
                }
                Ok((Value::Null, None))
            }
            Command::AppAudioRoute { app, routed } => self.route_app(c, &app, routed),
            Command::ItemRemove { scene, item } => {
                let scene = self.scene_or_edit(&scene)?;
                let item = self.item_id(scene, &item)?;
                if let Some(s) = c.scene_mut(scene) {
                    s.items.retain(|i| i.id != item);
                }
                drop_unused_sources(c);
                Ok((Value::Null, None))
            }
            Command::ItemDuplicate { scene, item } => {
                let scene = self.scene_or_edit(&scene)?;
                let item = self.item_id(scene, &item)?;
                let s = c.scene(scene).ok_or_else(|| EngineError::NoScene(scene.to_string()))?;
                let idx = s.item_index(item).ok_or_else(|| EngineError::NoItem(item.to_string()))?;
                let orig = s.items.get(idx).cloned().ok_or_else(|| EngineError::NoItem(item.to_string()))?;
                let mut source = c.source(orig.source).cloned().ok_or_else(|| EngineError::NoSource(orig.source.to_string()))?;
                source.id = SourceId(c.alloc_id());
                source.name = c.unique_source_name(&source.name);
                for f in &mut source.filters {
                    f.id = FilterId(c.alloc_id());
                }
                let new_item = SceneItem { id: ItemId(c.alloc_id()), source: source.id, ..orig };
                let new_source = source.id;
                c.sources.push(source);
                if let Some(s) = c.scene_mut(scene) {
                    s.items.insert(idx + 1, new_item.clone());
                }
                Ok((json!({"source": new_source, "item": new_item.id}), None))
            }
            Command::ItemMove { scene, item, to } => {
                let scene = self.scene_or_edit(&scene)?;
                let item = self.item_id(scene, &item)?;
                let s = c.scene_mut(scene).ok_or_else(|| EngineError::NoScene(scene.to_string()))?;
                let from = s.item_index(item).ok_or_else(|| EngineError::NoItem(item.to_string()))?;
                // The Sources list shows the top item first: list row `r` is index `last - r`.
                let last = s.items.len().saturating_sub(1);
                let dest = match to {
                    Move::Up => (from + 1).min(last),
                    Move::Down => from.saturating_sub(1),
                    Move::Top => last,
                    Move::Bottom => 0,
                    Move::To(row) => last.saturating_sub(row.min(last)),
                };
                let it = s.items.remove(from);
                s.items.insert(dest, it);
                Ok((Value::Null, None))
            }
            Command::ItemTransform { scene, item, transform, fit } => {
                transform.check().map_err(EngineError::Invalid)?;
                let scene = self.scene_or_edit(&scene)?;
                let item = self.item_id(scene, &item)?;
                let name = item_name(c, scene, item);
                let it = c.scene_mut(scene).and_then(|s| s.item_mut(item)).ok_or_else(|| EngineError::NoItem(item.to_string()))?;
                if it.locked {
                    return Err(EngineError::Locked(name));
                }
                it.transform = transform.sanitized();
                it.fit = fit;
                Ok((Value::Null, Some(format!("item.transform:{item}"))))
            }
            Command::ItemVisible { scene, item, visible } => {
                let scene = self.scene_or_edit(&scene)?;
                let item = self.item_id(scene, &item)?;
                if let Some(it) = c.scene_mut(scene).and_then(|s| s.item_mut(item)) {
                    it.visible = visible;
                }
                Ok((Value::Null, None))
            }
            Command::ItemLock { scene, item, locked } => {
                let scene = self.scene_or_edit(&scene)?;
                let item = self.item_id(scene, &item)?;
                if let Some(it) = c.scene_mut(scene).and_then(|s| s.item_mut(item)) {
                    it.locked = locked;
                }
                Ok((Value::Null, None))
            }
            Command::CanvasRelayout { from, to, sizes, items } => {
                use studiocraft_model::profile::{MAX_SIDE, MIN_SIDE};
                let side = |s: CanvasSize| (MIN_SIDE..=MAX_SIDE).contains(&s.width) && (MIN_SIDE..=MAX_SIDE).contains(&s.height);
                if !side(from) || !side(to) {
                    return Err(EngineError::Invalid(format!("`from` and `to` are canvas sizes with sides of {MIN_SIDE} to {MAX_SIDE} pixels")));
                }
                let mut known = std::collections::BTreeMap::new();
                for s in &sizes {
                    if c.source(s.source).is_none() {
                        return Err(EngineError::NoSource(s.source.to_string()));
                    }
                    if !(1..=MAX_SOURCE_SIDE).contains(&s.width) || !(1..=MAX_SOURCE_SIDE).contains(&s.height) {
                        return Err(EngineError::Invalid(format!("a source's size has sides of 1 to {MAX_SOURCE_SIDE} pixels")));
                    }
                    if known.insert(s.source, (s.width, s.height)).is_some() {
                        return Err(EngineError::Invalid(format!("`sizes` lists source {} twice", s.source)));
                    }
                }
                let only = match items {
                    Some(ids) => {
                        let mut only = std::collections::BTreeSet::new();
                        for id in ids {
                            if !c.scenes.iter().flat_map(|s| &s.items).any(|i| i.id == id) {
                                return Err(EngineError::NoItem(id.to_string()));
                            }
                            if !only.insert(id) {
                                return Err(EngineError::Invalid(format!("`items` lists item {id} twice")));
                            }
                        }
                        Some(only)
                    }
                    None => None,
                };
                let (from, to) = ((from.width, from.height), (to.width, to.height));
                for it in c.scenes.iter_mut().flat_map(|s| s.items.iter_mut()) {
                    if only.as_ref().is_none_or(|o| o.contains(&it.id)) {
                        it.transform = it.transform.relayout(it.fit, known.get(&it.source).copied(), from, to);
                    }
                }
                Ok((Value::Null, None))
            }
            Command::FilterAdd { source, kind, name } => {
                let id = self.source_id(&source)?;
                let plugin = self.registry.filter(&kind)?;
                if let Ok(sp) = c.source(id).map(|s| s.kind.clone()).ok_or(()).and_then(|k| self.registry.source(&k).map_err(|_| ())) {
                    let caps = sp.caps();
                    let fits = match plugin.target() {
                        studiocraft_plugin::FilterTarget::Video => caps.video,
                        studiocraft_plugin::FilterTarget::Audio => caps.audio,
                    };
                    if !fits {
                        let (has, wants) = if caps.video { ("video", "audio") } else { ("audio", "video") };
                        return Err(EngineError::FilterMismatch(has, wants));
                    }
                }
                let fid = FilterId(c.alloc_id());
                let s = c.source_mut(id).ok_or_else(|| EngineError::NoSource(id.to_string()))?;
                let name = match name {
                    Some(n) => unique_name(&n, |n| s.filters.iter().any(|f| f.name == n))?,
                    None => unique_name(plugin.name(), |n| s.filters.iter().any(|f| f.name == n)).unwrap_or_else(|_| plugin.name().into()),
                };
                s.filters.push(FilterConfig {
                    id: fid,
                    kind,
                    kind_version: plugin.version(),
                    name,
                    enabled: true,
                    settings: plugin.schema().defaults(),
                });
                Ok((json!({"filter": fid}), None))
            }
            Command::FilterRemove { source, filter } => {
                let id = self.source_id(&source)?;
                let s = c.source_mut(id).ok_or_else(|| EngineError::NoSource(id.to_string()))?;
                let before = s.filters.len();
                s.filters.retain(|f| f.id.0 != filter);
                if s.filters.len() == before {
                    return Err(EngineError::NoFilter(filter));
                }
                Ok((Value::Null, None))
            }
            Command::FilterSettings { source, filter, settings } => {
                let id = self.source_id(&source)?;
                let s = c.source_mut(id).ok_or_else(|| EngineError::NoSource(id.to_string()))?;
                let f = s.filters.iter_mut().find(|f| f.id.0 == filter).ok_or(EngineError::NoFilter(filter))?;
                f.settings = match self.registry.filter(&f.kind) {
                    Ok(p) => {
                        let schema = p.schema();
                        check_settings(&schema, &settings, Some(&f.settings))?;
                        schema.normalize(&settings)
                    }
                    Err(_) => settings,
                };
                Ok((Value::Null, Some(format!("filter.settings:{filter}"))))
            }
            Command::FilterEnable { source, filter, enabled } => {
                let id = self.source_id(&source)?;
                let s = c.source_mut(id).ok_or_else(|| EngineError::NoSource(id.to_string()))?;
                let f = s.filters.iter_mut().find(|f| f.id.0 == filter).ok_or(EngineError::NoFilter(filter))?;
                f.enabled = enabled;
                Ok((Value::Null, None))
            }
            Command::TransitionSet { kind } => {
                let p = self.registry.transition(&kind)?;
                c.transition.kind = kind;
                c.transition.settings = p.schema().normalize(&c.transition.settings);
                Ok((Value::Null, None))
            }
            Command::TransitionDuration { ms } => {
                if ms > MAX_TRANSITION_MS {
                    return Err(EngineError::Invalid(format!("a transition lasts at most {MAX_TRANSITION_MS} ms")));
                }
                c.transition.duration_ms = ms;
                Ok((Value::Null, Some("transition.duration".into())))
            }
            Command::TransitionSettings { settings } => {
                let p = self.registry.transition(&c.transition.kind)?;
                let schema = p.schema();
                check_settings(&schema, &settings, Some(&c.transition.settings))?;
                c.transition.settings = schema.normalize(&settings);
                Ok((Value::Null, Some("transition.settings".into())))
            }
            other => Err(EngineError::Invalid(format!("`{}` is not a scene collection change", other.id()))),
        }
    }

    /// `app_audio.route`. Routing adds the application's source to the global audio sources, so
    /// it is heard in every scene together with the microphone: a source the collection already
    /// has for the application, or else a new `app_audio` source named after it. Routing an
    /// application that is already routed changes nothing. Unrouting takes every source for the
    /// application out of the global audio sources and removes those no scene shows; a source a
    /// scene shows stays in that scene.
    fn route_app(&self, c: &mut SceneCollection, app: &str, routed: bool) -> Result<(Value, Option<String>), EngineError> {
        use crate::lanes::{APP_AUDIO_KIND, same_app, source_key};
        let key = craft_app_audio::AppKey::decode(app.trim())
            .ok_or_else(|| EngineError::Invalid(format!("`{app}` is not an application; app_audio.lanes lists them")))?;
        let list = self.app_audio.snapshot();
        let mut matched: Vec<(SourceId, craft_app_audio::AppKey)> =
            c.sources.iter().filter_map(|s| source_key(s).filter(|k| same_app(k, &key, &list)).map(|k| (s.id, k))).collect();
        let listed = list.find(&key).is_some();
        if !listed {
            // While sources name programs at more than one path with this file name, a source keyed
            // by the file name alone belongs to none of them: a value matches only sources with
            // exactly its key, as its lane in app_audio.lanes shows.
            if crate::lanes::shared_name(c, &key.name) {
                matched.retain(|(_, k)| *k == key);
            }
            if matched.is_empty() {
                return Err(EngineError::Invalid(format!(
                    "`{app}` is not running with audio and has no source; app_audio.lanes lists the applications"
                )));
            }
        }
        let ours: Vec<SourceId> = matched.iter().map(|(id, _)| *id).collect();
        if !routed {
            let removed: Vec<SourceId> = c.global_audio.iter().copied().filter(|g| ours.contains(g)).collect();
            c.global_audio.retain(|g| !removed.contains(g));
            // A source taken out here is removed when no scene shows it and it names this program
            // itself: the value given, the listed program at its own path, or, for a program that
            // is not listed, any of its sources. A source saved for another folder that the list
            // only matches by file name is kept.
            let shown: std::collections::BTreeSet<SourceId> = c.scenes.iter().flat_map(|s| s.items.iter().map(|i| i.source)).collect();
            let own = |k: &craft_app_audio::AppKey| !listed || *k == key || list.find(k).is_some_and(|a| k.matches(&a.key, &a.path));
            let drop: Vec<SourceId> =
                matched.iter().filter(|(id, k)| removed.contains(id) && !shown.contains(id) && own(k)).map(|(id, _)| *id).collect();
            c.sources.retain(|s| !drop.contains(&s.id));
            return Ok((json!({"sources": removed}), None));
        }
        let in_program = |c: &SceneCollection, id: &SourceId| c.source(*id).is_some_and(|s| s.audio.monitor.in_program());
        let heard = crate::lanes::heard(c);
        // A capture of one listed application can record another one's processes (`within`).
        // Routing either while the program hears the other would put that audio in the program
        // twice. The check is skipped only when routing changes nothing.
        if let Some(me) = list.find(&key)
            && !ours.iter().any(|id| c.global_audio.contains(id) && in_program(c, id))
        {
            let is_routed = |other: &craft_app_audio::AppInfo| {
                heard.iter().filter_map(|g| c.source(*g)).filter_map(source_key).any(|k| same_app(&k, &other.key, &list))
            };
            // `within` names the nearest listed application around one; its own `within` names the
            // next one out. The walk stops after as many steps as the list has applications.
            let outer = |a: &craft_app_audio::AppInfo| -> Vec<String> {
                let mut out = Vec::new();
                let mut at = a.within.clone();
                while let Some(w) = at.take()
                    && out.len() < list.apps.len()
                    && !out.contains(&w)
                {
                    at = list.apps.iter().find(|b| b.choice() == w).and_then(|b| b.within.clone());
                    out.push(w);
                }
                out
            };
            let (mine, around_me) = (me.choice(), outer(me));
            let overlap = list.apps.iter().filter(|o| !std::ptr::eq(*o, me)).find(|o| {
                let related = around_me.contains(&o.choice()) || outer(o).contains(&mine);
                related && is_routed(o)
            });
            if let Some(o) = overlap {
                // Which of the other application's heard sources are global, and which the program
                // scene shows; a source can be both.
                let other = |g: &&SourceId| c.source(**g).and_then(source_key).is_some_and(|k| same_app(&k, &o.key, &list));
                let in_scene = |g: &SourceId| c.scene(c.program_scene).is_some_and(|s| s.items.iter().any(|i| i.visible && i.source == *g));
                let routed = heard.iter().filter(other).any(|g| c.global_audio.contains(g));
                let shown = heard.iter().filter(other).any(in_scene);
                let remedy = match (routed, shown) {
                    (true, true) => format!("stop routing {} and hide its source in the program scene first", o.name),
                    (true, false) => format!("stop routing {} first", o.name),
                    _ => format!("hide {}'s source in the program scene first", o.name),
                };
                return Err(EngineError::Invalid(format!(
                    "the program already hears {}, and its capture or the capture of {} would record the other's processes; {remedy}",
                    o.name, me.name
                )));
            }
        }
        // An existing source is used: one the program already hears, then a routed one, then one
        // that allows program output. A source set to monitor only is set to monitor and output,
        // so the program hears it.
        let mut existing: Vec<SourceId> = ours.clone();
        existing.sort_by_key(|id| (!heard.contains(id), !c.global_audio.contains(id), !in_program(c, id)));
        if let Some(id) = existing.first().copied() {
            if !c.global_audio.contains(&id) {
                c.global_audio.push(id);
            }
            if let Some(s) = c.sources.iter_mut().find(|s| s.id == id)
                && !s.audio.monitor.in_program()
            {
                s.audio.monitor = studiocraft_model::MonitorMode::MonitorAndOutput;
            }
            return Ok((json!({"source": id}), None));
        }
        // A new source is made only for an application that is listed now; the source's settings
        // keep any value they are given, so they cannot tell an unknown name from a listed one.
        let Some(found) = list.find(&key) else {
            return Err(EngineError::Invalid(format!("`{app}` is not running with audio; app_audio.lanes lists the applications")));
        };
        let plugin = self.registry.source(APP_AUDIO_KIND)?;
        let settings = Settings::new().with("app", found.choice());
        let schema = plugin.schema_for(&settings);
        check_settings(&schema, &settings, None)?;
        let settings = schema.normalize(&settings);
        let name = c.unique_source_name(&checked_name(&found.name).unwrap_or_else(|_| plugin.name().to_string()));
        let id = SourceId(c.alloc_id());
        c.sources.push(SourceConfig {
            id,
            kind: APP_AUDIO_KIND.into(),
            kind_version: plugin.version(),
            name,
            settings,
            filters: Vec::new(),
            audio: Default::default(),
        });
        c.global_audio.push(id);
        Ok((json!({"source": id}), None))
    }

    /// The collection, the studio mode and the scene in each view, for scripts.
    pub fn state_json(&self) -> Value {
        json!({
            "collection": self.collection,
            "studio_mode": self.studio_mode,
            "program_scene": self.collection.program_scene,
            "preview_scene": self.edit_scene(),
            "can_undo": self.undo_label(),
            "can_redo": self.redo_label(),
        })
    }
}

fn item_name(c: &SceneCollection, scene: SceneId, item: ItemId) -> String {
    c.scene(scene).and_then(|s| s.item(item)).and_then(|i| c.source(i.source)).map_or_else(|| item.to_string(), |s| s.name.clone())
}

/// Whether `a` and `b` differ only in the window their Staged windows sources present.
fn only_presented_differs(a: &SceneCollection, b: &SceneCollection) -> bool {
    if !b.sources.iter().any(|s| s.kind == staged::KIND) {
        return false;
    }
    let unpresented = |c: &SceneCollection| {
        let mut c = c.clone();
        for s in c.sources.iter_mut().filter(|s| s.kind == staged::KIND) {
            s.settings.set(staged::PRESENTED, Value::Null);
        }
        c
    };
    unpresented(a) == unpresented(b)
}

/// Remove sources no scene places and that are not global audio sources.
fn drop_unused_sources(c: &mut SceneCollection) {
    let used: std::collections::BTreeSet<SourceId> =
        c.scenes.iter().flat_map(|s| s.items.iter().map(|i| i.source)).chain(c.global_audio.iter().copied()).collect();
    c.sources.retain(|s| used.contains(&s.id));
}

fn checked_name(name: &str) -> Result<String, EngineError> {
    let n = name.trim();
    if n.is_empty() || n.chars().count() > MAX_NAME || n.chars().any(char::is_control) {
        return Err(EngineError::BadName);
    }
    Ok(n.to_owned())
}

fn unique_name(name: &str, taken: impl Fn(&str) -> bool) -> Result<String, EngineError> {
    let n = checked_name(name)?;
    if taken(&n) { Err(EngineError::NameInUse(n)) } else { Ok(n) }
}

/// An error naming each value in `given` that `schema` does not accept. A value equal to the one
/// in `current` is not checked again, except a choice after a choice that changed, since the
/// options of a later choice can depend on an earlier one. An omitted value counts as its
/// default, which it becomes when the settings are saved.
fn check_settings(schema: &SettingsSchema, given: &Settings, current: Option<&Settings>) -> Result<(), EngineError> {
    let mut changed = Settings::default();
    let mut choice_changed = false;
    for f in &schema.fields {
        let is_choice = matches!(f.kind, studiocraft_model::FieldKind::Choice { .. });
        let value = given.get(&f.key);
        let differs = current.and_then(|c| c.get(&f.key)) != Some(value.unwrap_or(&f.default));
        if let Some(v) = value
            && (differs || (is_choice && choice_changed))
        {
            changed.set(&f.key, v.clone());
        }
        choice_changed |= is_choice && differs;
    }
    let problems = schema.problems(&changed);
    if problems.is_empty() { Ok(()) } else { Err(EngineError::Invalid(problems.join("; "))) }
}

#[cfg(test)]
mod tests;
