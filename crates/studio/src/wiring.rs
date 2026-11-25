//! What the compositor draws and what the mixer mixes, derived from the scene collection.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use studiocraft_engine::host::SourceSet;
use studiocraft_engine::mixer::{MixInput, MixerCmd};
use studiocraft_media::Tick;
use studiocraft_media::mix::Levels;
use studiocraft_model::{FilterId, MixTracks, MonitorMode, SceneCollection, SceneId, Settings, SourceId};
use studiocraft_plugin::{AudioOut, FilterInstance, FilterMeter, FilterTarget, Reframe, Registry};
use studiocraft_render::{ItemDesc, Plan, SceneDesc, ThumbnailRequest, TransitionDesc};

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

/// What a transition starts from: a scene of the collection, or the pause screen as drawn.
pub(crate) enum Shown {
    Scene(SceneId),
    Pause(SceneDesc),
}

#[derive(Default)]
pub(crate) struct Wiring {
    video_filters: HashMap<FilterId, VideoFilter>,
    mix: Option<Vec<MixKey>>,
    /// The gain-reduction handles of the audio filters in the mixer's inputs, by source and
    /// filter; replaced with the inputs.
    meters: HashMap<(SourceId, FilterId), FilterMeter>,
    /// The sample frames the audio filters in the mixer's inputs delay each source by, in total;
    /// replaced with the inputs. A source whose filters add no delay has no entry.
    latencies: HashMap<SourceId, usize>,
    gains: HashMap<SourceId, (f32, bool)>,
    offsets: HashMap<SourceId, u32>,
    modes: HashMap<SourceId, MonitorMode>,
    mix_tracks: HashMap<SourceId, MixTracks>,
    /// Each source's balance and whether it is turned to mono.
    channels: HashMap<SourceId, (f32, bool)>,
}

impl Wiring {
    /// The compositor's plan: the program scene, or `pause` while the program is paused, the
    /// preview scene in studio mode, and the transition from `transition.0` when one runs.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn plan(
        &mut self,
        c: &SceneCollection,
        studio_mode: bool,
        registry: &Registry,
        sources: &SourceSet,
        pause: Option<SceneDesc>,
        transition: Option<(Shown, Tick, Tick, &str, &Settings)>,
        notices: &mut Vec<Notice>,
    ) -> Plan {
        self.sync_video_filters(c, registry, notices);
        let program = pause.unwrap_or_else(|| self.scene(c, c.program_scene, sources));
        let preview = studio_mode.then(|| self.scene(c, c.preview_scene.unwrap_or(c.program_scene), sources));
        let transition = transition.and_then(|(from, start, duration, kind, settings)| {
            let t = registry.transition(kind).ok()?;
            Some(TransitionDesc {
                from: match from {
                    Shown::Scene(id) => self.scene(c, id, sources),
                    Shown::Pause(desc) => desc,
                },
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

    /// Small pictures of `scenes`, in this order, `fps` times a second, for a scene list, with the
    /// slot keys `keys`, so a slot that shows another scene is drawn again soon. Call after
    /// [`Wiring::plan`], which brings the video filters in line with the collection.
    pub(crate) fn thumbnails(&self, c: &SceneCollection, sources: &SourceSet, scenes: &[SceneId], keys: &[u64], fps: f32) -> ThumbnailRequest {
        ThumbnailRequest {
            scenes: scenes.iter().map(|&id| self.scene(c, id, sources)).collect(),
            fps,
            keys: keys.to_vec(),
            ..ThumbnailRequest::default()
        }
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
    /// sources, their filters or the program scene changed, else only volume, mute, sync offset,
    /// monitoring, mix track, balance and mono changes.
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
        let mix_tracks: HashMap<SourceId, MixTracks> = audio_sources.iter().map(|(s, _)| (s.id, s.audio.mix_tracks)).collect();
        let channels: HashMap<SourceId, (f32, bool)> = audio_sources.iter().map(|(s, _)| (s.id, (s.audio.balance, s.audio.mono))).collect();
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
            cmds.extend(
                mix_tracks
                    .iter()
                    .filter(|(id, t)| self.mix_tracks.get(id) != Some(t))
                    .map(|(id, tracks)| MixerCmd::MixTracks { source: *id, tracks: *tracks }),
            );
            cmds.extend(channels.iter().filter(|(id, c)| self.channels.get(id) != Some(c)).map(|(id, (balance, mono))| MixerCmd::Channels {
                source: *id,
                balance: *balance,
                mono: *mono,
            }));
            self.gains = gains;
            self.offsets = offsets;
            self.modes = modes;
            self.mix_tracks = mix_tracks;
            self.channels = channels;
            return (!cmds.is_empty()).then_some(cmds);
        }

        let mut inputs = Vec::new();
        let mut meters = HashMap::new();
        let mut latencies = HashMap::new();
        for (k, (s, audio)) in key.iter().zip(&audio_sources) {
            let mut filters = Vec::new();
            for (id, kind, settings) in &k.filters {
                match registry.filter(kind).and_then(|p| p.create(settings)) {
                    Ok(f) => {
                        if let Some(m) = f.meter() {
                            meters.insert((s.id, *id), m);
                        }
                        if f.latency_frames() > 0 {
                            let total: &mut usize = latencies.entry(s.id).or_default();
                            *total = total.saturating_add(f.latency_frames());
                        }
                        filters.push(f);
                    }
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
                mix_tracks: s.audio.mix_tracks,
                balance: s.audio.balance,
                mono: s.audio.mono,
            });
        }
        self.mix = Some(key);
        self.meters = meters;
        self.latencies = latencies;
        self.gains = gains;
        self.offsets = offsets;
        self.modes = modes;
        self.mix_tracks = mix_tracks;
        self.channels = channels;
        Some(vec![MixerCmd::Inputs(inputs)])
    }

    /// The gain reduction in dB that `filter` of `source` measured in its last processed block,
    /// or `None` when the mixer does not run that filter (disabled, not an audio filter, a source
    /// that is not running, settings it refused) or the filter has no meter.
    pub(crate) fn filter_meter(&self, source: SourceId, filter: FilterId) -> Option<f32> {
        self.meters.get(&(source, filter)).map(FilterMeter::get)
    }

    /// The sample frames the audio filters the mixer runs on `source` delay its audio by, in
    /// total: 0 for a source the mixer does not run or whose filters add no delay.
    pub(crate) fn filter_latency(&self, source: SourceId) -> usize {
        self.latencies.get(&source).copied().unwrap_or(0)
    }

    /// Each source whose running audio filters delay its audio, with that delay in sample frames.
    pub(crate) fn filter_latencies(&self) -> impl Iterator<Item = (SourceId, usize)> + '_ {
        self.latencies.iter().map(|(id, frames)| (*id, *frames))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::RwLock;

    use studiocraft_media::clock::MasterClock;

    use super::*;
    use crate::{CaptionBoard, builtin_registry, default_collection};

    #[test]
    fn a_source_started_again_under_the_same_id_is_mixed_from_its_new_queue() {
        let registry = builtin_registry(&Arc::new(RwLock::new(String::new())), &Arc::new(CaptionBoard::new())).unwrap();
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
        let registry = builtin_registry(&Arc::new(RwLock::new(String::new())), &Arc::new(CaptionBoard::new())).unwrap();
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
        let registry = builtin_registry(&Arc::new(RwLock::new(String::new())), &Arc::new(CaptionBoard::new())).unwrap();
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

    #[test]
    fn changed_mix_tracks_reach_the_mixer_without_a_new_input_list() {
        let registry = builtin_registry(&Arc::new(RwLock::new(String::new())), &Arc::new(CaptionBoard::new())).unwrap();
        let mut sources = SourceSet::new(MasterClock::new());
        let (mut wiring, mut levels, mut notices) = (Wiring::default(), HashMap::new(), Vec::new());
        let mut c = default_collection("Test");
        sources.sync(&c, &registry);
        let mic = c.global_audio.first().copied().unwrap_or_else(|| panic!("the default collection has global audio"));
        c.source_mut(mic).unwrap().audio.mix_tracks = MixTracks::from_bits(0b10);
        let Some(cmds) = wiring.mixer_inputs(&c, &registry, &sources, &mut levels, &mut notices) else { panic!("no inputs") };
        let [MixerCmd::Inputs(inputs)] = cmds.as_slice() else { panic!("{} commands", cmds.len()) };
        assert_eq!(inputs.iter().find(|i| i.source == mic).map(|i| i.mix_tracks), Some(MixTracks::from_bits(0b10)));
        c.source_mut(mic).unwrap().audio.mix_tracks = MixTracks::FIRST;
        let cmds = wiring.mixer_inputs(&c, &registry, &sources, &mut levels, &mut notices).unwrap_or_default();
        assert!(
            matches!(cmds.as_slice(), [MixerCmd::MixTracks { source, tracks }] if *source == mic && *tracks == MixTracks::FIRST),
            "{} commands",
            cmds.len()
        );
        assert!(wiring.mixer_inputs(&c, &registry, &sources, &mut levels, &mut notices).is_none(), "nothing changed");
    }

    #[test]
    fn a_changed_balance_or_mono_reaches_the_mixer_without_a_new_input_list() {
        let registry = builtin_registry(&Arc::new(RwLock::new(String::new())), &Arc::new(CaptionBoard::new())).unwrap();
        let mut sources = SourceSet::new(MasterClock::new());
        let (mut wiring, mut levels, mut notices) = (Wiring::default(), HashMap::new(), Vec::new());
        let mut c = default_collection("Test");
        sources.sync(&c, &registry);
        let mic = c.global_audio.first().copied().unwrap_or_else(|| panic!("the default collection has global audio"));
        c.source_mut(mic).unwrap().audio.balance = -0.5;
        let Some(cmds) = wiring.mixer_inputs(&c, &registry, &sources, &mut levels, &mut notices) else { panic!("no inputs") };
        let [MixerCmd::Inputs(inputs)] = cmds.as_slice() else { panic!("{} commands", cmds.len()) };
        assert_eq!(inputs.iter().find(|i| i.source == mic).map(|i| (i.balance, i.mono)), Some((-0.5, false)));
        c.source_mut(mic).unwrap().audio.mono = true;
        let cmds = wiring.mixer_inputs(&c, &registry, &sources, &mut levels, &mut notices).unwrap_or_default();
        assert!(
            matches!(cmds.as_slice(), [MixerCmd::Channels { source, balance, mono: true }] if *source == mic && *balance == -0.5),
            "{} commands",
            cmds.len()
        );
        assert!(wiring.mixer_inputs(&c, &registry, &sources, &mut levels, &mut notices).is_none(), "nothing changed");
    }

    /// The mixer's compressor and gate readings are found by source and filter while the mixer
    /// runs them; a filter without a meter, a disabled one and a removed one have none, and new
    /// inputs drop the old instances' readings.
    #[test]
    fn the_filter_meters_follow_the_mixer_inputs() {
        use studiocraft_media::audio::{BLOCK_FRAMES, BLOCK_SAMPLES, SAMPLE_RATE};
        use studiocraft_model::FilterConfig;
        let registry = builtin_registry(&Arc::new(RwLock::new(String::new())), &Arc::new(CaptionBoard::new())).unwrap();
        let mut sources = SourceSet::new(MasterClock::new());
        let (mut wiring, mut levels, mut notices) = (Wiring::default(), HashMap::new(), Vec::new());
        let mut c = default_collection("Test");
        let mic = c.global_audio.first().copied().unwrap_or_else(|| panic!("the default collection has global audio"));
        let filter = |n: u64, kind: &str, enabled: bool| FilterConfig {
            id: FilterId(900 + n),
            kind: kind.into(),
            kind_version: 1,
            name: format!("{kind} {n}"),
            enabled,
            settings: Settings::new(),
        };
        c.source_mut(mic).unwrap().filters = vec![
            filter(1, "compressor", true),
            filter(2, "gain", true),
            filter(3, "limiter", false),
            filter(4, "noise_gate", true),
            filter(5, "crop_pad", true),
        ];
        sources.sync(&c, &registry);
        assert_eq!(wiring.filter_meter(mic, FilterId(901)), None, "no inputs yet");
        let Some(cmds) = wiring.mixer_inputs(&c, &registry, &sources, &mut levels, &mut notices) else { panic!("no inputs") };
        let Some(MixerCmd::Inputs(mut inputs)) = cmds.into_iter().next() else { panic!("no input list") };
        let Some(input) = inputs.iter_mut().find(|i| i.source == mic) else { panic!("the microphone is not mixed") };
        assert_eq!(input.filters.len(), 3, "compressor, gain and gate run");
        assert_eq!(wiring.filter_meter(mic, FilterId(901)), Some(0.0));
        assert_eq!(wiring.filter_meter(mic, FilterId(904)), Some(0.0));
        for (id, why) in [(902, "gain has no meter"), (903, "disabled"), (905, "a video filter"), (999, "unknown")] {
            assert_eq!(wiring.filter_meter(mic, FilterId(id)), None, "{why}");
        }
        assert_eq!(wiring.filter_meter(SourceId(123_456), FilterId(901)), None, "another source");
        // A loud tone through the mixer's compressor instance shows in the wiring's reading.
        let mut tone: Vec<f32> = (0..8 * BLOCK_FRAMES)
            .flat_map(|i| {
                let v = 0.5 * (std::f32::consts::TAU * 997.0 * i as f32 / SAMPLE_RATE as f32).sin();
                [v, v]
            })
            .collect();
        tone.chunks_mut(BLOCK_SAMPLES).for_each(|b| input.filters[0].process_audio(b));
        let reading = wiring.filter_meter(mic, FilterId(901)).unwrap_or_default();
        assert!(reading > 1.0, "{reading} dB");
        // Silence through the closed gate: it turns the source down.
        input.filters[2].process_audio(&mut vec![0.0; BLOCK_SAMPLES]);
        assert!(wiring.filter_meter(mic, FilterId(904)).is_some_and(|db| db > 1.0));
        assert!(wiring.mixer_inputs(&c, &registry, &sources, &mut levels, &mut notices).is_none(), "nothing changed");
        assert_eq!(wiring.filter_meter(mic, FilterId(901)), Some(reading), "unchanged inputs keep their meters");
        // New settings make new instances: the old one's reading is dropped.
        c.source_mut(mic).unwrap().filters[0].settings = Settings::new().with("ratio", 4.0);
        assert!(wiring.mixer_inputs(&c, &registry, &sources, &mut levels, &mut notices).is_some());
        assert_eq!(wiring.filter_meter(mic, FilterId(901)), Some(0.0));
        input.filters[0].process_audio(&mut tone);
        assert_eq!(wiring.filter_meter(mic, FilterId(901)), Some(0.0), "the replaced instance no longer reports");
        // Disabled and removed filters have no reading.
        c.source_mut(mic).unwrap().filters[0].enabled = false;
        c.source_mut(mic).unwrap().filters.retain(|f| f.id != FilterId(904));
        assert!(wiring.mixer_inputs(&c, &registry, &sources, &mut levels, &mut notices).is_some());
        assert_eq!(wiring.filter_meter(mic, FilterId(901)), None);
        assert_eq!(wiring.filter_meter(mic, FilterId(904)), None);
        assert!(notices.is_empty(), "{} notices", notices.len());
    }

    /// A source's filter latency is the sum of the delays of the audio filters the mixer runs on
    /// it: disabled filters, video filters and settings a filter refused add nothing.
    #[test]
    fn the_filter_latency_adds_up_the_running_audio_filters() {
        use studiocraft_model::FilterConfig;
        let registry = builtin_registry(&Arc::new(RwLock::new(String::new())), &Arc::new(CaptionBoard::new())).unwrap();
        let mut sources = SourceSet::new(MasterClock::new());
        let (mut wiring, mut levels, mut notices) = (Wiring::default(), HashMap::new(), Vec::new());
        let mut c = default_collection("Test");
        let mic = c.global_audio.first().copied().unwrap_or_else(|| panic!("the default collection has global audio"));
        let filter = |n: u64, kind: &str, enabled: bool, settings: Settings| FilterConfig {
            id: FilterId(900 + n),
            kind: kind.into(),
            kind_version: 1,
            name: format!("{kind} {n}"),
            enabled,
            settings,
        };
        let limiter = registry.filter("limiter").and_then(|f| f.create(&Settings::new())).map(|f| f.latency_frames()).unwrap();
        assert!(limiter > 0);
        c.source_mut(mic).unwrap().filters = vec![
            filter(1, "limiter", true, Settings::new()),
            filter(2, "compressor", true, Settings::new()),
            filter(3, "limiter", true, Settings::new().with("ceiling_db", -3.0)),
            filter(4, "limiter", false, Settings::new()),
            filter(5, "limiter", true, Settings::new().with("ceiling_db", "loud")),
            filter(6, "crop_pad", true, Settings::new()),
        ];
        sources.sync(&c, &registry);
        assert_eq!(wiring.filter_latency(mic), 0, "no inputs yet");
        assert!(wiring.mixer_inputs(&c, &registry, &sources, &mut levels, &mut notices).is_some());
        assert_eq!(wiring.filter_latency(mic), 2 * limiter, "two running limiters");
        assert_eq!(notices.len(), 1, "the refused settings");
        assert_eq!(wiring.filter_latency(SourceId(123_456)), 0, "another source");
        assert!(wiring.mixer_inputs(&c, &registry, &sources, &mut levels, &mut notices).is_none(), "nothing changed");
        assert_eq!(wiring.filter_latency(mic), 2 * limiter, "unchanged inputs keep their latency");
        c.source_mut(mic).unwrap().filters[0].enabled = false;
        assert!(wiring.mixer_inputs(&c, &registry, &sources, &mut levels, &mut notices).is_some());
        assert_eq!(wiring.filter_latency(mic), limiter);
        c.source_mut(mic).unwrap().filters.retain(|f| f.kind != "limiter" || !f.enabled);
        assert!(wiring.mixer_inputs(&c, &registry, &sources, &mut levels, &mut notices).is_some());
        assert_eq!(wiring.filter_latency(mic), 0, "no filter adds a delay");
    }

    /// A source's size follows its enabled crop and pad filters in order, and a disabled one or
    /// one that rejected its settings changes nothing.
    #[test]
    fn the_filtered_size_follows_the_enabled_crop_and_pad_filters() {
        use studiocraft_model::FilterConfig;
        let registry = builtin_registry(&Arc::new(RwLock::new(String::new())), &Arc::new(CaptionBoard::new())).unwrap();
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
