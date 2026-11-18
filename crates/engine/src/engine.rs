//! The engine: the scene collection, its history, and [`Engine::execute`].

use std::sync::Arc;

use serde_json::{Value, json};
use studiocraft_model::collection::MAX_TRANSITION_MS;
use studiocraft_model::{FilterConfig, FilterId, ItemId, Scene, SceneCollection, SceneId, SceneItem, SourceConfig, SourceId};
use studiocraft_plugin::{PluginError, Registry, Settings, SettingsSchema};
use thiserror::Error;

use crate::command::{Command, ItemRef, Move, RuntimeAction, SceneRef, SourceRef, Undo};
use crate::history::History;
use crate::links;

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
}

impl Engine {
    pub fn new(collection: SceneCollection, registry: Arc<Registry>) -> Self {
        let studio_mode = false;
        Self { collection, history: History::default(), registry, studio_mode, revision: 1, dirty: false }
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
        let undoable = cmd.spec().is_none_or(|s| s.undo == Undo::Undoable);
        let mut c = self.collection.clone();
        let (result, key) = self.apply(&mut c, cmd)?;
        if c == self.collection {
            return Ok(Outcome { changed: false, runtime: None, result });
        }
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
                s.items.push(SceneItem { id: item, source, transform: transform.unwrap_or_default().sanitized(), visible: true, locked: false });
                Ok((json!({"source": source, "item": item, "scene": scene}), None))
            }
            Command::SourceAddExisting { scene, source } => {
                let scene = self.scene_or_edit(&scene)?;
                let source = self.source_id(&source)?;
                let item = ItemId(c.alloc_id());
                let s = c.scene_mut(scene).ok_or_else(|| EngineError::NoScene(scene.to_string()))?;
                s.items.push(SceneItem { id: item, source, transform: Default::default(), visible: true, locked: false });
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
            Command::ItemTransform { scene, item, transform } => {
                transform.check().map_err(EngineError::Invalid)?;
                let scene = self.scene_or_edit(&scene)?;
                let item = self.item_id(scene, &item)?;
                let name = item_name(c, scene, item);
                let it = c.scene_mut(scene).and_then(|s| s.item_mut(item)).ok_or_else(|| EngineError::NoItem(item.to_string()))?;
                if it.locked {
                    return Err(EngineError::Locked(name));
                }
                it.transform = transform.sanitized();
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
