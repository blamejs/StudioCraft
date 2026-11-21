//! Commands: every user action, with a stable id and typed parameters.
//!
//! On the wire (control channel, MCP, hotkey bindings) a command is
//! `{"command": "<id>", "params": {...}}`; [`Command::from_parts`] reads that form. Scenes and
//! sources may be named by id or by name.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use studiocraft_model::{FitMode, ItemId, PauseAtZero, SceneId, Settings, SourceId, Transform};

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

/// A canvas size in pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanvasSize {
    pub width: u32,
    pub height: u32,
}

/// Longest side of a [`SourceSize`], in pixels.
pub const MAX_SOURCE_SIDE: u32 = 65_536;

/// The size a source shows at, in pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceSize {
    pub source: SourceId,
    pub width: u32,
    pub height: u32,
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

/// Most scenes `scene.thumbnails` draws at once, StudioCraft's own limit.
pub const MAX_THUMBNAIL_SCENES: usize = 16;
/// Fewest pictures a second `scene.thumbnails` draws of each scene (one every 10 seconds),
/// StudioCraft's own limit.
pub const MIN_THUMBNAIL_FPS: f32 = 0.1;
/// Most pictures a second `scene.thumbnails` draws of each scene, StudioCraft's own limit.
pub const MAX_THUMBNAIL_FPS: f32 = 4.0;
/// Pictures a second `scene.thumbnails` draws of each scene when `fps` is left out.
pub const DEFAULT_THUMBNAIL_FPS: f32 = 2.0;

/// Something the engine cannot do itself: the app performs it.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeAction {
    /// Draw small pictures of `scenes`, in this order, `fps` times a second each; none when the
    /// list is empty.
    SceneThumbnails {
        scenes: Vec<SceneId>,
        fps: f32,
    },
    StartRecording,
    StopRecording,
    ToggleRecording,
    /// Mark the running recording now, named `name` or numbered when it is `None`.
    RecordingMarker {
        name: Option<String>,
    },
    /// The recordings, saved replays and takes of audio tracks in the recording folder.
    RecordingsList,
    StartStreaming,
    StopStreaming,
    ToggleStreaming,
    StartReplay,
    StopReplay,
    ToggleReplay,
    SaveReplay,
    /// The state of the audio tracks.
    AudioTracksStatus,
    /// Repair the audio tracks of takes in `folder` that ended without finishing their files.
    AudioTracksRepair {
        folder: String,
    },
    /// Start live captions of the program audio with `model` and `language` (`None`: from the
    /// first Live Captions source, else the defaults).
    CaptionsStart {
        model: Option<String>,
        language: Option<String>,
    },
    CaptionsStop,
    CaptionsToggle,
    /// The state of live captions and of the speech models.
    CaptionsStatus,
    /// Download the speech model `model`.
    CaptionsDownload {
        model: Option<String>,
    },
    CaptionsDownloadCancel,
    /// Open the session of the last take of audio tracks in the audio editor.
    AudioTracksOpenLast,
    /// Mark the running take of audio tracks now, named `name` or numbered when it is `None`.
    AudioTracksMarker {
        name: Option<String>,
    },
    /// Forget the hardware encoders' test results, so the next start tests them again.
    RetestHardwareEncoders,
    /// Show the pause screen on the program, with the profile's settings and, for this pause only,
    /// each given value in place of its setting. While paused, apply the given values.
    PauseStart {
        media: Option<String>,
        countdown_s: Option<u32>,
        at_zero: Option<PauseAtZero>,
    },
    /// Show the program scene again.
    PauseStop,
    /// Pause with the profile's settings when not paused, else resume.
    PauseToggle,
    /// Add `seconds` to the countdown.
    PauseExtend {
        seconds: u32,
    },
    /// Whether the program is paused, and the countdown.
    PauseStatus,
    /// The session's scene switches, stream reconnects, pauses and resumes after the event
    /// numbered `since` (all of them when it is `None`).
    SessionEvents {
        since: Option<u64>,
    },
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
    /// Draw a small picture of each of `scenes`, in this order, `fps` times a second, for a scene
    /// list; an empty list stops drawing them. Each call replaces the last one's list.
    #[serde(rename = "scene.thumbnails")]
    SceneThumbnails {
        /// More than [`MAX_THUMBNAIL_SCENES`] is an error while reading, before a longer list is
        /// held.
        #[serde(deserialize_with = "thumbnail_scenes")]
        scenes: Vec<SceneRef>,
        #[serde(default, deserialize_with = "given_f32", skip_serializing_if = "Option::is_none")]
        fps: Option<f32>,
    },

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
    /// Whether audio tracks record the source to a file of its own.
    #[serde(rename = "source.set_record_track")]
    SourceSetRecordTrack { source: SourceRef, on: bool },
    /// Present the window staged in slot `index` (1 is the first) of a Staged windows source, or
    /// of every one that has a window in that slot when `source` is absent.
    #[serde(rename = "present.window")]
    PresentWindow {
        #[serde(default)]
        source: Option<SourceRef>,
        index: usize,
    },
    /// Present the next staged window, wrapping around, on one Staged windows source or on all.
    #[serde(rename = "present.next")]
    PresentNext {
        #[serde(default)]
        source: Option<SourceRef>,
    },
    /// Present the staged window before the presented one, wrapping around.
    #[serde(rename = "present.previous")]
    PresentPrevious {
        #[serde(default)]
        source: Option<SourceRef>,
    },

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
        /// How `transform` sized the item to the canvas; omitted for any other change.
        #[serde(default)]
        fit: Option<FitMode>,
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
    /// Place every item of every scene for a canvas that changed from `from` to `to`
    /// ([`Transform::relayout`]), or only the items in `items` when given; `sizes` gives the
    /// sizes of the sources that show a picture.
    #[serde(rename = "canvas.relayout")]
    CanvasRelayout {
        from: CanvasSize,
        to: CanvasSize,
        #[serde(default)]
        sizes: Vec<SourceSize>,
        #[serde(default)]
        items: Option<Vec<ItemId>>,
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
    /// Mark the running recording at this moment, named `name` or numbered; the timeline file
    /// written beside the recording lists the marker.
    #[serde(rename = "record.marker")]
    RecordMarker {
        /// Absent gives a numbered marker; `null` or any value that is not a string is an error.
        #[serde(default, deserialize_with = "given_string")]
        name: Option<String>,
    },
    /// List the recordings, saved replays and takes of audio tracks in the recording folder, with
    /// each recording's marker timeline and take, newest first.
    #[serde(rename = "recordings.list")]
    RecordingsList {},
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
    #[serde(rename = "audio_tracks.status")]
    AudioTracksStatus {},
    /// Repair the audio tracks of takes that ended without finishing their files: `folder` is a
    /// take's `Audio Files` folder, its `<name> Audio` folder or the recording folder holding them.
    #[serde(rename = "audio_tracks.repair")]
    AudioTracksRepair { folder: String },
    /// Start live captions of the program audio. `model` is a speech model id and `language` an
    /// ISO 639-1 code or `auto`; each one left out comes from the first Live Captions source, or
    /// else the default.
    #[serde(rename = "captions.start")]
    CaptionsStart {
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        language: Option<String>,
    },
    #[serde(rename = "captions.stop")]
    CaptionsStop {},
    #[serde(rename = "captions.toggle")]
    CaptionsToggle {},
    /// Whether live captions run, and which speech models are downloaded.
    #[serde(rename = "captions.status")]
    CaptionsStatus {},
    /// Download a speech model into the models folder.
    #[serde(rename = "captions.download")]
    CaptionsDownload {
        #[serde(default)]
        model: Option<String>,
    },
    #[serde(rename = "captions.download_cancel")]
    CaptionsDownloadCancel {},
    /// Open the session of the last take in the audio editor, or its folder when the editor
    /// cannot be started.
    #[serde(rename = "audio_tracks.open_last")]
    AudioTracksOpenLast {},
    /// Mark the running take of audio tracks at this moment, named `name` or numbered; the
    /// take's session lists the marker.
    #[serde(rename = "audio_tracks.marker")]
    AudioTracksMarker {
        /// Absent gives a numbered marker; `null` or any value that is not a string is an error.
        #[serde(default, deserialize_with = "given_string")]
        name: Option<String>,
    },
    /// Forget the hardware encoders' test results, in the session and in the file that keeps
    /// them, so the next recording or stream tests them again.
    #[serde(rename = "hardware_encoders.retest")]
    RetestHardwareEncoders {},
    /// Show the pause screen on the program, and on every recording, stream and replay buffer.
    /// Each value given replaces its profile setting for this pause only: `media` a picture or
    /// video file (`""` for none), `countdown_s` the countdown in seconds (0 for none), `at_zero`
    /// `keep` or `resume`. While paused, the given values apply without a new transition.
    #[serde(rename = "pause.start")]
    PauseStart {
        #[serde(default, deserialize_with = "given_string", skip_serializing_if = "Option::is_none")]
        media: Option<String>,
        #[serde(default, deserialize_with = "given_u32", skip_serializing_if = "Option::is_none")]
        countdown_s: Option<u32>,
        #[serde(default, deserialize_with = "given_string", skip_serializing_if = "Option::is_none")]
        at_zero: Option<String>,
    },
    /// Show the program scene again, with the collection's transition.
    #[serde(rename = "pause.stop")]
    PauseStop {},
    #[serde(rename = "pause.toggle")]
    PauseToggle {},
    /// Add `seconds` to the countdown of the running pause.
    #[serde(rename = "pause.extend")]
    PauseExtend { seconds: u32 },
    /// Whether the program is paused, since when, and the countdown.
    #[serde(rename = "pause.status")]
    PauseStatus {},
    /// The scene switches, stream reconnects, pauses and resumes of the session, after the event
    /// numbered `since` or all the kept ones.
    #[serde(rename = "session.events")]
    SessionEvents {
        #[serde(default, deserialize_with = "given_u64", skip_serializing_if = "Option::is_none")]
        since: Option<u64>,
    },
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
    /// The settings fields of a source plug-in set up as `settings`, with the lists that depend on
    /// them (a camera's formats) read for those settings.
    #[serde(rename = "source.schema")]
    SourceSchema {
        kind: String,
        #[serde(default)]
        settings: Option<Settings>,
    },
    /// The running applications that have audio, with their peak levels.
    #[serde(rename = "app_audio.list")]
    AppAudioList {},
    /// Whether this system captures one application's audio, and the captures running now.
    #[serde(rename = "app_audio.status")]
    AppAudioStatus {},
    /// The lanes of the Application Audio panel: each microphone source and each running
    /// application with audio, with whether the program hears it.
    #[serde(rename = "app_audio.lanes")]
    AppAudioLanes {},
    /// Mix the application `app` (a value from `app_audio.lanes` or `app_audio.list`) into the
    /// program in every scene, together with the microphone, or stop doing so.
    #[serde(rename = "app_audio.route")]
    AppAudioRoute { app: String, routed: bool },
}

/// A parameter that may be left out but, when given, is a string: `null` is refused.
fn given_string<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    String::deserialize(d).map(Some)
}

/// A parameter that may be left out but, when given, is a whole number: `null` is refused.
fn given_u32<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u32>, D::Error> {
    u32::deserialize(d).map(Some)
}

/// A parameter that may be left out but, when given, is a number: `null` is refused.
fn given_f32<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<f32>, D::Error> {
    f32::deserialize(d).map(Some)
}

/// `scene.thumbnails`' list of scenes, refused once it passes [`MAX_THUMBNAIL_SCENES`] entries.
fn thumbnail_scenes<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<SceneRef>, D::Error> {
    struct Scenes;
    impl<'de> serde::de::Visitor<'de> for Scenes {
        type Value = Vec<SceneRef>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            write!(f, "a list of at most {MAX_THUMBNAIL_SCENES} scene ids or names")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut scenes = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(MAX_THUMBNAIL_SCENES));
            while let Some(scene) = seq.next_element()? {
                if scenes.len() == MAX_THUMBNAIL_SCENES {
                    return Err(serde::de::Error::custom(format_args!("more than {MAX_THUMBNAIL_SCENES} scenes are given")));
                }
                scenes.push(scene);
            }
            Ok(scenes)
        }
    }
    d.deserialize_seq(Scenes)
}

/// A parameter that may be left out but, when given, is a whole number: `null` is refused.
fn given_u64<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    u64::deserialize(d).map(Some)
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
            Command::SceneThumbnails { .. } => "scene.thumbnails",
            Command::SourceAdd { .. } => "source.add",
            Command::SourceAddExisting { .. } => "source.add_existing",
            Command::SourceRename { .. } => "source.rename",
            Command::SourceSettings { .. } => "source.settings",
            Command::SourceVolume { .. } => "source.volume",
            Command::SourceMute { .. } => "source.mute",
            Command::SourceSyncOffset { .. } => "source.sync_offset",
            Command::SourceMonitor { .. } => "source.monitor",
            Command::SourceSetRecordTrack { .. } => "source.set_record_track",
            Command::PresentWindow { .. } => "present.window",
            Command::PresentNext { .. } => "present.next",
            Command::PresentPrevious { .. } => "present.previous",
            Command::ItemRemove { .. } => "item.remove",
            Command::ItemDuplicate { .. } => "item.duplicate",
            Command::ItemMove { .. } => "item.move",
            Command::ItemTransform { .. } => "item.transform",
            Command::ItemVisible { .. } => "item.visible",
            Command::ItemLock { .. } => "item.lock",
            Command::CanvasRelayout { .. } => "canvas.relayout",
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
            Command::RecordMarker { .. } => "record.marker",
            Command::RecordingsList {} => "recordings.list",
            Command::StreamStart {} => "stream.start",
            Command::StreamStop {} => "stream.stop",
            Command::StreamToggle {} => "stream.toggle",
            Command::ReplayStart {} => "replay.start",
            Command::ReplayStop {} => "replay.stop",
            Command::ReplayToggle {} => "replay.toggle",
            Command::ReplaySave {} => "replay.save",
            Command::AudioTracksStatus {} => "audio_tracks.status",
            Command::AudioTracksRepair { .. } => "audio_tracks.repair",
            Command::CaptionsStart { .. } => "captions.start",
            Command::CaptionsStop {} => "captions.stop",
            Command::CaptionsToggle {} => "captions.toggle",
            Command::CaptionsStatus {} => "captions.status",
            Command::CaptionsDownload { .. } => "captions.download",
            Command::CaptionsDownloadCancel {} => "captions.download_cancel",
            Command::AudioTracksOpenLast {} => "audio_tracks.open_last",
            Command::AudioTracksMarker { .. } => "audio_tracks.marker",
            Command::RetestHardwareEncoders {} => "hardware_encoders.retest",
            Command::PauseStart { .. } => "pause.start",
            Command::PauseStop {} => "pause.stop",
            Command::PauseToggle {} => "pause.toggle",
            Command::PauseExtend { .. } => "pause.extend",
            Command::PauseStatus {} => "pause.status",
            Command::SessionEvents { .. } => "session.events",
            Command::OpenSettings {} => "app.settings",
            Command::Exit {} => "app.exit",
            Command::AppLinks { .. } => "app.links",
            Command::AppInfo {} => "app.info",
            Command::PluginList {} => "plugin.list",
            Command::SourceSchema { .. } => "source.schema",
            Command::AppAudioList {} => "app_audio.list",
            Command::AppAudioStatus {} => "app_audio.status",
            Command::AppAudioLanes {} => "app_audio.lanes",
            Command::AppAudioRoute { .. } => "app_audio.route",
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
    spec(
        "scene.thumbnails",
        "Draw Scene Pictures",
        "scenes: up to 16 scene ids or names, [] to stop, fps?: 0.1 to 4 pictures a second (2 by default)",
        Undo::NotUndoable,
    ),
    spec("source.add", "Add Source", "kind: plug-in id, scene?, name?, settings?, transform?", Undo::Undoable),
    spec("source.add_existing", "Add Existing Source", "source: id or name, scene?", Undo::Undoable),
    spec("source.rename", "Rename Source", "source: id or name, name: string", Undo::Undoable),
    spec("source.settings", "Source Properties", "source: id or name, settings: object", Undo::Undoable),
    spec("source.volume", "Set Volume", "source: id or name, gain: 0..1", Undo::Undoable),
    spec("source.mute", "Mute", "source: id or name, muted: bool", Undo::Undoable),
    spec("source.sync_offset", "Set Sync Offset", "source: id or name, ms: 0..5000 milliseconds the audio is delayed", Undo::Undoable),
    spec("source.monitor", "Audio Monitoring", "source: id or name, mode: off|monitor_only|monitor_and_output", Undo::Undoable),
    spec("source.set_record_track", "Record as Its Own Track", "source: id or name, on: bool", Undo::Undoable),
    spec(
        "present.window",
        "Present Staged Window",
        "index: slot 1 to 6, source?: id or name of a Staged windows source (all when absent)",
        Undo::NotUndoable,
    ),
    spec("present.next", "Present Next Staged Window", "source?: id or name of a Staged windows source (all when absent)", Undo::NotUndoable),
    spec("present.previous", "Present Previous Staged Window", "source?: id or name of a Staged windows source (all when absent)", Undo::NotUndoable),
    spec("item.remove", "Remove Source from Scene", "item: id or source name, scene?", Undo::Undoable),
    spec("item.duplicate", "Duplicate Source", "item: id or source name, scene?", Undo::Undoable),
    spec("item.move", "Move Source", "item, scene?, to: up|down|top|bottom|{to: index}", Undo::Undoable),
    spec("item.transform", "Transform Source", "item, scene?, transform: {position, size, rotation, crop}, fit?: fit|fill|stretch", Undo::Undoable),
    spec("item.visible", "Show or Hide Source", "item, scene?, visible: bool", Undo::Undoable),
    spec("item.lock", "Lock Source", "item, scene?, locked: bool", Undo::Undoable),
    spec(
        "canvas.relayout",
        "Lay Out for Canvas Size",
        "from: {width, height}, to: {width, height}, sizes?: [{source: id, width, height}], items?: [item id]",
        Undo::Undoable,
    ),
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
    spec("record.marker", "Add Recording Marker", "name?: the marker's name, 1 to 256 characters", Undo::NotUndoable),
    spec("recordings.list", "List Recordings", "", Undo::NotUndoable),
    spec("stream.start", "Start Streaming", "", Undo::NotUndoable),
    spec("stream.stop", "Stop Streaming", "", Undo::NotUndoable),
    spec("stream.toggle", "Start or Stop Streaming", "", Undo::NotUndoable),
    spec("replay.start", "Start Replay Buffer", "", Undo::NotUndoable),
    spec("replay.stop", "Stop Replay Buffer", "", Undo::NotUndoable),
    spec("replay.toggle", "Start or Stop Replay Buffer", "", Undo::NotUndoable),
    spec("replay.save", "Save Replay", "", Undo::NotUndoable),
    spec("audio_tracks.status", "Audio Tracks Status", "", Undo::NotUndoable),
    spec("audio_tracks.repair", "Repair Audio Tracks", "folder: path to a take's folder or the recording folder", Undo::NotUndoable),
    spec("captions.start", "Start Live Captions", "model?: speech model id, language?: ISO 639-1 code or auto", Undo::NotUndoable),
    spec("captions.stop", "Stop Live Captions", "", Undo::NotUndoable),
    spec("captions.toggle", "Start or Stop Live Captions", "", Undo::NotUndoable),
    spec("captions.status", "Live Captions Status", "", Undo::NotUndoable),
    spec("captions.download", "Download Speech Model", "model?: speech model id", Undo::NotUndoable),
    spec("captions.download_cancel", "Cancel Speech Model Download", "", Undo::NotUndoable),
    spec("audio_tracks.open_last", "Open Last Audio Tracks in Editor", "", Undo::NotUndoable),
    spec("audio_tracks.marker", "Add Audio Tracks Marker", "name?: the marker's name, 1 to 256 characters", Undo::NotUndoable),
    spec("hardware_encoders.retest", "Test Hardware Encoders Again", "", Undo::NotUndoable),
    spec(
        "pause.start",
        "Pause Program",
        "media?: picture or video path, \"\" for none, countdown_s?: 0..43200, at_zero?: keep|resume",
        Undo::NotUndoable,
    ),
    spec("pause.stop", "Resume Program", "", Undo::NotUndoable),
    spec("pause.toggle", "Pause or Resume Program", "", Undo::NotUndoable),
    spec("pause.extend", "Add Time to the Pause", "seconds: 1..43200", Undo::NotUndoable),
    spec("pause.status", "Pause Status", "", Undo::NotUndoable),
    spec("session.events", "Session Events", "since?: number of the last event already read", Undo::NotUndoable),
    spec("app.settings", "Settings", "", Undo::NotUndoable),
    spec("app.exit", "Exit", "", Undo::NotUndoable),
    spec("app.links", "Community Links", "link?: discord|website|github|issues", Undo::NotUndoable),
    spec("app.info", "About StudioCraft Info", "", Undo::NotUndoable),
    spec("plugin.list", "List Plug-ins", "", Undo::NotUndoable),
    spec("source.schema", "Source Settings Fields", "kind: plug-in id, settings?", Undo::NotUndoable),
    spec("app_audio.list", "List Applications with Audio", "", Undo::NotUndoable),
    spec("app_audio.status", "Application Audio Status", "", Undo::NotUndoable),
    spec("app_audio.lanes", "List Audio Lanes", "", Undo::NotUndoable),
    spec("app_audio.route", "Route Application Audio", "app: value from app_audio.lanes, routed: bool", Undo::Undoable),
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
            Command::SceneThumbnails { scenes: vec![SceneRef::Id(SceneId(2)), SceneRef::Name("Main".into())], fps: None },
            Command::SceneThumbnails { scenes: vec![], fps: Some(4.0) },
            Command::ItemMove { scene: None, item: ItemRef::Id(ItemId(3)), to: Move::To(2) },
            Command::StudioToggle {},
            Command::RecordToggle {},
            Command::RecordingsList {},
            Command::Exit {},
            Command::AppLinks { link: Some("github".into()) },
            Command::AppLinks { link: None },
            Command::AppInfo {},
            Command::PluginList {},
            Command::SourceSchema { kind: "camera".into(), settings: None },
            Command::SourceSchema { kind: "camera".into(), settings: Some(Settings::new().with("device", "Desk")) },
            Command::AppAudioList {},
            Command::AppAudioStatus {},
            Command::CaptionsStart { model: None, language: None },
            Command::CaptionsStart { model: Some("whisper-tiny".into()), language: Some("de".into()) },
            Command::CaptionsStop {},
            Command::CaptionsToggle {},
            Command::CaptionsStatus {},
            Command::CaptionsDownload { model: Some("whisper-base".into()) },
            Command::CaptionsDownloadCancel {},
            Command::AppAudioLanes {},
            Command::AppAudioRoute { app: "player.exe".into(), routed: true },
            Command::PauseStart { media: None, countdown_s: None, at_zero: None },
            Command::PauseStart { media: Some("break.png".into()), countdown_s: Some(600), at_zero: Some("resume".into()) },
            Command::PauseStop {},
            Command::PauseToggle {},
            Command::PauseExtend { seconds: 60 },
            Command::PauseStatus {},
            Command::SessionEvents { since: None },
            Command::SessionEvents { since: Some(42) },
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

    #[test]
    fn a_long_scene_list_is_refused_while_it_is_read() {
        let sixteen: Vec<u64> = (1..=16).collect();
        let c = Command::from_parts("scene.thumbnails", &json!({"scenes": sixteen})).unwrap();
        assert!(matches!(c, Command::SceneThumbnails { ref scenes, .. } if scenes.len() == 16));
        let huge: Vec<u64> = (1..=100_000).collect();
        let e = Command::from_parts("scene.thumbnails", &json!({"scenes": huge})).unwrap_err();
        assert!(e.contains("more than 16 scenes are given"), "{e}");
        assert!(Command::from_parts("scene.thumbnails", &json!({"scenes": "Main"})).is_err());
    }
}
