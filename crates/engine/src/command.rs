//! Commands: every user action, with a stable id and typed parameters.
//!
//! On the wire (control channel, MCP, hotkey bindings) a command is
//! `{"command": "<id>", "params": {...}}`; [`Command::from_parts`] reads that form. Scenes and
//! sources may be named by id or by name.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use studiocraft_model::{ItemId, SceneId, Settings, SourceId, Transform};

/// A scene by id or by name.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SceneRef {
    Id(SceneId),
    Name(String),
}

/// A source by id or by name.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SourceRef {
    Id(SourceId),
    Name(String),
}

/// A scene item by id, or by the name of the source it shows (the topmost such item).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ItemRef {
    Id(ItemId),
    Source(String),
}

/// Where a reordered entry goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Move {
    Up,
    Down,
    Top,
    Bottom,
    /// To this index in the list as shown (0 is the first row).
    To(usize),
}

/// Something the engine cannot do itself: the app performs it.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeAction {
    StartRecording,
    StopRecording,
    ToggleRecording,
    StartStreaming,
    StopStreaming,
    ToggleStreaming,
    StartReplay,
    StopReplay,
    ToggleReplay,
    SaveReplay,
    OpenSettings,
    Exit,
}

/// Every action.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "command", content = "params")]
pub enum Command {
    #[serde(rename = "scene.add")]
    SceneAdd {
        #[serde(default)]
        name: Option<String>,
    },
    #[serde(rename = "scene.remove")]
    SceneRemove { scene: SceneRef },
    #[serde(rename = "scene.rename")]
    SceneRename { scene: SceneRef, name: String },
    #[serde(rename = "scene.duplicate")]
    SceneDuplicate {
        scene: SceneRef,
        #[serde(default)]
        name: Option<String>,
    },
    #[serde(rename = "scene.move")]
    SceneMove { scene: SceneRef, to: Move },
    /// Show the scene: on the program, or in the preview in studio mode.
    #[serde(rename = "scene.select")]
    SceneSelect { scene: SceneRef },
    /// Show the scene at a position in the Scenes list (1 is the first), as `scene.select`.
    #[serde(rename = "scene.select_index")]
    SceneSelectIndex { index: usize },

    #[serde(rename = "source.add")]
    SourceAdd {
        /// The scene to place it in; the scene in the preview when absent.
        #[serde(default)]
        scene: Option<SceneRef>,
        kind: String,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        settings: Option<Settings>,
        /// Initial placement; the top-left corner of the canvas at the source's own size when
        /// absent.
        #[serde(default)]
        transform: Option<Transform>,
    },
    /// Place an existing source in a scene again.
    #[serde(rename = "source.add_existing")]
    SourceAddExisting {
        #[serde(default)]
        scene: Option<SceneRef>,
        source: SourceRef,
    },
    #[serde(rename = "source.rename")]
    SourceRename { source: SourceRef, name: String },
    #[serde(rename = "source.settings")]
    SourceSettings { source: SourceRef, settings: Settings },
    #[serde(rename = "source.volume")]
    SourceVolume { source: SourceRef, gain: f32 },
    #[serde(rename = "source.mute")]
    SourceMute { source: SourceRef, muted: bool },
    /// How long the mixer delays the source's audio, in milliseconds.
    #[serde(rename = "source.sync_offset")]
    SourceSyncOffset { source: SourceRef, ms: i64 },
    /// Whether the source's audio plays on the monitoring device: `off`, `monitor_only` or
    /// `monitor_and_output`.
    #[serde(rename = "source.monitor")]
    SourceMonitor { source: SourceRef, mode: String },

    #[serde(rename = "item.remove")]
    ItemRemove {
        #[serde(default)]
        scene: Option<SceneRef>,
        item: ItemRef,
    },
    /// A copy of the item's source, placed just above it.
    #[serde(rename = "item.duplicate")]
    ItemDuplicate {
        #[serde(default)]
        scene: Option<SceneRef>,
        item: ItemRef,
    },
    #[serde(rename = "item.move")]
    ItemMove {
        #[serde(default)]
        scene: Option<SceneRef>,
        item: ItemRef,
        to: Move,
    },
    #[serde(rename = "item.transform")]
    ItemTransform {
        #[serde(default)]
        scene: Option<SceneRef>,
        item: ItemRef,
        transform: Transform,
    },
    #[serde(rename = "item.visible")]
    ItemVisible {
        #[serde(default)]
        scene: Option<SceneRef>,
        item: ItemRef,
        visible: bool,
    },
    #[serde(rename = "item.lock")]
    ItemLock {
        #[serde(default)]
        scene: Option<SceneRef>,
        item: ItemRef,
        locked: bool,
    },

    #[serde(rename = "filter.add")]
    FilterAdd {
        source: SourceRef,
        kind: String,
        #[serde(default)]
        name: Option<String>,
    },
    #[serde(rename = "filter.remove")]
    FilterRemove { source: SourceRef, filter: u64 },
    #[serde(rename = "filter.settings")]
    FilterSettings { source: SourceRef, filter: u64, settings: Settings },
    #[serde(rename = "filter.enable")]
    FilterEnable { source: SourceRef, filter: u64, enabled: bool },

    #[serde(rename = "transition.set")]
    TransitionSet { kind: String },
    #[serde(rename = "transition.duration")]
    TransitionDuration { ms: u32 },
    #[serde(rename = "transition.settings")]
    TransitionSettings { settings: Settings },

    #[serde(rename = "studio.toggle")]
    StudioToggle {},
    /// Studio mode: send the preview scene to the program, with the collection's transition or,
    /// for this switch only, the transition `kind` lasting `ms`.
    #[serde(rename = "studio.transition")]
    StudioTransition {
        #[serde(default)]
        kind: Option<String>,
        #[serde(default)]
        ms: Option<u32>,
    },

    #[serde(rename = "edit.undo")]
    Undo {},
    #[serde(rename = "edit.redo")]
    Redo {},

    #[serde(rename = "record.start")]
    RecordStart {},
    #[serde(rename = "record.stop")]
    RecordStop {},
    #[serde(rename = "record.toggle")]
    RecordToggle {},
    #[serde(rename = "stream.start")]
    StreamStart {},
    #[serde(rename = "stream.stop")]
    StreamStop {},
    #[serde(rename = "stream.toggle")]
    StreamToggle {},
    #[serde(rename = "replay.start")]
    ReplayStart {},
    #[serde(rename = "replay.stop")]
    ReplayStop {},
    #[serde(rename = "replay.toggle")]
    ReplayToggle {},
    #[serde(rename = "replay.save")]
    ReplaySave {},
    #[serde(rename = "app.settings")]
    OpenSettings {},
    #[serde(rename = "app.exit")]
    Exit {},
    #[serde(rename = "app.links")]
    AppLinks {
        #[serde(default)]
        link: Option<String>,
    },
    #[serde(rename = "app.info")]
    AppInfo {},
    #[serde(rename = "plugin.list")]
    PluginList {},
}

impl Command {
    /// A command from its id and parameters (`null` or absent parameters mean none).
    pub fn from_parts(id: &str, params: &Value) -> Result<Command, String> {
        let params = if params.is_null() { json!({}) } else { params.clone() };
        let cmd: Command = serde_json::from_value(json!({"command": id, "params": params}))
            .map_err(|e| if specs().iter().any(|s| s.id == id) { format!("`{id}`: {e}") } else { format!("unknown command `{id}`") })?;
        // Every parameter the command reads is written back out, so a given key missing there
        // was not read: usually a misspelling.
        let known = serde_json::to_value(&cmd).ok().and_then(|v| v.get("params").and_then(Value::as_object).cloned()).unwrap_or_default();
        if let Some(given) = params.as_object() {
            let unknown: Vec<&str> = given.iter().filter(|(k, v)| !v.is_null() && !known.contains_key(*k)).map(|(k, _)| k.as_str()).collect();
            if !unknown.is_empty() {
                return Err(format!("`{id}` has no parameter {}", unknown.iter().map(|k| format!("`{k}`")).collect::<Vec<_>>().join(", ")));
            }
        }
        Ok(cmd)
    }

    /// The command's stable id.
    pub fn id(&self) -> &'static str {
        match self {
            Command::SceneAdd { .. } => "scene.add",
            Command::SceneRemove { .. } => "scene.remove",
            Command::SceneRename { .. } => "scene.rename",
            Command::SceneDuplicate { .. } => "scene.duplicate",
            Command::SceneMove { .. } => "scene.move",
            Command::SceneSelect { .. } => "scene.select",
            Command::SceneSelectIndex { .. } => "scene.select_index",
            Command::SourceAdd { .. } => "source.add",
            Command::SourceAddExisting { .. } => "source.add_existing",
            Command::SourceRename { .. } => "source.rename",
            Command::SourceSettings { .. } => "source.settings",
            Command::SourceVolume { .. } => "source.volume",
            Command::SourceMute { .. } => "source.mute",
            Command::SourceSyncOffset { .. } => "source.sync_offset",
            Command::SourceMonitor { .. } => "source.monitor",
            Command::ItemRemove { .. } => "item.remove",
            Command::ItemDuplicate { .. } => "item.duplicate",
            Command::ItemMove { .. } => "item.move",
            Command::ItemTransform { .. } => "item.transform",
            Command::ItemVisible { .. } => "item.visible",
            Command::ItemLock { .. } => "item.lock",
            Command::FilterAdd { .. } => "filter.add",
            Command::FilterRemove { .. } => "filter.remove",
            Command::FilterSettings { .. } => "filter.settings",
            Command::FilterEnable { .. } => "filter.enable",
            Command::TransitionSet { .. } => "transition.set",
            Command::TransitionDuration { .. } => "transition.duration",
            Command::TransitionSettings { .. } => "transition.settings",
            Command::StudioToggle {} => "studio.toggle",
            Command::StudioTransition { .. } => "studio.transition",
            Command::Undo {} => "edit.undo",
            Command::Redo {} => "edit.redo",
            Command::RecordStart {} => "record.start",
            Command::RecordStop {} => "record.stop",
            Command::RecordToggle {} => "record.toggle",
            Command::StreamStart {} => "stream.start",
            Command::StreamStop {} => "stream.stop",
            Command::StreamToggle {} => "stream.toggle",
            Command::ReplayStart {} => "replay.start",
            Command::ReplayStop {} => "replay.stop",
            Command::ReplayToggle {} => "replay.toggle",
            Command::ReplaySave {} => "replay.save",
            Command::OpenSettings {} => "app.settings",
            Command::Exit {} => "app.exit",
            Command::AppLinks { .. } => "app.links",
            Command::AppInfo {} => "app.info",
            Command::PluginList {} => "plugin.list",
        }
    }

    /// The command's description.
    pub fn spec(&self) -> Option<&'static CommandSpec> {
        let id = self.id();
        specs().iter().find(|s| s.id == id)
    }
}

/// How undo treats a command.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Undo {
    /// Changes the scene collection and can be undone.
    Undoable,
    /// Changes what is shown or saved state that undo leaves alone (the program scene, studio
    /// mode), or acts outside the collection (recording, streaming).
    NotUndoable,
}

/// A command's description, for menus, the shortcut list and scripts.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct CommandSpec {
    pub id: &'static str,
    /// English label; the interface translates it.
    pub label: &'static str,
    /// The parameters, for scripts and the MCP tool list.
    pub params: &'static str,
    pub undo: Undo,
}

const fn spec(id: &'static str, label: &'static str, params: &'static str, undo: Undo) -> CommandSpec {
    CommandSpec { id, label, params, undo }
}

const SPECS: &[CommandSpec] = &[
    spec("scene.add", "Add Scene", "name?: string", Undo::Undoable),
    spec("scene.remove", "Remove Scene", "scene: id or name", Undo::Undoable),
    spec("scene.rename", "Rename Scene", "scene: id or name, name: string", Undo::Undoable),
    spec("scene.duplicate", "Duplicate Scene", "scene: id or name, name?: string", Undo::Undoable),
    spec("scene.move", "Move Scene", "scene: id or name, to: up|down|top|bottom|{to: index}", Undo::Undoable),
    spec("scene.select", "Show Scene", "scene: id or name", Undo::NotUndoable),
    spec("scene.select_index", "Show Scene by Position", "index: 1-based position in the Scenes list", Undo::NotUndoable),
    spec("source.add", "Add Source", "kind: plug-in id, scene?, name?, settings?, transform?", Undo::Undoable),
    spec("source.add_existing", "Add Existing Source", "source: id or name, scene?", Undo::Undoable),
    spec("source.rename", "Rename Source", "source: id or name, name: string", Undo::Undoable),
    spec("source.settings", "Source Properties", "source: id or name, settings: object", Undo::Undoable),
    spec("source.volume", "Set Volume", "source: id or name, gain: 0..1", Undo::Undoable),
    spec("source.mute", "Mute", "source: id or name, muted: bool", Undo::Undoable),
    spec("source.sync_offset", "Set Sync Offset", "source: id or name, ms: 0..5000 milliseconds the audio is delayed", Undo::Undoable),
    spec("source.monitor", "Audio Monitoring", "source: id or name, mode: off|monitor_only|monitor_and_output", Undo::Undoable),
    spec("item.remove", "Remove Source from Scene", "item: id or source name, scene?", Undo::Undoable),
    spec("item.duplicate", "Duplicate Source", "item: id or source name, scene?", Undo::Undoable),
    spec("item.move", "Move Source", "item, scene?, to: up|down|top|bottom|{to: index}", Undo::Undoable),
    spec("item.transform", "Transform Source", "item, scene?, transform: {position, size, rotation, crop}", Undo::Undoable),
    spec("item.visible", "Show or Hide Source", "item, scene?, visible: bool", Undo::Undoable),
    spec("item.lock", "Lock Source", "item, scene?, locked: bool", Undo::Undoable),
    spec("filter.add", "Add Filter", "source, kind: plug-in id, name?", Undo::Undoable),
    spec("filter.remove", "Remove Filter", "source, filter: id", Undo::Undoable),
    spec("filter.settings", "Filter Properties", "source, filter: id, settings: object", Undo::Undoable),
    spec("filter.enable", "Enable Filter", "source, filter: id, enabled: bool", Undo::Undoable),
    spec("transition.set", "Set Transition", "kind: plug-in id", Undo::Undoable),
    spec("transition.duration", "Set Transition Duration", "ms: milliseconds", Undo::Undoable),
    spec("transition.settings", "Transition Properties", "settings: object", Undo::Undoable),
    spec("studio.toggle", "Studio Mode", "", Undo::NotUndoable),
    spec("studio.transition", "Transition to Program", "kind?: transition plug-in id, ms?: milliseconds", Undo::NotUndoable),
    spec("edit.undo", "Undo", "", Undo::NotUndoable),
    spec("edit.redo", "Redo", "", Undo::NotUndoable),
    spec("record.start", "Start Recording", "", Undo::NotUndoable),
    spec("record.stop", "Stop Recording", "", Undo::NotUndoable),
    spec("record.toggle", "Start or Stop Recording", "", Undo::NotUndoable),
    spec("stream.start", "Start Streaming", "", Undo::NotUndoable),
    spec("stream.stop", "Stop Streaming", "", Undo::NotUndoable),
    spec("stream.toggle", "Start or Stop Streaming", "", Undo::NotUndoable),
    spec("replay.start", "Start Replay Buffer", "", Undo::NotUndoable),
    spec("replay.stop", "Stop Replay Buffer", "", Undo::NotUndoable),
    spec("replay.toggle", "Start or Stop Replay Buffer", "", Undo::NotUndoable),
    spec("replay.save", "Save Replay", "", Undo::NotUndoable),
    spec("app.settings", "Settings", "", Undo::NotUndoable),
    spec("app.exit", "Exit", "", Undo::NotUndoable),
    spec("app.links", "Community Links", "link?: discord|website|github|issues", Undo::NotUndoable),
    spec("app.info", "About StudioCraft Info", "", Undo::NotUndoable),
    spec("plugin.list", "List Plug-ins", "", Undo::NotUndoable),
];

/// Every command's description.
pub fn specs() -> &'static [CommandSpec] {
    SPECS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_command_has_a_spec_and_round_trips() {
        let samples = vec![
            Command::SceneAdd { name: None },
            Command::SceneSelect { scene: SceneRef::Name("Main".into()) },
            Command::ItemMove { scene: None, item: ItemRef::Id(ItemId(3)), to: Move::To(2) },
            Command::StudioToggle {},
            Command::RecordToggle {},
            Command::Exit {},
            Command::AppLinks { link: Some("github".into()) },
            Command::AppLinks { link: None },
            Command::AppInfo {},
            Command::PluginList {},
        ];
        for c in samples {
            let v = serde_json::to_value(&c).unwrap();
            assert_eq!(v["command"], c.id());
            let back = Command::from_parts(c.id(), &v["params"]).unwrap();
            assert_eq!(back, c);
            assert!(c.spec().is_some(), "{}", c.id());
        }
        let ids: std::collections::BTreeSet<_> = specs().iter().map(|s| s.id).collect();
        assert_eq!(ids.len(), specs().len(), "ids are unique");
    }

    #[test]
    fn parameters_may_be_omitted_or_named() {
        assert_eq!(Command::from_parts("edit.undo", &Value::Null).unwrap(), Command::Undo {});
        let c = Command::from_parts("scene.rename", &json!({"scene": "Main", "name": "Live"})).unwrap();
        assert_eq!(c, Command::SceneRename { scene: SceneRef::Name("Main".into()), name: "Live".into() });
        let c = Command::from_parts("scene.select", &json!({"scene": 4})).unwrap();
        assert_eq!(c, Command::SceneSelect { scene: SceneRef::Id(SceneId(4)) });
        let c = Command::from_parts("item.move", &json!({"item": "Camera", "to": "top"})).unwrap();
        assert_eq!(c, Command::ItemMove { scene: None, item: ItemRef::Source("Camera".into()), to: Move::Top });
    }

    #[test]
    fn a_misspelled_parameter_is_an_error() {
        let e = Command::from_parts("scene.add", &json!({"nmae": "Intro"})).unwrap_err();
        assert!(e.contains("`nmae`"), "{e}");
        assert!(Command::from_parts("edit.undo", &json!({"extra": 1})).is_err());
        // A null value for a key the command does not read is the same as leaving it out.
        assert_eq!(Command::from_parts("edit.undo", &json!({"extra": null})).unwrap(), Command::Undo {});
    }

    #[test]
    fn bad_input_is_an_error_not_a_crash() {
        assert!(Command::from_parts("no.such", &json!({})).unwrap_err().contains("unknown command"));
        assert!(Command::from_parts("scene.rename", &json!({"scene": 1})).unwrap_err().contains("scene.rename"));
        assert!(Command::from_parts("scene.add", &json!([1, 2])).is_err());
    }
}
