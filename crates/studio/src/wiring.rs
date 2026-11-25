//! What the compositor draws and what the mixer mixes, derived from the scene collection.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use studiocraft_engine::host::SourceSet;
use studiocraft_engine::mixer::{MixInput, Mixer, MixerCmd};
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

/// What a mixer input is made of. The mixer gets a new input list when anything but a filter's
/// settings changes; a filter with new settings is replaced on its own.
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
    /// The audio filters the mixer runs on each source, in its order, with the sample frames each
    /// one delays the audio by; replaced with the inputs.
    running: HashMap<SourceId, Vec<(FilterId, usize)>>,
    /// The settings each running audio filter was last built with, by source and filter, with the
    /// source's audio queue and the filter's kind: a new input list builds a filter that refused
    /// its newer settings from these, while the source runs on the same queue and the filter is of
    /// the same kind.
    accepted: HashMap<(SourceId, FilterId), (SameQueue, String, Settings)>,
    gains: HashMap<SourceId, (f32, bool)>,
    offsets: HashMap<SourceId, u32>,
    modes: HashMap<SourceId, MonitorMode>,
    mix_tracks: HashMap<SourceId, MixTracks>,
    /// The mixer's count of dropped control commands at the last [`Wiring::mixer_dropped`].
    control_dropped: u64,
}

impl Wiring {
    /// Take the mixer's count of control commands a full queue dropped
    /// ([`Mixer::control_dropped`]). When it grew, a replacement or
    /// another change this wiring sent may not have reached the mixer, so the next
    /// [`Wiring::mixer_inputs`] sends a whole input list, and this returns `true`.
    pub(crate) fn mixer_dropped(&mut self, dropped: u64) -> bool {
        if dropped == self.control_dropped {
            return false;
        }
        self.control_dropped = dropped;
        self.mix = None;
        true
    }

    /// Take the mixer's count of dropped control commands right after sending it a whole input
    /// list, which carries every change the dropped commands carried.
    pub(crate) fn mixer_caught_up(&mut self, dropped: u64) {
        self.control_dropped = dropped;
    }

    /// [`Wiring::mixer_inputs`] for `mixer`. After the mixer's control queue dropped a command
    /// ([`Wiring::mixer_dropped`]), or when the commands are more than the mixer's queues hold
    /// ([`Mixer::fits`]), it is a whole input list instead, so no change is recorded as applied
    /// without reaching the mixer.
    pub(crate) fn mixer_commands(
        &mut self,
        mixer: &Mixer,
        c: &SceneCollection,
        registry: &Registry,
        sources: &SourceSet,
        levels: &mut HashMap<SourceId, Arc<Levels>>,
        notices: &mut Vec<Notice>,
    ) -> Option<Vec<MixerCmd>> {
        self.mixer_dropped(mixer.control_dropped());
        let noticed = notices.len();
        let cmds = self.mixer_inputs(c, registry, sources, levels, notices)?;
        if mixer.fits(&cmds) {
            return Some(cmds);
        }
        // The input list reports the refused settings again.
        notices.truncate(noticed);
        self.mix = None;
        self.mixer_inputs(c, registry, sources, levels, notices)
    }

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
                        // `settings` are the last ones the filter took, so rejected settings
                        // reach it again at the next sync.
                        if v.settings != f.settings {
                            match v.instance.update(&f.settings) {
                                Ok(()) => v.settings = f.settings.clone(),
                                Err(e) => notices.push(problem(&f.name, &e)),
                            }
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
    /// sources, the list of their running filters or the program scene changed. Otherwise each
    /// filter with new settings is built again with them and replaces its running instance, which
    /// the mixer crossfades into, and volume, mute, sync offset and monitoring changes follow.
    /// Settings a running filter refuses leave it running with its last ones, with a notice.
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
        levels.retain(|id, _| gains.contains_key(id));

        if let Some(changed) = self.settings_changes(&key) {
            let mut cmds = Vec::new();
            for (input, filter, index) in changed {
                let (Some(k), Some((s, _))) = (key.get(input), audio_sources.get(input)) else { continue };
                let Some((id, kind, settings)) = k.filters.get(filter) else { continue };
                // Settings the filter refuses leave the running instance as it is.
                match registry.filter(kind).and_then(|p| p.create(settings)) {
                    Ok(f) => {
                        self.accepted.insert((s.id, *id), (k.queue.clone(), kind.clone(), settings.clone()));
                        match f.meter() {
                            Some(m) => self.meters.insert((s.id, *id), m),
                            None => self.meters.remove(&(s.id, *id)),
                        };
                        if let Some(running) = self.running.get_mut(&s.id) {
                            if let Some(slot) = running.get_mut(index) {
                                slot.1 = f.latency_frames();
                            }
                            let total = running.iter().fold(0usize, |t, (_, frames)| t.saturating_add(*frames));
                            if total > 0 {
                                self.latencies.insert(s.id, total);
                            } else {
                                self.latencies.remove(&s.id);
                            }
                        }
                        cmds.push(MixerCmd::ReplaceFilter { source: s.id, index, instance: f });
                    }
                    Err(e) => notices.push(problem(&s.name, &e)),
                }
            }
            cmds.extend(gains.iter().filter(|(id, g)| self.gains.get(id) != Some(g)).map(|(id, (gain, muted))| MixerCmd::Gain {
                source: *id,
                gain: *gain,
                muted: *muted,
            }));
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
            self.mix = Some(key);
            self.gains = gains;
            self.offsets = offsets;
            self.modes = modes;
            self.mix_tracks = mix_tracks;
            return (!cmds.is_empty()).then_some(cmds);
        }

        let mut inputs = Vec::new();
        let mut meters = HashMap::new();
        let mut latencies = HashMap::new();
        let mut running = HashMap::new();
        let mut accepted = HashMap::new();
        for (k, (s, audio)) in key.iter().zip(&audio_sources) {
            let mut filters = Vec::new();
            let mut ids = Vec::new();
            for (id, kind, settings) in &k.filters {
                let create = |settings: &Settings| registry.filter(kind).and_then(|p| p.create(settings));
                // A running filter that refused its new settings keeps running with the last ones it
                // took.
                let built = create(settings).map(|f| (f, settings)).or_else(|e| {
                    notices.push(problem(&s.name, &e));
                    let last = self.accepted.get(&(s.id, *id)).filter(|(queue, was, _)| *queue == k.queue && was == kind);
                    last.ok_or(e).and_then(|(_, _, last)| create(last).map(|f| (f, last)))
                });
                let Ok((f, settings)) = built else { continue };
                accepted.insert((s.id, *id), (k.queue.clone(), kind.clone(), settings.clone()));
                if let Some(m) = f.meter() {
                    meters.insert((s.id, *id), m);
                }
                if f.latency_frames() > 0 {
                    let total: &mut usize = latencies.entry(s.id).or_default();
                    *total = total.saturating_add(f.latency_frames());
                }
                ids.push((*id, f.latency_frames()));
                filters.push(f);
            }
            running.insert(s.id, ids);
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
            });
        }
        self.mix = Some(key);
        self.meters = meters;
        self.latencies = latencies;
        self.running = running;
        self.accepted = accepted;
        self.gains = gains;
        self.offsets = offsets;
        self.modes = modes;
        self.mix_tracks = mix_tracks;
        Some(vec![MixerCmd::Inputs(inputs)])
    }

    /// The filters whose settings changed since the mixer's inputs were last brought in line, as
    /// (input in `key`, filter in that input's key, place in the mixer's filter list), when
    /// nothing else changed: the same sources on the same queues, heard as before, with the same
    /// filters in the same order. `None` when the mixer needs a new input list, which includes a
    /// changed filter the mixer does not run because it refused its earlier settings.
    fn settings_changes(&self, key: &[MixKey]) -> Option<Vec<(usize, usize, usize)>> {
        let old = self.mix.as_ref()?;
        if old.len() != key.len() {
            return None;
        }
        let mut changed = Vec::new();
        for (input, (o, n)) in old.iter().zip(key).enumerate() {
            if o.source != n.source || o.queue != n.queue || o.active != n.active || o.filters.len() != n.filters.len() {
                return None;
            }
            for (filter, ((id, kind, before), (new_id, new_kind, after))) in o.filters.iter().zip(&n.filters).enumerate() {
                if id != new_id || kind != new_kind {
                    return None;
                }
                if before != after {
                    let index = self.running.get(&n.source)?.iter().position(|(running, _)| running == id)?;
                    changed.push((input, filter, index));
                }
            }
        }
        Some(changed)
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

    /// An audio filter that delays its source by its `frames` setting, and refuses a negative one.
    struct Lag;
    impl studiocraft_plugin::Plugin for Lag {
        fn id(&self) -> &'static str {
            "test_lag"
        }
        fn name(&self) -> &'static str {
            "Lag"
        }
        fn schema(&self) -> studiocraft_plugin::SettingsSchema {
            studiocraft_plugin::SettingsSchema::new(vec![])
        }
    }
    impl studiocraft_plugin::Filter for Lag {
        fn target(&self) -> FilterTarget {
            FilterTarget::Audio
        }
        fn create(&self, settings: &Settings) -> Result<Box<dyn FilterInstance>, studiocraft_plugin::PluginError> {
            let frames = settings.i64("frames").unwrap_or(0);
            let frames = usize::try_from(frames).map_err(|_| studiocraft_plugin::PluginError::Failed(format!("frames {frames}")))?;
            Ok(Box::new(LagInstance(frames)))
        }
    }
    struct LagInstance(usize);
    impl FilterInstance for LagInstance {
        fn update(&mut self, _: &Settings) -> Result<(), studiocraft_plugin::PluginError> {
            Ok(())
        }
        fn latency_frames(&self) -> usize {
            self.0
        }
    }

    /// New settings on a running audio filter replace that filter alone, at its place among the
    /// filters the mixer runs, without a new input list, so the filters beside it keep their
    /// state. Its delay and its meter follow the new instance; settings it refuses leave it
    /// running; a filter that was not running starts through a new input list.
    #[test]
    fn new_settings_replace_only_that_filter_and_keep_its_delay_and_meter_current() {
        use studiocraft_model::FilterConfig;
        let mut registry = builtin_registry(&Arc::new(RwLock::new(String::new())), &Arc::new(CaptionBoard::new())).unwrap();
        registry.register_filter(Arc::new(Lag)).unwrap();
        let mut sources = SourceSet::new(MasterClock::new());
        let (mut wiring, mut levels, mut notices) = (Wiring::default(), HashMap::new(), Vec::new());
        let mut c = default_collection("Test");
        let mic = c.global_audio.first().copied().unwrap_or_else(|| panic!("the default collection has global audio"));
        let filter = |n: u64, kind: &str, settings: Settings| FilterConfig {
            id: FilterId(900 + n),
            kind: kind.into(),
            kind_version: 1,
            name: format!("{kind} {n}"),
            enabled: true,
            settings,
        };
        c.source_mut(mic).unwrap().filters = vec![
            filter(1, "limiter", Settings::new().with("ceiling_db", "loud")),
            filter(2, "test_lag", Settings::new().with("frames", 100)),
            filter(3, "compressor", Settings::new()),
            filter(4, "gain", Settings::new()),
        ];
        sources.sync(&c, &registry);
        assert!(wiring.mixer_inputs(&c, &registry, &sources, &mut levels, &mut notices).is_some());
        assert_eq!((notices.len(), wiring.filter_latency(mic)), (1, 100), "the limiter refused its settings");
        let replaced = |cmds: Option<Vec<MixerCmd>>| -> Vec<(SourceId, usize, usize)> {
            cmds.unwrap_or_default()
                .into_iter()
                .map(|cmd| match cmd {
                    MixerCmd::ReplaceFilter { source, index, instance } => (source, index, instance.latency_frames()),
                    _ => panic!("a command other than a filter replacement"),
                })
                .collect()
        };
        // The lag is the first filter the mixer runs, behind the limiter that does not run.
        c.source_mut(mic).unwrap().filters[1].settings = Settings::new().with("frames", 300);
        assert_eq!(replaced(wiring.mixer_inputs(&c, &registry, &sources, &mut levels, &mut notices)), [(mic, 0, 300)]);
        assert_eq!(wiring.filter_latency(mic), 300, "the delay follows the new instance");
        assert!(wiring.mixer_inputs(&c, &registry, &sources, &mut levels, &mut notices).is_none(), "nothing changed");
        // The compressor's meter is the new instance's.
        let before = wiring.meters.get(&(mic, FilterId(903))).cloned().unwrap_or_else(|| panic!("the compressor has a meter"));
        c.source_mut(mic).unwrap().filters[2].settings = Settings::new().with("ratio", 4.0);
        c.source_mut(mic).unwrap().filters[3].settings = Settings::new().with("db", -6.0);
        assert_eq!(replaced(wiring.mixer_inputs(&c, &registry, &sources, &mut levels, &mut notices)), [(mic, 1, 0), (mic, 2, 0)]);
        assert!(wiring.meters.get(&(mic, FilterId(903))).is_some_and(|m| !m.same(&before)));
        assert_eq!(wiring.filter_meter(mic, FilterId(904)), None, "gain has no meter");
        // Refused settings: a notice, and the running lag keeps its delay.
        c.source_mut(mic).unwrap().filters[1].settings = Settings::new().with("frames", -1);
        assert!(wiring.mixer_inputs(&c, &registry, &sources, &mut levels, &mut notices).is_none());
        assert_eq!((notices.len(), wiring.filter_latency(mic)), (2, 300));
        // The limiter starts with settings it takes, through a new input list, and the lag that
        // refuses -1 frames keeps running with its last settings.
        c.source_mut(mic).unwrap().filters[0].settings = Settings::new();
        let cmds = wiring.mixer_inputs(&c, &registry, &sources, &mut levels, &mut notices).unwrap_or_default();
        let [MixerCmd::Inputs(inputs)] = cmds.as_slice() else { panic!("{} commands", cmds.len()) };
        assert_eq!(inputs.iter().find(|i| i.source == mic).map(|i| i.filters.len()), Some(4));
        assert_eq!(wiring.filter_latency(mic), 248 + 300);
        // A volume change beside new settings comes with the replacement.
        c.source_mut(mic).unwrap().filters[3].settings = Settings::new().with("db", 3.0);
        c.source_mut(mic).unwrap().audio.gain = 0.5;
        let cmds = wiring.mixer_inputs(&c, &registry, &sources, &mut levels, &mut notices).unwrap_or_default();
        assert!(
            matches!(cmds.as_slice(), [MixerCmd::ReplaceFilter { index: 3, .. }, MixerCmd::Gain { gain, .. }] if *gain == 0.5),
            "{} commands",
            cmds.len()
        );
    }

    /// After the mixer's control queue drops a command, which may have been a filter replacement,
    /// the next refresh sends a whole input list with the current settings; the same count again
    /// sends nothing.
    #[test]
    fn a_dropped_control_command_brings_a_whole_input_list() {
        use studiocraft_model::FilterConfig;
        let registry = builtin_registry(&Arc::new(RwLock::new(String::new())), &Arc::new(CaptionBoard::new())).unwrap();
        let mut sources = SourceSet::new(MasterClock::new());
        let (mut wiring, mut levels, mut notices) = (Wiring::default(), HashMap::new(), Vec::new());
        let mut c = default_collection("Test");
        let mic = c.global_audio.first().copied().unwrap_or_else(|| panic!("the default collection has global audio"));
        c.source_mut(mic).unwrap().filters = vec![FilterConfig {
            id: FilterId(901),
            kind: "gain".into(),
            kind_version: 1,
            name: "Gain".into(),
            enabled: true,
            settings: Settings::new(),
        }];
        sources.sync(&c, &registry);
        assert!(wiring.mixer_inputs(&c, &registry, &sources, &mut levels, &mut notices).is_some());
        c.source_mut(mic).unwrap().filters[0].settings = Settings::new().with("db", -6.0);
        let cmds = wiring.mixer_inputs(&c, &registry, &sources, &mut levels, &mut notices).unwrap_or_default();
        assert!(matches!(cmds.as_slice(), [MixerCmd::ReplaceFilter { index: 0, .. }]), "{} commands", cmds.len());
        assert!(!wiring.mixer_dropped(0));
        assert!(wiring.mixer_inputs(&c, &registry, &sources, &mut levels, &mut notices).is_none(), "nothing was dropped");
        assert!(wiring.mixer_dropped(1));
        let cmds = wiring.mixer_inputs(&c, &registry, &sources, &mut levels, &mut notices).unwrap_or_default();
        let [MixerCmd::Inputs(inputs)] = cmds.as_slice() else { panic!("{} commands", cmds.len()) };
        assert_eq!(inputs.iter().find(|i| i.source == mic).map(|i| i.filters.len()), Some(1));
        assert!(!wiring.mixer_dropped(1));
        assert!(wiring.mixer_inputs(&c, &registry, &sources, &mut levels, &mut notices).is_none(), "nothing was dropped since");
    }

    /// More filter replacements than the mixer's control queue holds go to the mixer as one input
    /// list, and fewer go as replacements.
    #[test]
    fn more_replacements_than_the_mixer_queue_holds_bring_a_whole_input_list() {
        use studiocraft_model::FilterConfig;
        let registry = builtin_registry(&Arc::new(RwLock::new(String::new())), &Arc::new(CaptionBoard::new())).unwrap();
        let mixer = Mixer::detached();
        let mut sources = SourceSet::new(MasterClock::new());
        let (mut wiring, mut levels, mut notices) = (Wiring::default(), HashMap::new(), Vec::new());
        let mut c = default_collection("Test");
        let mic = c.global_audio.first().copied().unwrap_or_else(|| panic!("the default collection has global audio"));
        let count = 2048;
        c.source_mut(mic).unwrap().filters = (0..count)
            .map(|n| FilterConfig {
                id: FilterId(1000 + n),
                kind: "gain".into(),
                kind_version: 1,
                name: format!("Gain {n}"),
                enabled: true,
                settings: Settings::new(),
            })
            .collect();
        sources.sync(&c, &registry);
        assert!(wiring.mixer_commands(&mixer, &c, &registry, &sources, &mut levels, &mut notices).is_some());
        let set_all = |c: &mut SceneCollection, db: f64| {
            for f in &mut c.source_mut(mic).unwrap().filters {
                f.settings = Settings::new().with("db", db);
            }
        };
        set_all(&mut c, -6.0);
        let cmds = wiring.mixer_commands(&mixer, &c, &registry, &sources, &mut levels, &mut notices).unwrap_or_default();
        let [MixerCmd::Inputs(inputs)] = cmds.as_slice() else { panic!("{} commands", cmds.len()) };
        assert_eq!(inputs.iter().find(|i| i.source == mic).map(|i| i.filters.len()), Some(count as usize));
        assert!(wiring.mixer_commands(&mixer, &c, &registry, &sources, &mut levels, &mut notices).is_none(), "nothing changed");
        c.source_mut(mic).unwrap().filters[0].settings = Settings::new().with("db", 3.0);
        let cmds = wiring.mixer_commands(&mixer, &c, &registry, &sources, &mut levels, &mut notices).unwrap_or_default();
        assert!(matches!(cmds.as_slice(), [MixerCmd::ReplaceFilter { index: 0, .. }]), "{} commands", cmds.len());
        assert!(notices.is_empty());
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

    /// Settings a filter rejected reach it again at the next sync, so a file that was missing
    /// is read once it is there.
    #[test]
    fn rejected_filter_settings_are_tried_again_at_the_next_sync() {
        use studiocraft_model::FilterConfig;
        let registry = builtin_registry(&Arc::new(RwLock::new(String::new())), &Arc::new(CaptionBoard::new())).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let table = |name: &str| dir.path().join(name).to_string_lossy().into_owned();
        let identity = "LUT_3D_SIZE 2\n0 0 0\n1 0 0\n0 1 0\n1 1 0\n0 0 1\n1 0 1\n0 1 1\n1 1 1\n";
        std::fs::write(table("first.cube"), identity).unwrap();
        let mut c = default_collection("Test");
        let id = FilterId(950);
        c.sources[0].filters = vec![FilterConfig {
            id,
            kind: "apply_lut".into(),
            kind_version: 1,
            name: "Look".into(),
            enabled: true,
            settings: Settings::new().with("file", table("first.cube")),
        }];
        let (mut wiring, mut notices) = (Wiring::default(), Vec::new());
        wiring.sync_video_filters(&c, &registry, &mut notices);
        assert!(notices.is_empty());
        let image = |w: &Wiring| w.video_filters.get(&id).and_then(|v| v.instance.video_effect()).and_then(|e| e.image);
        let first = image(&wiring);
        assert!(first.is_some());
        // The second file is missing: the filter keeps its table.
        c.sources[0].filters[0].settings = Settings::new().with("file", table("second.cube"));
        wiring.sync_video_filters(&c, &registry, &mut notices);
        assert_eq!(notices.len(), 1);
        assert_eq!(image(&wiring), first);
        // Once the file is there, the same settings load it.
        std::fs::write(table("second.cube"), identity.replace("1 1 1\n", "0.5 0.5 0.5\n")).unwrap();
        wiring.sync_video_filters(&c, &registry, &mut notices);
        assert_eq!(notices.len(), 1);
        assert_ne!(image(&wiring), first);
    }
}
