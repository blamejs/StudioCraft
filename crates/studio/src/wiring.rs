//! What the compositor draws and what the mixer mixes, derived from the scene collection.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use studiocraft_engine::host::SourceSet;
use studiocraft_engine::mixer::{MixInput, MixerCmd};
use studiocraft_media::Tick;
use studiocraft_media::mix::Levels;
use studiocraft_model::{FilterId, MonitorMode, SceneCollection, SceneId, Settings, SourceId};
use studiocraft_plugin::{AudioOut, FilterInstance, FilterTarget, Reframe, Registry};
use studiocraft_render::{ItemDesc, Plan, SceneDesc, TransitionDesc};

use crate::Notice;

/// A running video filter. Video filters run as GPU passes; the instance turns settings into
/// the pass's parameters.
struct VideoFilter {
    kind: String,
    settings: Settings,
    instance: Box<dyn FilterInstance>,
}

/// What a mixer input is made of; the mixer is rebuilt only when this changes.
#[derive(Clone, Debug, PartialEq)]
struct MixKey {
    source: SourceId,
    /// The running source's audio queue: a restarted source has a new one.
    queue: SameQueue,
    active: bool,
    filters: Vec<(FilterId, String, Settings)>,
}

/// An audio queue compared by identity. Holding it keeps the address from being reused.
#[derive(Clone)]
struct SameQueue(Arc<AudioOut>);

impl PartialEq for SameQueue {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl std::fmt::Debug for SameQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SameQueue({:p})", Arc::as_ptr(&self.0))
    }
}

/// The error notice for a filter or source named `name`.
fn problem(name: &str, e: &impl std::fmt::Display) -> Notice {
    Notice::new("{name}: {problem}", vec![("name", name.to_string()), ("problem", e.to_string())], true)
}

#[derive(Default)]
pub(crate) struct Wiring {
    video_filters: HashMap<FilterId, VideoFilter>,
    mix: Option<Vec<MixKey>>,
    gains: HashMap<SourceId, (f32, bool)>,
    offsets: HashMap<SourceId, u32>,
    modes: HashMap<SourceId, MonitorMode>,
}

impl Wiring {
    /// The compositor's plan: the program scene, the preview scene in studio mode, and the
    /// transition from `transition.0` when one runs.
    pub(crate) fn plan(
        &mut self,
        c: &SceneCollection,
        studio_mode: bool,
        registry: &Registry,
        sources: &SourceSet,
        transition: Option<(SceneId, Tick, Tick, &str, &Settings)>,
        notices: &mut Vec<Notice>,
    ) -> Plan {
        self.sync_video_filters(c, registry, notices);
        let program = self.scene(c, c.program_scene, sources);
        let preview = studio_mode.then(|| self.scene(c, c.preview_scene.unwrap_or(c.program_scene), sources));
        let transition = transition.and_then(|(from, start, duration, kind, settings)| {
            let t = registry.transition(kind).ok()?;
            Some(TransitionDesc {
                from: self.scene(c, from, sources),
                key: t.id().to_string(),
                wgsl: t.shader(),
                params: t.params(settings),
                start,
                duration,
                ease: t.easing(settings),
            })
        });
        Plan { program, transition, preview }
    }

    fn scene(&self, c: &SceneCollection, id: SceneId, sources: &SourceSet) -> SceneDesc {
        let Some(scene) = c.scene(id) else { return SceneDesc::default() };
        let items = scene
            .items
            .iter()
            .filter(|it| it.visible)
            .filter_map(|it| {
                let io = sources.io(it.source)?;
                let src = c.source(it.source)?;
                let effects =
                    src.filters.iter().filter(|f| f.enabled).filter_map(|f| self.video_filters.get(&f.id)?.instance.video_effect()).collect();
                Some(ItemDesc { source: it.source, video: Arc::clone(&io.video), transform: it.transform, effects })
            })
            .collect();
        SceneDesc { items }
    }

    /// The size a `native`-sized picture of `source` has after its enabled video filters, as the
    /// compositor draws it.
    pub(crate) fn filtered_size(&self, c: &SceneCollection, source: SourceId, native: (u32, u32)) -> (u32, u32) {
        let Some(src) = c.source(source) else { return native };
        let reframes =
            src.filters.iter().filter(|f| f.enabled).filter_map(|f| self.video_filters.get(&f.id)?.instance.video_effect()).map(|e| e.reframe);
        Reframe::chain(reframes, native)
    }

    fn sync_video_filters(&mut self, c: &SceneCollection, registry: &Registry, notices: &mut Vec<Notice>) {
        let mut seen = HashSet::new();
        for src in &c.sources {
            for f in &src.filters {
                let Ok(plugin) = registry.filter(&f.kind) else { continue };
                if plugin.target() != FilterTarget::Video {
                    continue;
                }
                seen.insert(f.id);
                match self.video_filters.get_mut(&f.id) {
                    Some(v) if v.kind == f.kind => {
                        if v.settings != f.settings {
                            if let Err(e) = v.instance.update(&f.settings) {
                                notices.push(problem(&f.name, &e));
                            }
                            v.settings = f.settings.clone();
                        }
                    }
                    _ => match plugin.create(&f.settings) {
                        Ok(instance) => {
                            self.video_filters.insert(f.id, VideoFilter { kind: f.kind.clone(), settings: f.settings.clone(), instance });
                        }
                        Err(e) => notices.push(problem(&f.name, &e)),
                    },
                }
            }
        }
        self.video_filters.retain(|id, _| seen.contains(id));
    }

    /// Mixer commands that bring it in line with the collection: a new set of inputs when the
    /// sources, their filters or the program scene changed, else only volume, mute, sync offset
    /// and monitoring changes.
    pub(crate) fn mixer_inputs(
        &mut self,
        c: &SceneCollection,
        registry: &Registry,
        sources: &SourceSet,
        levels: &mut HashMap<SourceId, Arc<Levels>>,
        notices: &mut Vec<Notice>,
    ) -> Option<Vec<MixerCmd>> {
        let heard: HashSet<SourceId> =
            c.scene(c.program_scene).map(|s| s.items.iter().filter(|i| i.visible).map(|i| i.source).collect()).unwrap_or_default();
        let audio_sources: Vec<_> = c
            .sources
            .iter()
            .filter(|s| registry.source(&s.kind).is_ok_and(|p| p.caps().audio))
            .filter_map(|s| sources.io(s.id).map(|io| (s, Arc::clone(&io.audio))))
            .collect();
        let key: Vec<MixKey> = audio_sources
            .iter()
            .map(|(s, audio)| MixKey {
                source: s.id,
                queue: SameQueue(Arc::clone(audio)),
                active: c.global_audio.contains(&s.id) || heard.contains(&s.id),
                filters: s
                    .filters
                    .iter()
                    .filter(|f| f.enabled && registry.filter(&f.kind).is_ok_and(|p| p.target() == FilterTarget::Audio))
                    .map(|f| (f.id, f.kind.clone(), f.settings.clone()))
                    .collect(),
            })
            .collect();
        let gains: HashMap<SourceId, (f32, bool)> = audio_sources.iter().map(|(s, _)| (s.id, (s.audio.gain, s.audio.muted))).collect();
        let offsets: HashMap<SourceId, u32> = audio_sources.iter().map(|(s, _)| (s.id, s.audio.sync_offset_ms)).collect();
        let modes: HashMap<SourceId, MonitorMode> = audio_sources.iter().map(|(s, _)| (s.id, s.audio.monitor)).collect();
        levels.retain(|id, _| gains.contains_key(id));

        if self.mix.as_ref() == Some(&key) {
            let mut cmds: Vec<MixerCmd> = gains
                .iter()
                .filter(|(id, g)| self.gains.get(id) != Some(g))
                .map(|(id, (gain, muted))| MixerCmd::Gain { source: *id, gain: *gain, muted: *muted })
                .collect();
            cmds.extend(
                offsets.iter().filter(|(id, ms)| self.offsets.get(id) != Some(ms)).map(|(id, ms)| MixerCmd::SyncOffset { source: *id, ms: *ms }),
            );
            cmds.extend(
                modes.iter().filter(|(id, m)| self.modes.get(id) != Some(m)).map(|(id, mode)| MixerCmd::Monitor { source: *id, mode: *mode }),
            );
            self.gains = gains;
            self.offsets = offsets;
            self.modes = modes;
            return (!cmds.is_empty()).then_some(cmds);
        }

        let mut inputs = Vec::new();
        for (k, (s, audio)) in key.iter().zip(&audio_sources) {
            let mut filters = Vec::new();
            for (_, kind, settings) in &k.filters {
                match registry.filter(kind).and_then(|p| p.create(settings)) {
                    Ok(f) => filters.push(f),
                    Err(e) => notices.push(problem(&s.name, &e)),
                }
            }
            let lv = Arc::clone(levels.entry(s.id).or_default());
            inputs.push(MixInput {
                source: s.id,
                audio: Arc::clone(audio),
                gain: s.audio.gain,
                muted: s.audio.muted,
                active: k.active,
                levels: lv,
                filters,
                sync_offset_ms: s.audio.sync_offset_ms,
                monitor: s.audio.monitor,
            });
        }
        self.mix = Some(key);
        self.gains = gains;
        self.offsets = offsets;
        self.modes = modes;
        Some(vec![MixerCmd::Inputs(inputs)])
    }
}

#[cfg(test)]
mod tests {
    use std::sync::RwLock;

    use studiocraft_media::clock::MasterClock;

    use super::*;
    use crate::{builtin_registry, default_collection};

    #[test]
    fn a_source_started_again_under_the_same_id_is_mixed_from_its_new_queue() {
        let registry = builtin_registry(&Arc::new(RwLock::new(String::new()))).unwrap();
        let mut sources = SourceSet::new(MasterClock::new());
        let (mut wiring, mut levels, mut notices) = (Wiring::default(), HashMap::new(), Vec::new());
        let mut c = default_collection("Test");
        sources.sync(&c, &registry);
        assert!(wiring.mixer_inputs(&c, &registry, &sources, &mut levels, &mut notices).is_some());
        assert!(wiring.mixer_inputs(&c, &registry, &sources, &mut levels, &mut notices).is_none(), "nothing changed");
        // The same id with another kind: the source set starts it again with a new queue.
        let first = c.sources[0].id;
        let other = c.sources[1].kind.clone();
        c.sources[0].kind = other;
        sources.sync(&c, &registry);
        let new_queue = sources.io(first).map(|io| Arc::clone(&io.audio));
        let Some(cmds) = wiring.mixer_inputs(&c, &registry, &sources, &mut levels, &mut notices) else {
            panic!("the mixer kept the stopped source's queue")
        };
        let [MixerCmd::Inputs(inputs)] = cmds.as_slice() else { panic!("{} commands", cmds.len()) };
        let input = inputs.iter().find(|i| i.source == first);
        assert!(input.zip(new_queue).is_some_and(|(i, q)| Arc::ptr_eq(&i.audio, &q)));
    }

    #[test]
    fn a_changed_sync_offset_reaches_the_mixer_without_a_new_input_list() {
        let registry = builtin_registry(&Arc::new(RwLock::new(String::new()))).unwrap();
        let mut sources = SourceSet::new(MasterClock::new());
        let (mut wiring, mut levels, mut notices) = (Wiring::default(), HashMap::new(), Vec::new());
        let mut c = default_collection("Test");
        sources.sync(&c, &registry);
        let mic = c.global_audio.first().copied().unwrap_or_else(|| panic!("the default collection has global audio"));
        c.source_mut(mic).unwrap().audio.sync_offset_ms = 120;
        let Some(cmds) = wiring.mixer_inputs(&c, &registry, &sources, &mut levels, &mut notices) else { panic!("no inputs") };
        let [MixerCmd::Inputs(inputs)] = cmds.as_slice() else { panic!("{} commands", cmds.len()) };
        assert_eq!(inputs.iter().find(|i| i.source == mic).map(|i| i.sync_offset_ms), Some(120));
        c.source_mut(mic).unwrap().audio.sync_offset_ms = 300;
        let cmds = wiring.mixer_inputs(&c, &registry, &sources, &mut levels, &mut notices).unwrap_or_default();
        assert!(matches!(cmds.as_slice(), [MixerCmd::SyncOffset { source, ms: 300 }] if *source == mic), "{} commands", cmds.len());
        assert!(wiring.mixer_inputs(&c, &registry, &sources, &mut levels, &mut notices).is_none(), "nothing changed");
    }

    #[test]
    fn a_changed_monitoring_mode_reaches_the_mixer_without_a_new_input_list() {
        let registry = builtin_registry(&Arc::new(RwLock::new(String::new()))).unwrap();
        let mut sources = SourceSet::new(MasterClock::new());
        let (mut wiring, mut levels, mut notices) = (Wiring::default(), HashMap::new(), Vec::new());
        let mut c = default_collection("Test");
        sources.sync(&c, &registry);
        let mic = c.global_audio.first().copied().unwrap_or_else(|| panic!("the default collection has global audio"));
        c.source_mut(mic).unwrap().audio.monitor = MonitorMode::MonitorOnly;
        let Some(cmds) = wiring.mixer_inputs(&c, &registry, &sources, &mut levels, &mut notices) else { panic!("no inputs") };
        let [MixerCmd::Inputs(inputs)] = cmds.as_slice() else { panic!("{} commands", cmds.len()) };
        assert_eq!(inputs.iter().find(|i| i.source == mic).map(|i| i.monitor), Some(MonitorMode::MonitorOnly));
        c.source_mut(mic).unwrap().audio.monitor = MonitorMode::MonitorAndOutput;
        let cmds = wiring.mixer_inputs(&c, &registry, &sources, &mut levels, &mut notices).unwrap_or_default();
        assert!(
            matches!(cmds.as_slice(), [MixerCmd::Monitor { source, mode: MonitorMode::MonitorAndOutput }] if *source == mic),
            "{} commands",
            cmds.len()
        );
        assert!(wiring.mixer_inputs(&c, &registry, &sources, &mut levels, &mut notices).is_none(), "nothing changed");
    }

    /// A source's size follows its enabled crop and pad filters in order, and a disabled one or
    /// one that rejected its settings changes nothing.
    #[test]
    fn the_filtered_size_follows_the_enabled_crop_and_pad_filters() {
        use studiocraft_model::FilterConfig;
        let registry = builtin_registry(&Arc::new(RwLock::new(String::new()))).unwrap();
        let mut c = default_collection("Test");
        let id = c.sources[0].id;
        let filter = |n: u64, settings: Settings| FilterConfig {
            id: FilterId(900 + n),
            kind: "crop_pad".into(),
            kind_version: 1,
            name: format!("Crop {n}"),
            enabled: true,
            settings,
        };
        c.sources[0].filters =
            vec![filter(1, Settings::new().with("left", 10).with("right", 30)), filter(2, Settings::new().with("top", -5).with("bottom", -5))];
        let (mut wiring, mut notices) = (Wiring::default(), Vec::new());
        assert_eq!(wiring.filtered_size(&c, id, (640, 360)), (640, 360), "no filter is running yet");
        wiring.sync_video_filters(&c, &registry, &mut notices);
        assert!(notices.is_empty());
        assert_eq!(wiring.filtered_size(&c, id, (640, 360)), (600, 370));
        c.sources[0].filters[1].enabled = false;
        assert_eq!(wiring.filtered_size(&c, id, (640, 360)), (600, 360));
        // Rejected settings leave the filter's last good ones.
        c.sources[0].filters[0].settings = Settings::new().with("left", 1_000_000);
        wiring.sync_video_filters(&c, &registry, &mut notices);
        assert_eq!(notices.len(), 1);
        assert_eq!(wiring.filtered_size(&c, id, (640, 360)), (600, 360));
        // Cropping everything leaves nothing; an unknown source keeps its size.
        c.sources[0].filters[0].settings = Settings::new().with("left", 640);
        wiring.sync_video_filters(&c, &registry, &mut notices);
        assert_eq!(wiring.filtered_size(&c, id, (640, 360)), (0, 0));
        assert_eq!(wiring.filtered_size(&c, SourceId(123_456), (640, 360)), (640, 360));
    }
}
