//! A scene collection: the scenes, the sources they show, and the transition between scenes.

use serde::{Deserialize, Serialize};

use crate::geom::{FitMode, Transform};
use crate::ids::{FilterId, ItemId, SceneId, SourceId};
use crate::settings::Settings;

/// A filter applied to a source's video or audio.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FilterConfig {
    pub id: FilterId,
    /// The filter plug-in's id.
    pub kind: String,
    /// The plug-in version the settings were written by.
    #[serde(default = "one")]
    pub kind_version: u32,
    pub name: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub settings: Settings,
}

/// Whether a source's audio plays on the monitoring device, and whether it stays in the program
/// mix that recordings and streams carry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MonitorMode {
    /// Not monitored; heard in the program mix.
    #[default]
    Off,
    /// Played on the monitoring device and left out of the program mix.
    MonitorOnly,
    /// Played on the monitoring device and heard in the program mix.
    MonitorAndOutput,
}

impl MonitorMode {
    pub const ALL: [MonitorMode; 3] = [MonitorMode::Off, MonitorMode::MonitorOnly, MonitorMode::MonitorAndOutput];

    /// The name commands and files use.
    pub fn id(self) -> &'static str {
        match self {
            MonitorMode::Off => "off",
            MonitorMode::MonitorOnly => "monitor_only",
            MonitorMode::MonitorAndOutput => "monitor_and_output",
        }
    }

    /// Whether the source plays on the monitoring device.
    pub fn monitored(self) -> bool {
        self != MonitorMode::Off
    }

    /// Whether the source is heard in the program mix.
    pub fn in_program(self) -> bool {
        self != MonitorMode::MonitorOnly
    }
}

/// Volume, mute, sync offset and monitoring of a source's audio.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioState {
    /// Linear gain, 0 to [`MAX_GAIN`] (the fader's 0 dB is 1; above 1 boosts the source).
    pub gain: f32,
    pub muted: bool,
    /// How long the mixer delays the source's audio, in milliseconds, 0 to
    /// [`MAX_SYNC_OFFSET_MS`].
    pub sync_offset_ms: u32,
    /// Whether the audio plays on the monitoring device.
    pub monitor: MonitorMode,
    /// Whether audio tracks record the source to a file of its own: `None` (the default) records
    /// it when its monitoring setting sends it to the program mix, so a monitor-only source is
    /// left out until it is set to `Some(true)`.
    pub record_track: Option<bool>,
}

/// Longest audio sync offset accepted, in milliseconds.
pub const MAX_SYNC_OFFSET_MS: u32 = 5_000;

/// Most boost a source's volume accepts, in dB above the fader's 0 dB.
pub const MAX_GAIN_DB: f32 = 20.0;

/// Highest linear gain a source's volume accepts: [`MAX_GAIN_DB`] above 0 dB.
pub const MAX_GAIN: f32 = 10.0;

impl Default for AudioState {
    fn default() -> Self {
        Self { gain: 1.0, muted: false, sync_offset_ms: 0, monitor: MonitorMode::Off, record_track: None }
    }
}

impl AudioState {
    /// Whether audio tracks record the source to its own file.
    pub fn records_track(&self) -> bool {
        self.record_track.unwrap_or(self.monitor.in_program())
    }
}

/// A source: what a plug-in produces from its settings. Scenes place it through scene items.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SourceConfig {
    pub id: SourceId,
    /// The source plug-in's id.
    pub kind: String,
    #[serde(default = "one")]
    pub kind_version: u32,
    pub name: String,
    #[serde(default)]
    pub settings: Settings,
    #[serde(default)]
    pub filters: Vec<FilterConfig>,
    #[serde(default)]
    pub audio: AudioState,
}

/// One placement of a source in a scene.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SceneItem {
    pub id: ItemId,
    pub source: SourceId,
    #[serde(default)]
    pub transform: Transform,
    #[serde(default = "yes")]
    pub visible: bool,
    #[serde(default)]
    pub locked: bool,
    /// How the item was sized to the canvas, kept until its transform is changed another way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fit: Option<FitMode>,
}

/// A scene: items drawn bottom to top (the last item is on top).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Scene {
    pub id: SceneId,
    pub name: String,
    #[serde(default)]
    pub items: Vec<SceneItem>,
}

impl Scene {
    pub fn item(&self, id: ItemId) -> Option<&SceneItem> {
        self.items.iter().find(|i| i.id == id)
    }

    pub fn item_mut(&mut self, id: ItemId) -> Option<&mut SceneItem> {
        self.items.iter_mut().find(|i| i.id == id)
    }

    pub fn item_index(&self, id: ItemId) -> Option<usize> {
        self.items.iter().position(|i| i.id == id)
    }
}

/// The scene transition used when the program scene changes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TransitionConfig {
    /// The transition plug-in's id.
    pub kind: String,
    /// Length in milliseconds (ignored by transitions that are instant).
    pub duration_ms: u32,
    #[serde(default)]
    pub settings: Settings,
}

/// Longest transition accepted, in milliseconds.
pub const MAX_TRANSITION_MS: u32 = 20_000;

impl Default for TransitionConfig {
    fn default() -> Self {
        Self { kind: "fade".into(), duration_ms: 300, settings: Settings::default() }
    }
}

/// Everything a scene collection file holds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SceneCollection {
    pub name: String,
    pub scenes: Vec<Scene>,
    pub sources: Vec<SourceConfig>,
    /// Sources that belong to no scene and are always live: desktop audio, microphones.
    #[serde(default)]
    pub global_audio: Vec<SourceId>,
    /// The scene on the program output (what is recorded and streamed).
    pub program_scene: SceneId,
    /// In studio mode, the scene being prepared in the preview.
    #[serde(default)]
    pub preview_scene: Option<SceneId>,
    #[serde(default)]
    pub transition: TransitionConfig,
    /// The next id to hand out (scene, source, item or filter).
    pub next_id: u64,
}

fn one() -> u32 {
    1
}

fn yes() -> bool {
    true
}

impl SceneCollection {
    /// An empty collection with one scene.
    pub fn new(name: &str, scene_name: &str) -> Self {
        let scene = Scene { id: SceneId(1), name: scene_name.into(), items: Vec::new() };
        Self {
            name: name.into(),
            scenes: vec![scene],
            sources: Vec::new(),
            global_audio: Vec::new(),
            program_scene: SceneId(1),
            preview_scene: None,
            transition: TransitionConfig::default(),
            next_id: 2,
        }
    }

    /// A fresh id, never handed out before in this collection.
    pub fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        id
    }

    pub fn scene(&self, id: SceneId) -> Option<&Scene> {
        self.scenes.iter().find(|s| s.id == id)
    }

    pub fn scene_mut(&mut self, id: SceneId) -> Option<&mut Scene> {
        self.scenes.iter_mut().find(|s| s.id == id)
    }

    pub fn scene_index(&self, id: SceneId) -> Option<usize> {
        self.scenes.iter().position(|s| s.id == id)
    }

    pub fn source(&self, id: SourceId) -> Option<&SourceConfig> {
        self.sources.iter().find(|s| s.id == id)
    }

    pub fn source_mut(&mut self, id: SourceId) -> Option<&mut SourceConfig> {
        self.sources.iter_mut().find(|s| s.id == id)
    }

    pub fn scene_by_name(&self, name: &str) -> Option<&Scene> {
        self.scenes.iter().find(|s| s.name == name)
    }

    pub fn source_by_name(&self, name: &str) -> Option<&SourceConfig> {
        self.sources.iter().find(|s| s.name == name)
    }

    /// The scene in the preview: the preview scene in studio mode, else the program scene.
    pub fn edit_scene(&self, studio_mode: bool) -> SceneId {
        if studio_mode { self.preview_scene.filter(|id| self.scene(*id).is_some()).unwrap_or(self.program_scene) } else { self.program_scene }
    }

    /// Whether any scene places `source`, or it is a global audio source.
    pub fn source_in_use(&self, source: SourceId) -> bool {
        self.global_audio.contains(&source) || self.scenes.iter().any(|s| s.items.iter().any(|i| i.source == source))
    }

    /// `base` if no scene has that name, else `base 2`, `base 3`, ...
    pub fn unique_scene_name(&self, base: &str) -> String {
        unique(base, |n| self.scene_by_name(n).is_some())
    }

    /// `base` if no source has that name, else `base 2`, `base 3`, ...
    pub fn unique_source_name(&self, base: &str) -> String {
        unique(base, |n| self.source_by_name(n).is_some())
    }

    /// The structural problems of the collection, one line each; empty when it is consistent.
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        let mut check_id = |id: u64, what: &str, out: &mut Vec<String>| {
            if !seen.insert(id) {
                out.push(format!("id {id} is used twice ({what})"));
            }
            if id >= self.next_id {
                out.push(format!("id {id} ({what}) is not below next_id {}", self.next_id));
            }
        };
        for s in &self.scenes {
            check_id(s.id.0, "scene", &mut out);
            for i in &s.items {
                check_id(i.id.0, "scene item", &mut out);
                if self.source(i.source).is_none() {
                    out.push(format!("scene `{}` places missing source {}", s.name, i.source));
                }
            }
        }
        for s in &self.sources {
            check_id(s.id.0, "source", &mut out);
            for f in &s.filters {
                check_id(f.id.0, "filter", &mut out);
            }
        }
        for g in &self.global_audio {
            if self.source(*g).is_none() {
                out.push(format!("global audio names missing source {g}"));
            }
        }
        if self.scenes.is_empty() {
            out.push("the collection has no scene".into());
        }
        if self.scene(self.program_scene).is_none() {
            out.push(format!("program scene {} does not exist", self.program_scene));
        }
        if self.next_id == u64::MAX {
            out.push("no new ids are left".into());
        }
        out
    }

    /// Repairs what [`SceneCollection::problems`] reports where a repair keeps the user's work:
    /// items of missing sources are dropped, a missing program scene falls back to the first
    /// scene (one is created when there is none), `next_id` moves past every id, and a source's
    /// gain outside 0 to [`MAX_GAIN`] is brought into that range.
    pub fn repair(&mut self, default_scene_name: &str) {
        let sources: std::collections::BTreeSet<SourceId> = self.sources.iter().map(|s| s.id).collect();
        for s in &mut self.scenes {
            s.items.retain(|i| sources.contains(&i.source));
        }
        self.global_audio.retain(|g| sources.contains(g));
        let max_id = self
            .scenes
            .iter()
            .flat_map(|s| std::iter::once(s.id.0).chain(s.items.iter().map(|i| i.id.0)))
            .chain(self.sources.iter().flat_map(|s| std::iter::once(s.id.0).chain(s.filters.iter().map(|f| f.id.0))))
            .max()
            .unwrap_or(0);
        let past_every_id = max_id.saturating_add(1);
        // A counter at the largest number has no id left to hand out: it goes back to just past
        // the ids in use.
        self.next_id = if self.next_id == u64::MAX { past_every_id } else { self.next_id.max(past_every_id) };
        if self.scenes.is_empty() {
            let id = SceneId(self.alloc_id());
            self.scenes.push(Scene { id, name: default_scene_name.into(), items: Vec::new() });
        }
        if self.scene(self.program_scene).is_none()
            && let Some(first) = self.scenes.first()
        {
            self.program_scene = first.id;
        }
        if self.preview_scene.is_some_and(|p| self.scene(p).is_none()) {
            self.preview_scene = None;
        }
        self.transition.duration_ms = self.transition.duration_ms.min(MAX_TRANSITION_MS);
        for s in &mut self.sources {
            s.audio.sync_offset_ms = s.audio.sync_offset_ms.min(MAX_SYNC_OFFSET_MS);
            s.audio.gain = if s.audio.gain.is_finite() { s.audio.gain.clamp(0.0, MAX_GAIN) } else { 1.0 };
        }
    }
}

fn unique(base: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(base) {
        return base.to_owned();
    }
    (2..u32::MAX).map(|n| format!("{base} {n}")).find(|n| !taken(n)).unwrap_or_else(|| base.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> SceneCollection {
        let mut c = SceneCollection::new("Untitled", "Scene");
        let src = SourceId(c.alloc_id());
        c.sources.push(SourceConfig {
            id: src,
            kind: "color".into(),
            kind_version: 1,
            name: "Color".into(),
            settings: Settings::default(),
            filters: Vec::new(),
            audio: AudioState::default(),
        });
        let item = ItemId(c.alloc_id());
        if let Some(s) = c.scene_mut(SceneId(1)) {
            s.items.push(SceneItem { id: item, source: src, transform: Transform::default(), visible: true, locked: false, fit: None });
        }
        c
    }

    #[test]
    fn fresh_collection_is_consistent() {
        assert!(sample().problems().is_empty(), "{:?}", sample().problems());
    }

    #[test]
    fn unique_names_count_up() {
        let mut c = sample();
        assert_eq!(c.unique_scene_name("Scene"), "Scene 2");
        let id = SceneId(c.alloc_id());
        c.scenes.push(Scene { id, name: "Scene 2".into(), items: vec![] });
        assert_eq!(c.unique_scene_name("Scene"), "Scene 3");
        assert_eq!(c.unique_source_name("Text"), "Text");
    }

    #[test]
    fn repair_fixes_dangling_references() {
        let mut c = sample();
        c.sources.clear();
        c.program_scene = SceneId(99);
        c.global_audio.push(SourceId(42));
        c.next_id = 0;
        assert!(!c.problems().is_empty());
        c.repair("Scene");
        assert!(c.problems().is_empty(), "{:?}", c.problems());
        assert!(c.scenes[0].items.is_empty());
    }

    #[test]
    fn repair_adds_a_scene_to_an_empty_collection() {
        let mut c = sample();
        c.scenes.clear();
        c.repair("Scene");
        assert_eq!(c.scenes.len(), 1);
        assert_eq!(c.program_scene, c.scenes[0].id);
    }

    #[test]
    fn an_exhausted_id_counter_goes_back_past_the_ids_in_use() {
        let mut c = sample();
        let past = c.next_id;
        c.next_id = u64::MAX;
        c.repair("Scene");
        assert_eq!(c.next_id, past);
        assert!(c.problems().is_empty(), "{:?}", c.problems());
        let (a, b) = (c.alloc_id(), c.alloc_id());
        assert_ne!(a, b);

        let mut full = sample();
        full.scenes[0].id = SceneId(u64::MAX - 1);
        full.program_scene = full.scenes[0].id;
        full.repair("Scene");
        assert!(full.problems().iter().any(|p| p.contains("no new ids")), "{:?}", full.problems());
    }

    #[test]
    fn the_top_gain_is_the_top_boost_in_decibels() {
        assert!((10f32.powf(MAX_GAIN_DB / 20.0) - MAX_GAIN).abs() < 1e-5);
    }
}
