//! The audio mixer thread.
//!
//! Once per block (1024 frames at 48 kHz, every 21.3 ms) on the master clock, the mixer takes
//! one block from each input's queue, delays it by the input's sync offset, runs the input's
//! audio filters, turns it to mono and applies its balance when the input is set to, applies its
//! volume, mute and transition fade, adds it to the mix and publishes
//! the input's levels. The clipped mix goes to every output's queue, stamped with the block's
//! time.
//!
//! Each input's delay line holds [`MAX_SYNC_OFFSET_MS`] of audio and is allocated with the input
//! list on the sending thread, so changing an offset does not allocate on the mixer thread. When a
//! source sends no block, its delay line plays on into a silent block until the audio it holds has
//! played.
//!
//! Inputs set to monitoring also go into a second mix, after the same delay, filters, volume,
//! mute and fade, which goes clipped to the monitoring queue when one is set. An input set to
//! monitor only is left out of the program mix that the outputs receive; its levels are still
//! measured.
//!
//! Each input also goes to the mix tracks it is assigned to ([`MIX_TRACKS`] of them), with the
//! same share it adds to the program mix, hold included; an input left out of the program is on no
//! track. A track is mixed only while an output takes it ([`MixerCmd::AddTrackOutput`]), and each
//! such track goes clipped to the queues of the outputs that take it. The track mixes are
//! allocated with the mixer, so taking or dropping a track does not allocate on the mixer thread.
//!
//! The interface talks to the thread through a lock-free command queue, so moving a fader never
//! waits on the mixer.
//!
//! A filter whose settings change is replaced on its own ([`MixerCmd::ReplaceFilter`]): for one
//! block the old and the new instance both process the input, and the output crossfades from the
//! old one's audio to the new one's. The source's other filters keep running with their state,
//! and the old instance goes back to the sending thread to be freed.
//!
//! The clipped mix also feeds a loudness meter (ITU-R BS.1770-4, EBU R 128), whose readings are
//! published after every block in [`Loudness`].
//!
//! While audio tracks are recorded, a [`TapSet`] gives inputs and the program a [`TapSlot`]
//! each. A tapped input stores what it adds to the mix (after its delay, filters, gain, mute and
//! fade) into a block from its slot in the same pass that adds it, so the stems of the program's
//! inputs add up to the program mix before clipping; a muted or faded-out input stores silence.
//! While the program is held ([`MixerCmd::Hold`]), a tapped input still stores its own audio, so
//! the stems then add up to more than the held mix. The program's tap gets the clipped mix the outputs get. Tap sets are built on the sending
//! thread and handed back to it when replaced, and without one each input costs one check.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use craft_audio_dsp::LoudnessMeter;
use studiocraft_media::audio::{AudioPool, BLOCK_FRAMES, BLOCK_SAMPLES, CHANNELS, PooledBlock, SAMPLE_RATE, block_pool, block_rate};
use studiocraft_media::clock::{MasterClock, Pacer};
use studiocraft_media::mix::{BlockLevels, Levels, clip, measure, mix_into, mix_into_tap, scale_into};
use studiocraft_media::queue::Fifo;
use studiocraft_media::tap::TapSlot;
use studiocraft_model::collection::MAX_SYNC_OFFSET_MS;
use studiocraft_model::{MIX_TRACKS, MixTracks, MonitorMode, SourceId};
use studiocraft_plugin::{AudioOut, FilterInstance};

/// Blocks an input may have queued before the oldest are dropped, which bounds its latency to
/// about 85 ms when a device runs faster than the master clock.
pub const MAX_QUEUED_BLOCKS: usize = 4;

/// The frames of a sync offset of `ms` milliseconds, at most [`MAX_SYNC_OFFSET_MS`].
fn offset_frames(ms: u32) -> usize {
    let ms = u64::from(ms.min(MAX_SYNC_OFFSET_MS));
    usize::try_from(ms * u64::from(SAMPLE_RATE) / 1000).unwrap_or(0)
}

/// An input's delay line: the last [`MAX_SYNC_OFFSET_MS`] of its audio, and one frame more.
struct Delay {
    ring: Vec<f32>,
    write: usize,
}

impl Delay {
    fn new() -> Delay {
        Delay { ring: vec![0.0; (offset_frames(MAX_SYNC_OFFSET_MS) + 1) * CHANNELS], write: 0 }
    }

    /// The frames the line holds.
    fn frames(&self) -> usize {
        self.ring.len() / CHANNELS
    }

    /// Replace `block` with the audio `frames` frames older. The ring starts silent, so the first
    /// `frames` frames after the input starts are silence.
    fn process(&mut self, block: &mut [f32], frames: usize) {
        let cap = self.ring.len();
        if cap == 0 {
            return;
        }
        let behind = frames.saturating_mul(CHANNELS).min(cap - CHANNELS.min(cap));
        for s in block.iter_mut() {
            if let Some(slot) = self.ring.get_mut(self.write) {
                *slot = *s;
            }
            if behind > 0 {
                *s = self.ring.get((self.write + cap - behind) % cap).copied().unwrap_or(0.0);
            }
            self.write = (self.write + 1) % cap;
        }
    }
}

/// One input of the mix.
pub struct MixInput {
    pub source: SourceId,
    pub audio: Arc<AudioOut>,
    pub gain: f32,
    pub muted: bool,
    /// Heard in the program right now (global audio, or a source of the program scene).
    pub active: bool,
    pub levels: Arc<Levels>,
    pub filters: Vec<Box<dyn FilterInstance>>,
    /// How long the input's audio is delayed, in milliseconds (at most [`MAX_SYNC_OFFSET_MS`]).
    pub sync_offset_ms: u32,
    /// Whether the input goes into the monitoring mix, and whether it stays in the program mix.
    pub monitor: MonitorMode,
    /// The mix tracks the input goes to, when it is heard in the program.
    pub mix_tracks: MixTracks,
    /// Where the input sits between the left (−1) and right (1) channels; 0 leaves both alone.
    pub balance: f32,
    /// Whether the input's two channels are mixed into one, played on both.
    pub mono: bool,
}

/// The taps of one take of audio tracks: the program's, and those of the inputs recorded to
/// their own track. Built on the sending thread; the mixer only clones the slots' handles.
#[derive(Default)]
pub struct TapSet {
    pub program: Option<Arc<TapSlot>>,
    pub inputs: Vec<(SourceId, Arc<TapSlot>)>,
}

impl TapSet {
    fn input(&self, source: SourceId) -> Option<&Arc<TapSlot>> {
        self.inputs.iter().find(|(s, _)| *s == source).map(|(_, t)| t)
    }
}

/// Messages to the mixer thread.
pub enum MixerCmd {
    /// Replace the inputs (sources, filters or the program scene changed).
    Inputs(Vec<MixInput>),
    /// A fader or mute button moved.
    Gain {
        source: SourceId,
        gain: f32,
        muted: bool,
    },
    /// A source's sync offset changed, in milliseconds.
    SyncOffset {
        source: SourceId,
        ms: u32,
    },
    /// A source's monitoring mode changed.
    Monitor {
        source: SourceId,
        mode: MonitorMode,
    },
    /// A filter of a source has new settings: `instance`, built with them on the sending thread,
    /// takes the place of the running filter at `index` in the source's filter list. Over the
    /// next block the output crossfades from the old instance's audio to the new one's, and the
    /// old instance goes back to the sending thread. An `index` or `source` the mixer does not
    /// run sends `instance` straight back.
    ReplaceFilter {
        source: SourceId,
        index: usize,
        instance: Box<dyn FilterInstance>,
    },
    /// The mix tracks a source goes to changed.
    MixTracks {
        source: SourceId,
        tracks: MixTracks,
    },
    /// A source's balance or its downmix to mono changed.
    Channels {
        source: SourceId,
        balance: f32,
        mono: bool,
    },
    /// Where the monitoring mix goes, or `None` to stop making it.
    MonitorOutput(Option<Arc<Fifo<PooledBlock>>>),
    /// How long inputs take to fade in or out when the program scene changes, in milliseconds.
    FadeMs(u32),
    /// Hold the program mix: what every input adds to the program fades out, global audio
    /// included, and fades back in when the hold ends. The monitoring device, the meters and each
    /// input's own audio track still get the input's audio. A new input list keeps the hold.
    Hold(bool),
    AddOutput {
        id: u64,
        queue: Arc<Fifo<PooledBlock>>,
    },
    /// Send mix track `track` (1 to [`MIX_TRACKS`]) to the output `id`. One output can take
    /// several tracks, each on its own queue; [`MixerCmd::RemoveOutput`] removes them all.
    AddTrackOutput {
        id: u64,
        track: usize,
        queue: Arc<Fifo<PooledBlock>>,
    },
    RemoveOutput {
        id: u64,
    },
    /// Tap the inputs and the program for audio tracks, or stop tapping with `None`. Once the
    /// mixer has applied it, [`Mixer::taps_applied`] reads `generation`.
    Taps {
        set: Option<Arc<TapSet>>,
        generation: u64,
    },
}

/// A command as the mixer thread takes it: an input list arrives built, so the thread does not
/// allocate it.
enum Cmd {
    Mix(MixerCmd),
    Inputs(Vec<Input>),
}

impl From<MixerCmd> for Cmd {
    fn from(cmd: MixerCmd) -> Cmd {
        match cmd {
            MixerCmd::Inputs(list) => Cmd::Inputs(
                list.into_iter()
                    .map(|cfg| Input {
                        delay_frames: offset_frames(cfg.sync_offset_ms),
                        channels: Arc::new(AtomicU64::new(pack_channels(cfg.balance, cfg.mono))),
                        fading: Vec::with_capacity(cfg.filters.len()),
                        cfg,
                        fade: 0.0,
                        delay: Delay::new(),
                        idle: usize::MAX,
                        tap: None,
                    })
                    .collect(),
            ),
            other => Cmd::Mix(other),
        }
    }
}

/// Commands numbered in the order they were sent.
type CmdQueue = Fifo<(u64, Cmd)>;

/// An input's balance and mono in one atomic word: the balance's bits, and the mono bit above
/// them.
fn pack_channels(balance: f32, mono: bool) -> u64 {
    u64::from(balance.to_bits()) | (u64::from(mono) << 32)
}

/// The balance and mono [`pack_channels`] packed.
fn unpack_channels(bits: u64) -> (f32, bool) {
    (f32::from_bits(bits as u32), (bits >> 32) & 1 == 1)
}

/// Input lists the mixer replaced, freed by the sending thread.
type Retired = Fifo<Vec<Input>>;

/// Tap sets the mixer replaced, freed by the sending thread.
type RetiredTaps = Fifo<Arc<TapSet>>;

/// Filter instances the mixer replaced, freed by the sending thread.
type RetiredFilters = Fifo<Box<dyn FilterInstance>>;

/// The final mix's loudness readings, as the mixer thread last published them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LoudnessReading {
    /// Momentary loudness (the last 400 ms), LUFS; −∞ for silence and for the first 400 ms
    /// after the start or a reset.
    pub momentary_lufs: f64,
    /// Short-term loudness (the last 3 s), LUFS; −∞ for silence and for the first 3 s after the
    /// start or a reset.
    pub short_term_lufs: f64,
    /// Gated integrated loudness since the start or the last reset, LUFS; −∞ until a block
    /// passes the gates.
    pub integrated_lufs: f64,
    /// Loudness range since the start or the last reset, LU; 0 with too little signal.
    pub range_lu: f64,
    /// Highest true peak since the start or the last reset, dBTP; −∞ for silence.
    pub true_peak_dbtp: f64,
    /// How many times the mixer has published readings: a reading with the same generation as
    /// an earlier one is the same reading. 0 before the first block.
    pub generation: u64,
}

/// How many times [`Loudness::load`] spins for a consistent set of readings before it yields
/// the thread between tries. The mixer writes for well under a microsecond about 47 times a
/// second, so a reader almost never needs a second try.
const LOUDNESS_LOAD_SPINS: usize = 64;

/// Loudness readings shared between the mixer thread and the interface, without locks. The
/// readings are published as one set: `seq` is odd while the mixer writes them, and a reader
/// that sees it change reads again.
#[derive(Debug)]
pub struct Loudness {
    seq: AtomicU64,
    momentary: AtomicU64,
    short_term: AtomicU64,
    integrated: AtomicU64,
    range: AtomicU64,
    true_peak: AtomicU64,
    reset: AtomicBool,
}

impl Default for Loudness {
    fn default() -> Self {
        let silent = f64::NEG_INFINITY.to_bits();
        Loudness {
            seq: AtomicU64::new(0),
            momentary: AtomicU64::new(silent),
            short_term: AtomicU64::new(silent),
            integrated: AtomicU64::new(silent),
            range: AtomicU64::new(0.0f64.to_bits()),
            true_peak: AtomicU64::new(silent),
            reset: AtomicBool::new(false),
        }
    }
}

impl Loudness {
    /// The readings the mixer last published, all from the same block.
    pub fn load(&self) -> LoudnessReading {
        let mut tries = 0usize;
        loop {
            let before = self.seq.load(Ordering::Acquire);
            if before.is_multiple_of(2) {
                let reading = self.read_fields(before / 2);
                std::sync::atomic::fence(Ordering::Acquire);
                if self.seq.load(Ordering::Relaxed) == before {
                    return reading;
                }
            }
            tries = tries.saturating_add(1);
            if tries < LOUDNESS_LOAD_SPINS {
                std::hint::spin_loop();
            } else {
                std::thread::yield_now();
            }
        }
    }

    fn read_fields(&self, generation: u64) -> LoudnessReading {
        let get = |a: &AtomicU64| f64::from_bits(a.load(Ordering::Relaxed));
        LoudnessReading {
            momentary_lufs: get(&self.momentary),
            short_term_lufs: get(&self.short_term),
            integrated_lufs: get(&self.integrated),
            range_lu: get(&self.range),
            true_peak_dbtp: get(&self.true_peak),
            generation,
        }
    }

    /// Ask the mixer to start every reading again (momentary, short-term, integrated, loudness
    /// range and true peak) before its next block.
    pub fn request_reset(&self) {
        self.reset.store(true, Ordering::Relaxed);
    }

    /// Publish the meter's readings. Only the mixer thread calls this.
    fn store(&self, m: &LoudnessMeter) {
        self.publish([m.momentary(), m.short_term(), m.integrated(), m.loudness_range(), m.true_peak_dbtp()]);
    }

    /// Publish momentary, short-term, integrated, range and true peak as one set. Only one thread
    /// may publish.
    fn publish(&self, values: [f64; 5]) {
        let seq = self.seq.load(Ordering::Relaxed);
        self.seq.store(seq.wrapping_add(1), Ordering::Relaxed);
        std::sync::atomic::fence(Ordering::Release);
        let fields = [&self.momentary, &self.short_term, &self.integrated, &self.range, &self.true_peak];
        for (a, v) in fields.into_iter().zip(values) {
            a.store(v.to_bits(), Ordering::Relaxed);
        }
        self.seq.store(seq.wrapping_add(2), Ordering::Release);
    }
}

/// The mixer thread's handle.
pub struct Mixer {
    /// Fader moves. A full queue drops the oldest: a later move of the same fader follows.
    gains: Arc<CmdQueue>,
    /// Each input's balance and mono, by source, for every input list not yet freed. A balance or
    /// mono change is stored straight into the inputs' words, which the mixer reads every block,
    /// so no queue can drop it. Only the sending thread uses the list itself.
    channels: Mutex<Vec<(SourceId, Arc<AtomicU64>)>>,
    /// Input lists, sync offsets, monitoring modes, mix tracks and filter replacements: the
    /// commands an input list also carries. A full queue drops the oldest, and the sender follows
    /// a drop ([`Mixer::control_dropped`]) with a whole input list. None is dropped for a burst of
    /// fader moves or balance changes.
    control: Arc<CmdQueue>,
    /// Outputs added and removed, and where the monitoring mix goes. These are few, and a burst
    /// of input commands, fader moves or balance changes cannot drop one.
    outputs: Arc<CmdQueue>,
    /// The latest fade time and hold in one word ([`pack_settings`]), which the mixer reads after
    /// taking its queues. No queue can drop either, a hold always comes with the fade time sent
    /// before it, and an input list comes with the fade time sent before it.
    settings: Arc<AtomicU64>,
    /// The latest tap set, which replaces one the mixer has not applied yet. It is kept apart
    /// from `control`, so a burst of other commands can never drop the removal of the taps.
    taps: Arc<CmdQueue>,
    retired: Arc<Retired>,
    retired_taps: Arc<RetiredTaps>,
    retired_filters: Arc<RetiredFilters>,
    taps_applied: Arc<AtomicU64>,
    sent: AtomicU64,
    master: Arc<Levels>,
    loudness: Arc<Loudness>,
    stop: Arc<AtomicBool>,
}

impl Mixer {
    /// Start the mixer thread on `clock`.
    pub fn spawn(clock: MasterClock) -> std::io::Result<Mixer> {
        let m = Mixer::detached();
        m.stop.store(false, Ordering::Relaxed);
        let mut state = MixState::new(Arc::clone(&m.master));
        state.retired = Arc::clone(&m.retired);
        state.retired_taps = Arc::clone(&m.retired_taps);
        state.retired_filters = Arc::clone(&m.retired_filters);
        state.taps_applied = Arc::clone(&m.taps_applied);
        state.loudness = Arc::clone(&m.loudness);
        let queues = m.queues().map(Arc::clone);
        let settings = Arc::clone(&m.settings);
        let s = Arc::clone(&m.stop);
        std::thread::Builder::new().name("audio mixer".into()).spawn(move || run(clock, queues, &settings, &s, state))?;
        Ok(m)
    }

    /// A mixer without a thread, for when no thread can be started: it takes commands and
    /// mixes nothing.
    pub fn detached() -> Mixer {
        Mixer {
            gains: Arc::new(Fifo::new(256)),
            channels: Mutex::new(Vec::new()),
            control: Arc::new(Fifo::new(CONTROL_QUEUE)),
            outputs: Arc::new(Fifo::new(CONTROL_QUEUE)),
            settings: Arc::new(AtomicU64::new(pack_settings(0, SCENE_FADE_MS, false))),
            taps: Arc::new(Fifo::new(1)),
            retired: Arc::new(Fifo::new(RETIRED_LISTS)),
            retired_taps: Arc::new(Fifo::new(RETIRED_LISTS)),
            retired_filters: Arc::new(Fifo::new(RETIRED_FILTERS)),
            taps_applied: Arc::new(AtomicU64::new(0)),
            sent: AtomicU64::new(0),
            master: Arc::new(Levels::default()),
            loudness: Arc::new(Loudness::default()),
            stop: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Send `cmd` to the mixer thread, after freeing what it handed back. It takes `&mut self` so
    /// that one sender at a time pushes, which bounds what the mixer can retire between two frees
    /// ([`RETIRED_LISTS`], [`RETIRED_FILTERS`]).
    pub fn send(&mut self, cmd: MixerCmd) {
        self.free_retired();
        let n = self.sent.fetch_add(1, Ordering::Relaxed) + 1;
        match cmd {
            MixerCmd::Gain { .. } => self.gains.push((n, cmd.into())),
            // Stored into the inputs of every list not yet freed: the one in use and any still
            // waiting, so a list sent before the change takes it and one sent after keeps its own
            // values.
            MixerCmd::Channels { source, balance, mono } => {
                let bits = pack_channels(balance, mono);
                let mut channels = self.channels.lock().unwrap_or_else(PoisonError::into_inner);
                // A word only this list holds belongs to an input list that was freed.
                channels.retain(|(_, word)| Arc::strong_count(word) > 1);
                for (_, word) in channels.iter().filter(|(s, _)| *s == source) {
                    word.store(bits, Ordering::Relaxed);
                }
            }
            // A set the mixer has not applied yet is replaced, and freed here.
            MixerCmd::Taps { .. } => self.taps.push((n, cmd.into())),
            MixerCmd::Inputs(_) => {
                let cmd = Cmd::from(cmd);
                if let Cmd::Inputs(list) = &cmd {
                    let mut channels = self.channels.lock().unwrap_or_else(PoisonError::into_inner);
                    channels.retain(|(_, word)| Arc::strong_count(word) > 1);
                    channels.extend(list.iter().map(|i| (i.cfg.source, Arc::clone(&i.channels))));
                }
                self.control.push((n, cmd));
            }
            _ if is_output(&cmd) => self.outputs.push((n, cmd.into())),
            // A value the mixer has not read yet is replaced.
            MixerCmd::FadeMs(ms) => self.store_settings(Some(ms), None),
            MixerCmd::Hold(on) => self.store_settings(None, Some(on)),
            _ => self.control.push((n, cmd.into())),
        }
    }

    /// Store a new fade time or hold, keeping the other value. Only this thread writes the word.
    fn store_settings(&mut self, fade: Option<u32>, hold: Option<bool>) {
        let (count, ms, held) = unpack_settings(self.settings.load(Ordering::Relaxed));
        self.settings.store(pack_settings(count.wrapping_add(1), fade.unwrap_or(ms), hold.unwrap_or(held)), Ordering::Release);
    }

    /// The queues in the order the mixer thread takes them ([`drain`]).
    fn queues(&self) -> [&Arc<CmdQueue>; 4] {
        [&self.taps, &self.outputs, &self.control, &self.gains]
    }

    /// Free the input lists, tap sets and filter instances the mixer handed back, on this thread.
    pub fn free_retired(&self) {
        while let Some(list) = self.retired.pop() {
            drop(list);
        }
        while let Some(set) = self.retired_taps.pop() {
            drop(set);
        }
        while let Some(filter) = self.retired_filters.pop() {
            drop(filter);
        }
    }

    /// How many input lists, sync offsets, monitoring modes, mix tracks and filter replacements a
    /// full queue has dropped since the mixer started. A dropped command never reaches the mixer;
    /// a whole input list carries what it carried.
    pub fn control_dropped(&self) -> u64 {
        self.control.dropped()
    }

    /// Whether `cmds`, sent one after another, all stay in their queues until the mixer takes
    /// them: no more fader moves than the fader queue holds, no more input commands than the
    /// control queue holds, and no more output commands than the output queue holds. A full queue
    /// drops its oldest command, so a later push drops one of `cmds` only when they are more than
    /// their queue holds. Tap sets, fade times and holds are not counted: the latest replaces the
    /// one before. Balance and mono changes are not counted either: they go into no queue.
    pub fn fits(&self, cmds: &[MixerCmd]) -> bool {
        let gains = cmds.iter().filter(|c| matches!(c, MixerCmd::Gain { .. })).count();
        let outputs = cmds.iter().filter(|c| is_output(c)).count();
        let uncounted = |c: &MixerCmd| {
            matches!(c, MixerCmd::Gain { .. } | MixerCmd::Taps { .. } | MixerCmd::Channels { .. } | MixerCmd::FadeMs(_) | MixerCmd::Hold(_))
        };
        let control = cmds.iter().filter(|c| !is_output(c) && !uncounted(c)).count();
        gains <= self.gains.capacity() && outputs <= self.outputs.capacity() && control <= self.control.capacity()
    }

    /// The `generation` of the last [`MixerCmd::Taps`] the mixer applied; 0 before the first.
    pub fn taps_applied(&self) -> u64 {
        self.taps_applied.load(Ordering::Acquire)
    }

    /// The counter [`Mixer::taps_applied`] reads, for another thread to wait on.
    pub fn taps_applied_counter(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.taps_applied)
    }

    /// Whether the mixer thread runs. A detached mixer applies no command.
    pub fn running(&self) -> bool {
        !self.stop.load(Ordering::Relaxed)
    }

    /// Levels of the final mix.
    pub fn master(&self) -> &Arc<Levels> {
        &self.master
    }

    /// Loudness of the final mix.
    pub fn loudness(&self) -> &Arc<Loudness> {
        &self.loudness
    }
}

impl Drop for Mixer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

struct Input {
    cfg: MixInput,
    /// Current fade, 0 (silent) to 1 (full), moving toward `active`.
    fade: f32,
    delay: Delay,
    /// The sync offset in frames.
    delay_frames: usize,
    /// The balance and mono in use ([`pack_channels`]), which [`Mixer::send`] changes in place.
    channels: Arc<AtomicU64>,
    /// Frames of silence the delay line has taken since the source's last block. While it is
    /// below the offset, the line still holds audio to play; while it is below the line's length,
    /// the line still holds audio that a longer offset would read.
    idle: usize,
    /// Where the input's stem goes while audio tracks are recorded.
    tap: Option<Arc<TapSlot>>,
    /// Replaced filters still heard for one block, with their place in `cfg.filters`: the
    /// output crossfades from each one's audio to its replacement's. It has room for one per
    /// filter, made on the sending thread.
    fading: Vec<(usize, Box<dyn FilterInstance>)>,
}

/// Input commands that can wait for the mixer, and output commands that can wait.
const CONTROL_QUEUE: usize = 1024;

/// Whether `cmd` goes to the mixer's output queue.
fn is_output(cmd: &MixerCmd) -> bool {
    matches!(cmd, MixerCmd::AddOutput { .. } | MixerCmd::AddTrackOutput { .. } | MixerCmd::RemoveOutput { .. } | MixerCmd::MonitorOutput(_))
}

/// How long inputs fade in or out on a scene change until a [`MixerCmd::FadeMs`] sets it.
const SCENE_FADE_MS: u32 = 300;

/// The fade time in milliseconds in the low 32 bits, the hold in bit 32, and in the bits above a
/// count of the changes, which tells the mixer that the word changed.
fn pack_settings(count: u32, fade_ms: u32, held: bool) -> u64 {
    (u64::from(count & 0x7fff_ffff) << 33) | (u64::from(held) << 32) | u64::from(fade_ms)
}

/// The count, fade time and hold that [`pack_settings`] packed.
fn unpack_settings(word: u64) -> (u32, u32, bool) {
    ((word >> 33) as u32, word as u32, word & (1 << 32) != 0)
}

/// Replaced input lists and tap sets held for the sending thread to free. The sender frees them
/// before each command it sends, and each command retires at most one of each. Between two frees
/// the mixer can apply the command it took before the first free, the commands the control queue
/// held then, and the one the sender pushes after it, so no more can wait than that and none is
/// freed on the mixer thread. [`Mixer::send`] takes `&mut self`, so sends cannot overlap and push
/// more.
const RETIRED_LISTS: usize = CONTROL_QUEUE + 2;

/// Replaced filters the inputs hold to crossfade from, all inputs together. A filter replaced
/// while this many wait goes back at once, without a crossfade.
const FADING_FILTERS: usize = CONTROL_QUEUE;

/// Replaced filter instances held for the sending thread to free: the ones the commands since the
/// sender last freed retire, as for [`RETIRED_LISTS`], plus the ones that were still crossfading
/// then and go back on the next block.
const RETIRED_FILTERS: usize = RETIRED_LISTS + FADING_FILTERS;

/// Output queues the mixer holds room for without allocating: the program mix of a few outputs,
/// and every mix track of two.
const OUTPUT_ROOM: usize = 8 + 2 * MIX_TRACKS;

/// A queue an output reads, with what goes to it: the program mix, or a mix track.
struct OutputQueue {
    id: u64,
    /// `None` for the program mix, else the track's index (track 1 is 0).
    track: Option<usize>,
    queue: Arc<Fifo<PooledBlock>>,
}

/// The mixer's state, separate from the thread so it can be tested block by block.
pub struct MixState {
    inputs: Vec<Input>,
    retired: Arc<Retired>,
    outputs: Vec<OutputQueue>,
    /// One mix for each mix track, and a bit for each track an output takes (bit 0 is track 1).
    track_mixes: [Vec<f32>; MIX_TRACKS],
    tracks_taken: u8,
    master: Arc<Levels>,
    meter: LoudnessMeter,
    loudness: Arc<Loudness>,
    pool: Arc<AudioPool>,
    mix: Vec<f32>,
    /// A block the delay lines of inputs without a new block play into.
    silence: Vec<f32>,
    /// A copy of the block a replaced filter processes while its output crossfades.
    crossfade: Vec<f32>,
    retired_filters: Arc<RetiredFilters>,
    /// The mix of the monitored inputs, and the queue it goes to.
    monitor_mix: Vec<f32>,
    monitor_out: Option<Arc<Fifo<PooledBlock>>>,
    fade_step: f32,
    /// Set while the program mix is held ([`MixerCmd::Hold`]).
    held: bool,
    /// The count of the last fade time and hold read from [`Mixer::send`]'s word.
    settings_read: u32,
    /// The share of each input that reaches the program mix: it fades from 1 to 0 while held,
    /// and back.
    hold: f32,
    /// The number of the input list in use: a fader move sent before it is older than the
    /// gains the list carries.
    inputs_sent: u64,
    /// The taps of the take of audio tracks in progress.
    taps: Option<Arc<TapSet>>,
    retired_taps: Arc<RetiredTaps>,
    taps_applied: Arc<AtomicU64>,
}

impl MixState {
    pub fn new(master: Arc<Levels>) -> Self {
        let mut s = Self {
            inputs: Vec::new(),
            retired: Arc::new(Fifo::new(RETIRED_LISTS)),
            taps: None,
            retired_taps: Arc::new(Fifo::new(RETIRED_LISTS)),
            taps_applied: Arc::new(AtomicU64::new(0)),
            outputs: Vec::with_capacity(OUTPUT_ROOM),
            track_mixes: std::array::from_fn(|_| vec![0.0; BLOCK_SAMPLES]),
            tracks_taken: 0,
            master,
            meter: LoudnessMeter::new(f64::from(SAMPLE_RATE), CHANNELS),
            loudness: Arc::new(Loudness::default()),
            pool: block_pool(),
            mix: vec![0.0; BLOCK_SAMPLES],
            silence: vec![0.0; BLOCK_SAMPLES],
            crossfade: vec![0.0; BLOCK_SAMPLES],
            retired_filters: Arc::new(Fifo::new(RETIRED_FILTERS)),
            monitor_mix: vec![0.0; BLOCK_SAMPLES],
            monitor_out: None,
            fade_step: 1.0,
            held: false,
            settings_read: 0,
            hold: 1.0,
            inputs_sent: 0,
        };
        s.set_fade_ms(SCENE_FADE_MS);
        s
    }

    /// Take the fade time and hold of `word` ([`pack_settings`]) when they changed since the last
    /// word read.
    fn read_settings(&mut self, word: u64) {
        let (count, ms, held) = unpack_settings(word);
        if count != self.settings_read {
            self.settings_read = count;
            self.set_fade_ms(ms);
            self.held = held;
        }
    }

    fn set_fade_ms(&mut self, ms: u32) {
        let blocks = (f64::from(ms) / 1000.0) * block_rate().as_f64();
        self.fade_step = if blocks < 1.0 { 1.0 } else { (1.0 / blocks) as f32 };
    }

    /// Note which tracks an output takes.
    fn count_tracks(&mut self) {
        self.tracks_taken = self.outputs.iter().filter_map(|o| o.track).fold(0, |bits, t| bits | (1 << t));
    }

    pub fn apply(&mut self, cmd: MixerCmd) {
        self.apply_sent(self.inputs_sent, cmd.into());
    }

    /// Apply the command numbered `sent` by [`Mixer::send`].
    fn apply_sent(&mut self, sent: u64, cmd: Cmd) {
        let cmd = match cmd {
            Cmd::Inputs(mut list) => {
                self.inputs_sent = sent;
                for i in &mut list {
                    let active = if i.cfg.active { 1.0 } else { 0.0 };
                    match self.inputs.iter_mut().find(|o| o.cfg.source == i.cfg.source) {
                        // The source keeps the audio its delay line holds while it sends into the
                        // same queue; the new, empty line goes back with the old list. A source
                        // started again has a new queue and starts with a silent line.
                        Some(o) => {
                            i.fade = o.fade;
                            if Arc::ptr_eq(&i.cfg.audio, &o.cfg.audio) {
                                std::mem::swap(&mut i.delay, &mut o.delay);
                                i.idle = o.idle;
                            }
                        }
                        None => i.fade = active,
                    }
                    // Cloning a slot's handle only counts a reference.
                    i.tap = self.taps.as_ref().and_then(|t| t.input(i.cfg.source)).cloned();
                }
                let old = std::mem::replace(&mut self.inputs, list);
                self.retired.push(old);
                return;
            }
            Cmd::Mix(cmd) => cmd,
        };
        match cmd {
            MixerCmd::Gain { .. } if sent < self.inputs_sent => {}
            // Built into `Cmd::Inputs` before it reaches the mixer.
            MixerCmd::Inputs(_) => {}
            MixerCmd::Gain { source, gain, muted } => {
                for i in self.inputs.iter_mut().filter(|i| i.cfg.source == source) {
                    i.cfg.gain = gain;
                    i.cfg.muted = muted;
                }
            }
            MixerCmd::SyncOffset { source, ms } => {
                for i in self.inputs.iter_mut().filter(|i| i.cfg.source == source) {
                    i.cfg.sync_offset_ms = ms.min(MAX_SYNC_OFFSET_MS);
                    i.delay_frames = offset_frames(ms);
                }
            }
            MixerCmd::Monitor { source, mode } => {
                for i in self.inputs.iter_mut().filter(|i| i.cfg.source == source) {
                    i.cfg.monitor = mode;
                }
            }
            MixerCmd::ReplaceFilter { source, index, instance } => {
                let waiting: usize = self.inputs.iter().map(|i| i.fading.len()).sum();
                let Some(input) = self.inputs.iter_mut().find(|i| i.cfg.source == source) else {
                    self.retired_filters.push(instance);
                    return;
                };
                let Some(slot) = input.cfg.filters.get_mut(index) else {
                    self.retired_filters.push(instance);
                    return;
                };
                let old = std::mem::replace(slot, instance);
                // A filter replaced again before a block was mixed: the instance in between was
                // never heard, and the output still crossfades from the one that was.
                if input.fading.iter().any(|(i, _)| *i == index) || input.fading.len() == input.fading.capacity() || waiting >= FADING_FILTERS {
                    self.retired_filters.push(old);
                } else {
                    input.fading.push((index, old));
                }
            }
            MixerCmd::MonitorOutput(queue) => self.monitor_out = queue,
            MixerCmd::FadeMs(ms) => self.set_fade_ms(ms),
            MixerCmd::Hold(on) => self.held = on,
            MixerCmd::MixTracks { source, tracks } => {
                for i in self.inputs.iter_mut().filter(|i| i.cfg.source == source) {
                    i.cfg.mix_tracks = tracks;
                }
            }
            MixerCmd::Channels { source, balance, mono } => {
                for i in self.inputs.iter_mut().filter(|i| i.cfg.source == source) {
                    i.channels.store(pack_channels(balance, mono), Ordering::Relaxed);
                }
            }
            MixerCmd::AddOutput { id, queue } => {
                self.outputs.retain(|o| !(o.id == id && o.track.is_none()));
                self.outputs.push(OutputQueue { id, track: None, queue });
            }
            MixerCmd::AddTrackOutput { id, track, queue } => {
                // A track that does not exist is not mixed.
                if (1..=MIX_TRACKS).contains(&track) {
                    let index = track - 1;
                    self.outputs.retain(|o| !(o.id == id && o.track == Some(index)));
                    self.outputs.push(OutputQueue { id, track: Some(index), queue });
                }
                self.count_tracks();
            }
            MixerCmd::RemoveOutput { id } => {
                self.outputs.retain(|o| o.id != id);
                self.count_tracks();
            }
            MixerCmd::Taps { set, generation } => {
                // The inputs let go of their slots first; the old set still holds them, and it
                // goes back to the sending thread, so no slot is freed here.
                for i in &mut self.inputs {
                    i.tap = set.as_ref().and_then(|t| t.input(i.cfg.source)).cloned();
                }
                if let Some(old) = std::mem::replace(&mut self.taps, set) {
                    self.retired_taps.push(old);
                }
                self.taps_applied.store(generation, Ordering::Release);
            }
        }
    }

    /// Mix one block stamped `pts`.
    pub fn tick(&mut self, pts: studiocraft_media::Tick) {
        self.mix.iter_mut().for_each(|s| *s = 0.0);
        let monitoring = self.monitor_out.is_some();
        if monitoring {
            self.monitor_mix.iter_mut().for_each(|s| *s = 0.0);
        }
        let hold_target = if self.held { 0.0 } else { 1.0 };
        self.hold =
            if self.hold < hold_target { (self.hold + self.fade_step).min(hold_target) } else { (self.hold - self.fade_step).max(hold_target) };
        let hold = self.hold;
        let taken = self.tracks_taken;
        for (t, m) in self.track_mixes.iter_mut().enumerate() {
            if taken & (1 << t) != 0 {
                m.iter_mut().for_each(|s| *s = 0.0);
            }
        }
        for input in &mut self.inputs {
            while input.cfg.audio.queued() > MAX_QUEUED_BLOCKS {
                drop(input.cfg.audio.pop());
            }
            let target = if input.cfg.active { 1.0 } else { 0.0 };
            input.fade = if input.fade < target { (input.fade + self.fade_step).min(target) } else { (input.fade - self.fade_step).max(target) };
            let mut popped = input.cfg.audio.pop();
            let samples: &mut [f32] = match &mut popped {
                Some(block) => {
                    if block.samples.len() != BLOCK_SAMPLES {
                        block.clear();
                    }
                    input.idle = 0;
                    &mut block.samples[..]
                }
                // A source that sends nothing still plays the audio its delay line holds, after
                // silence, as the same source without an offset would have played it.
                None if input.idle < input.delay_frames => {
                    input.idle = input.idle.saturating_add(BLOCK_FRAMES);
                    self.silence.iter_mut().for_each(|s| *s = 0.0);
                    &mut self.silence[..]
                }
                None => {
                    // The line still ages while nothing plays from it, so a longer offset set later
                    // reads silence instead of the audio from before the gap.
                    if input.idle < input.delay.frames() {
                        input.idle = input.idle.saturating_add(BLOCK_FRAMES);
                        self.silence.iter_mut().for_each(|s| *s = 0.0);
                        input.delay.process(&mut self.silence, 0);
                    }
                    input.cfg.levels.store(BlockLevels::default());
                    // No audio runs through the filters, so a replaced one has nothing to fade
                    // out and goes back at once.
                    retire_fading(&mut input.fading, &self.retired_filters);
                    // A tapped source that sends nothing records silence, so its file keeps time.
                    if let Some(t) = &input.tap
                        && let Some(mut b) = t.claim()
                    {
                        b.samples.fill(0.0);
                        b.pts = pts;
                        t.push(b);
                    }
                    continue;
                }
            };
            input.delay.process(samples, input.delay_frames);
            for (index, f) in input.cfg.filters.iter_mut().enumerate() {
                match (input.fading.iter_mut().find(|(i, _)| *i == index), self.crossfade.get_mut(..samples.len())) {
                    (Some((_, old)), Some(before)) => {
                        before.copy_from_slice(samples);
                        old.process_audio(before);
                        f.process_audio(samples);
                        crossfade(samples, before);
                    }
                    _ => f.process_audio(samples),
                }
            }
            retire_fading(&mut input.fading, &self.retired_filters);
            let (balance, mono) = unpack_channels(input.channels.load(Ordering::Relaxed));
            balance_and_mono(samples, balance, mono);
            let gain = input.cfg.gain * input.fade;
            let heard = !input.cfg.muted && input.fade > 0.0;
            if heard && monitoring && input.cfg.monitor.monitored() {
                mix_into(&mut self.monitor_mix, samples, gain);
            }
            let mut stem = input.tap.as_ref().and_then(|t| t.claim());
            let levels = match (heard, input.cfg.monitor.in_program(), &mut stem) {
                // The stem is what the input adds to the program, stored in the same pass.
                (true, true, Some(b)) if hold >= 1.0 => mix_into_tap(&mut self.mix, samples, gain, &mut b.samples),
                (true, true, None) if hold >= 1.0 => mix_into(&mut self.mix, samples, gain),
                // While the program is held, the stem and the meter keep the input's audio, and
                // the program gets the held share of it.
                (true, true, stem) => {
                    if let Some(b) = stem {
                        scale_into(&mut b.samples, samples, gain);
                    }
                    if hold > 0.0 {
                        mix_into(&mut self.mix, samples, gain * hold);
                    }
                    measure(samples, gain)
                }
                // A source left out of the program still meters what it plays on the monitor, and
                // its stem, when it has one, is what it plays.
                (true, false, stem) => {
                    if let Some(b) = stem {
                        scale_into(&mut b.samples, samples, gain);
                    }
                    measure(samples, gain)
                }
                (false, _, stem) => {
                    if let Some(b) = stem {
                        b.samples.fill(0.0);
                    }
                    measure(samples, input.cfg.gain)
                }
            };
            if let (Some(mut b), Some(t)) = (stem, &input.tap) {
                b.pts = pts;
                t.push(b);
            }
            // Each track the input is on gets the share it adds to the program.
            let mut on = taken & input.cfg.mix_tracks.bits();
            if heard && hold > 0.0 && input.cfg.monitor.in_program() && on != 0 {
                let share = if hold >= 1.0 { gain } else { gain * hold };
                while on != 0 {
                    let t = on.trailing_zeros() as usize;
                    on &= on - 1;
                    if let Some(m) = self.track_mixes.get_mut(t) {
                        add_into(m, samples, share);
                    }
                }
            }
            input.cfg.levels.store(levels);
        }
        if let Some(q) = &self.monitor_out {
            clip(&mut self.monitor_mix);
            send_block(&self.pool, &self.outputs, q, &self.monitor_mix, pts);
        }
        clip(&mut self.mix);
        if let Some(t) = self.taps.as_ref().and_then(|t| t.program.as_ref())
            && let Some(mut b) = t.claim()
        {
            b.samples.copy_from_slice(&self.mix);
            b.pts = pts;
            t.push(b);
        }
        self.master.store(measure(&self.mix, 1.0));
        if self.loudness.reset.swap(false, Ordering::Relaxed) {
            self.meter.reset_program();
        }
        self.meter.process_interleaved(&self.mix);
        self.loudness.store(&self.meter);
        for (t, m) in self.track_mixes.iter_mut().enumerate() {
            if taken & (1 << t) != 0 {
                clip(m);
            }
        }
        for o in &self.outputs {
            let mix = match o.track {
                None => &self.mix[..],
                Some(t) => match self.track_mixes.get(t) {
                    Some(m) => &m[..],
                    None => continue,
                },
            };
            send_block(&self.pool, &self.outputs, &o.queue, mix, pts);
        }
    }
}

/// Turn `block`'s interleaved stereo frames to mono when `mono` is set, each channel becoming the
/// average of the two, and then apply `balance`: above 0 the left channel is scaled by
/// `1 - balance`, below 0 the right channel by `1 + balance`, so the center leaves both at full
/// level and either end silences the other side. A balance that is not a number counts as the
/// center, and one past an end as that end.
fn balance_and_mono(block: &mut [f32], balance: f32, mono: bool) {
    let balance = if balance.is_finite() { balance.clamp(-1.0, 1.0) } else { 0.0 };
    if !mono && balance == 0.0 {
        return;
    }
    let (left, right) = if balance > 0.0 { (1.0 - balance, 1.0) } else { (1.0, 1.0 + balance) };
    for frame in block.as_chunks_mut::<CHANNELS>().0 {
        let [l, r, ..] = frame;
        if mono {
            let m = (*l + *r) * 0.5;
            *l = m;
            *r = m;
        }
        *l *= left;
        *r *= right;
    }
}

/// Add `input * gain` to `out`, sample by sample, as far as the shorter of the two goes.
fn add_into(out: &mut [f32], input: &[f32], gain: f32) {
    let gain = if gain.is_finite() { gain.max(0.0) } else { 0.0 };
    for (o, s) in out.iter_mut().zip(input) {
        *o += s * gain;
    }
}

/// Fade `new` in over `old` across the block, frame by frame: the gains of the two add up to 1,
/// the first frame is mostly `old` and the last one is `new`.
fn crossfade(new: &mut [f32], old: &[f32]) {
    let frames = new.len() / CHANNELS;
    if frames == 0 {
        return;
    }
    let step = 1.0 / frames as f32;
    for (i, (n, o)) in new.as_chunks_mut::<CHANNELS>().0.iter_mut().zip(old.as_chunks::<CHANNELS>().0).enumerate() {
        let w = (i + 1) as f32 * step;
        for (a, b) in n.iter_mut().zip(o) {
            *a = *b + (*a - *b) * w;
        }
    }
}

/// Hand the replaced filters an input no longer plays back to the sending thread. The list keeps
/// its room, so this does not free memory on the mixer thread.
fn retire_fading(fading: &mut Vec<(usize, Box<dyn FilterInstance>)>, retired: &RetiredFilters) {
    while let Some((_, old)) = fading.pop() {
        retired.push(old);
    }
}

/// Push a copy of `mix` stamped `pts` to `q`.
///
/// A queue that stops being read would make the pool allocate a block every tick; its oldest
/// unread block is dropped and reused instead. When `q` holds none, the oldest block of the
/// output queue holding the most is reused.
fn send_block(pool: &Arc<AudioPool>, outputs: &[OutputQueue], q: &Fifo<PooledBlock>, mix: &[f32], pts: studiocraft_media::Tick) {
    let reclaimed =
        if pool.exhausted() { q.reclaim().or_else(|| outputs.iter().max_by_key(|o| o.queue.len()).and_then(|o| o.queue.reclaim())) } else { None };
    let mut out = reclaimed.unwrap_or_else(|| pool.take());
    out.samples.clear();
    out.samples.extend_from_slice(mix);
    out.pts = pts;
    q.push(out);
}

/// Apply the waiting commands in the order of `queues` ([`Mixer::queues`]): the latest tap set,
/// the output commands, the input commands, then the fader moves, skipping the moves older than
/// the input list in use. Then take the latest fade time and hold from `settings`, after the
/// queues, so every command popped comes with the fade time and hold sent before it. All of them
/// apply before the next block, so a tap set applied ahead of an input list sent before it still
/// reaches that list.
fn drain(queues: [&Arc<CmdQueue>; 4], settings: &AtomicU64, state: &mut MixState) {
    for q in queues {
        while let Some((n, c)) = q.pop() {
            state.apply_sent(n, c);
        }
    }
    state.read_settings(settings.load(Ordering::Acquire));
}

fn run(clock: MasterClock, queues: [Arc<CmdQueue>; 4], settings: &AtomicU64, stop: &AtomicBool, mut state: MixState) {
    let mut pacer = Pacer::new(clock, block_rate());
    while !stop.load(Ordering::Relaxed) {
        let t = pacer.wait();
        drain(queues.each_ref(), settings, &mut state);
        state.tick(t.pts);
    }
}

#[cfg(test)]
mod tests {
    use studiocraft_media::Tick;

    use super::*;

    fn input(source: u64, audio: &Arc<AudioOut>, gain: f32, active: bool) -> (MixInput, Arc<Levels>) {
        let levels = Arc::new(Levels::default());
        (
            MixInput {
                source: SourceId(source),
                audio: Arc::clone(audio),
                gain,
                muted: false,
                active,
                levels: Arc::clone(&levels),
                filters: vec![],
                sync_offset_ms: 0,
                monitor: MonitorMode::Off,
                mix_tracks: MixTracks::ALL,
                balance: 0.0,
                mono: false,
            },
            levels,
        )
    }

    /// Push one block whose left channel is `left` and right channel `right` to `a`.
    fn feed_stereo(a: &AudioOut, left: f32, right: f32) {
        let mut b = a.block();
        b.samples.clear();
        for _ in 0..BLOCK_FRAMES {
            b.samples.extend_from_slice(&[left, right]);
        }
        a.push(b);
    }

    /// The left and right samples of the first frame of the next block on `q`.
    fn first_frame(q: &Fifo<PooledBlock>) -> (f32, f32) {
        let b = q.pop().unwrap_or_else(|| panic!("no block"));
        (b.samples[0], b.samples[1])
    }

    #[test]
    fn balance_and_mono_shape_an_input_before_the_program_the_tracks_and_the_monitor() {
        let close2 = |got: (f32, f32), want: (f32, f32)| (got.0 - want.0).abs() < 1e-6 && (got.1 - want.1).abs() < 1e-6;
        let a = Arc::new(AudioOut::default());
        let (mut i, levels) = input(1, &a, 0.5, true);
        i.balance = 0.5;
        i.monitor = MonitorMode::MonitorAndOutput;
        let (mut m, out, monitor) = monitored(vec![i]);
        let track = take_track(&mut m, 2, 2);
        // Toward the right, the left channel is lowered and the right keeps its level; the gain
        // applies after.
        feed_stereo(&a, 0.8, 0.4);
        m.tick(Tick(0));
        let want = (0.8 * 0.5 * 0.5, 0.4 * 0.5);
        for (name, got) in [("program", first_frame(&out)), ("track", first_frame(&track)), ("monitor", first_frame(&monitor))] {
            assert!(close2(got, want), "{name}: {got:?}");
        }
        assert!((levels.load().peak[0] - 0.2).abs() < 1e-6, "the meter reads the balanced left channel: {:?}", levels.load().peak);
        // Mono averages the channels first, then the balance applies; the change needs no new list.
        m.apply(MixerCmd::Channels { source: SourceId(1), balance: -1.0, mono: true });
        feed_stereo(&a, 0.8, 0.4);
        m.tick(Tick(1));
        let got = first_frame(&out);
        assert!(close2(got, (0.6 * 0.5, 0.0)), "fully left silences the right: {got:?}");
        drop((track.pop(), monitor.pop()));
        m.apply(MixerCmd::Channels { source: SourceId(1), balance: 0.0, mono: true });
        feed_stereo(&a, 1.0, -1.0);
        m.tick(Tick(2));
        assert!(close2(first_frame(&out), (0.0, 0.0)), "opposite channels cancel in mono");
        // A balance that is not a number counts as the center; one past an end as that end.
        let mut block = vec![0.8, 0.4, 0.8, 0.4];
        balance_and_mono(&mut block, f32::NAN, false);
        assert_eq!(block, [0.8, 0.4, 0.8, 0.4]);
        balance_and_mono(&mut block, 7.0, false);
        assert_eq!(block, [0.0, 0.4, 0.0, 0.4]);
        let mut odd = vec![0.8, 0.4, 0.3];
        balance_and_mono(&mut odd, 0.0, true);
        assert!(close2((odd[0], odd[1]), (0.6, 0.6)) && odd[2] == 0.3, "a trailing half frame is left alone: {odd:?}");
    }

    fn feed(a: &AudioOut, value: f32) {
        let mut b = a.block();
        b.samples.clear();
        b.samples.resize(BLOCK_SAMPLES, value);
        a.push(b);
    }

    struct Half;
    impl FilterInstance for Half {
        fn update(&mut self, _: &studiocraft_model::Settings) -> Result<(), studiocraft_plugin::PluginError> {
            Ok(())
        }
        fn process_audio(&mut self, block: &mut [f32]) {
            block.iter_mut().for_each(|s| *s *= 0.5);
        }
    }

    #[test]
    fn mixes_inputs_with_gain_filters_and_mute() {
        let mut m = MixState::new(Arc::new(Levels::default()));
        let (a, b) = (Arc::new(AudioOut::default()), Arc::new(AudioOut::default()));
        let (mut ia, la) = input(1, &a, 0.5, true);
        ia.filters.push(Box::new(Half));
        let (ib, lb) = input(2, &b, 1.0, true);
        m.apply(MixerCmd::Inputs(vec![ia, ib]));
        let out = Arc::new(Fifo::new(8));
        m.apply(MixerCmd::AddOutput { id: 1, queue: Arc::clone(&out) });
        feed(&a, 0.8);
        feed(&b, 0.1);
        m.tick(Tick(42));
        let block = out.pop().unwrap();
        assert!((block.samples[0] - (0.8 * 0.5 * 0.5 + 0.1)).abs() < 1e-6);
        assert_eq!(block.pts, Tick(42));
        assert!((la.load().peak[0] - 0.2).abs() < 1e-6);
        assert!((lb.load().peak[1] - 0.1).abs() < 1e-6);

        m.apply(MixerCmd::Gain { source: SourceId(2), gain: 1.0, muted: true });
        feed(&a, 0.0);
        feed(&b, 0.4);
        m.tick(Tick(43));
        let block = out.pop().unwrap();
        assert_eq!(block.samples[0], 0.0, "muted input is not heard");
        assert!((lb.load().peak[0] - 0.4).abs() < 1e-6, "but still metered");
    }

    #[test]
    fn a_boosted_input_is_louder_than_unity_and_the_mix_still_clips() {
        let mut m = MixState::new(Arc::new(Levels::default()));
        let a = Arc::new(AudioOut::default());
        let (ia, la) = input(1, &a, 1.0, true);
        m.apply(MixerCmd::Inputs(vec![ia]));
        let out = Arc::new(Fifo::new(8));
        m.apply(MixerCmd::AddOutput { id: 1, queue: Arc::clone(&out) });
        feed(&a, 0.05);
        m.tick(Tick(1));
        let unity = out.pop().unwrap().samples[0];
        assert!((unity - 0.05).abs() < 1e-6);

        m.apply(MixerCmd::Gain { source: SourceId(1), gain: studiocraft_model::collection::MAX_GAIN, muted: false });
        feed(&a, 0.05);
        m.tick(Tick(2));
        let boosted = out.pop().unwrap().samples[0];
        assert!((boosted - 0.5).abs() < 1e-6, "+20 dB is ten times the level: {boosted}");
        assert!((la.load().peak[0] - 0.5).abs() < 1e-6, "the meter reads the boosted level");

        feed(&a, 0.5);
        m.tick(Tick(3));
        assert_eq!(out.pop().unwrap().samples[0], 1.0, "the final mix is clipped as before");
    }

    #[test]
    fn an_output_that_stops_reading_does_not_make_the_mixer_allocate() {
        let mut m = MixState::new(Arc::new(Levels::default()));
        let out = Arc::new(Fifo::new(256));
        m.apply(MixerCmd::AddOutput { id: 1, queue: Arc::clone(&out) });
        let kept = m.pool.capacity();
        for i in 0..1000 {
            m.tick(Tick(i));
        }
        assert_eq!(m.pool.made(), kept, "blocks made with nothing read");
        assert_eq!(out.len(), kept);
        assert_eq!(out.dropped(), 1000 - kept as u64);
        // The newest block is the last one mixed.
        let newest = std::iter::from_fn(|| out.pop()).last().map(|b| b.pts);
        assert_eq!(newest, Some(Tick(999)));
    }

    #[test]
    fn a_stalled_output_beside_a_reading_one_does_not_make_the_mixer_allocate() {
        let mut m = MixState::new(Arc::new(Levels::default()));
        let (stalled, reading) = (Arc::new(Fifo::new(256)), Arc::new(Fifo::new(256)));
        m.apply(MixerCmd::AddOutput { id: 1, queue: Arc::clone(&stalled) });
        m.apply(MixerCmd::AddOutput { id: 2, queue: Arc::clone(&reading) });
        let kept = m.pool.capacity();
        let mut held = None;
        for i in 0..1000 {
            // The reading output takes each block and holds it until the next one is mixed.
            drop(held.take());
            m.tick(Tick(i));
            held = reading.pop();
        }
        assert_eq!(m.pool.made(), kept, "blocks made beyond the ones the pool keeps");
    }

    /// A queue for `track` of output `id`.
    fn take_track(m: &mut MixState, id: u64, track: usize) -> Arc<Fifo<PooledBlock>> {
        let queue = Arc::new(Fifo::new(64));
        m.apply(MixerCmd::AddTrackOutput { id, track, queue: Arc::clone(&queue) });
        queue
    }

    fn close(got: f32, want: f32) -> bool {
        (got - want).abs() < 1e-6
    }

    #[test]
    fn each_mix_track_holds_the_inputs_assigned_to_it() {
        let mut m = MixState::new(Arc::new(Levels::default()));
        let sources: Vec<Arc<AudioOut>> = (0..4).map(|_| Arc::new(AudioOut::default())).collect();
        let (mut a, _) = input(1, &sources[0], 1.0, true);
        a.mix_tracks = MixTracks::from_bits(0b011);
        let (mut b, _) = input(2, &sources[1], 1.0, true);
        b.mix_tracks = MixTracks::from_bits(0b010);
        let (mut c, _) = input(3, &sources[2], 1.0, true);
        c.mix_tracks = MixTracks::NONE;
        // Left out of the program, a monitor-only source is on no track either.
        let (mut d, _) = input(4, &sources[3], 1.0, true);
        d.monitor = MonitorMode::MonitorOnly;
        m.apply(MixerCmd::Inputs(vec![a, b, c, d]));
        let program = Arc::new(Fifo::new(64));
        m.apply(MixerCmd::AddOutput { id: 1, queue: Arc::clone(&program) });
        let tracks: Vec<_> = (1..=3).map(|t| take_track(&mut m, 1, t)).collect();
        let play = |m: &mut MixState, at: i64| {
            for (s, v) in sources.iter().zip([0.1, 0.2, 0.3, 0.4]) {
                feed(s, v);
            }
            m.tick(Tick(at));
        };
        play(&mut m, 7);
        let p = program.pop().unwrap();
        assert!(close(p.samples[0], 0.6), "the program has every input it hears: {}", p.samples[0]);
        let got: Vec<(f32, Tick)> = tracks.iter().map(|q| q.pop().map(|b| (b.samples[1], b.pts)).unwrap()).collect();
        assert!(close(got[0].0, 0.1) && close(got[1].0, 0.3) && got[2].0 == 0.0, "{got:?}");
        assert!(got.iter().all(|(_, pts)| *pts == Tick(7)), "every track is stamped with the block's time");

        // A change of tracks and a mute take effect on the next block, without a new input list.
        m.apply(MixerCmd::MixTracks { source: SourceId(3), tracks: MixTracks::from_bits(0b100) });
        m.apply(MixerCmd::Gain { source: SourceId(2), gain: 1.0, muted: true });
        play(&mut m, 8);
        let got: Vec<f32> = tracks.iter().map(|q| q.pop().unwrap().samples[0]).collect();
        assert!(close(got[0], 0.1) && close(got[1], 0.1) && close(got[2], 0.3), "{got:?}");
        assert!(close(program.pop().unwrap().samples[0], 0.4));

        // Removing the output removes every queue it had.
        m.apply(MixerCmd::RemoveOutput { id: 1 });
        play(&mut m, 9);
        assert!(program.pop().is_none() && tracks.iter().all(|q| q.pop().is_none()));
        assert_eq!(m.tracks_taken, 0, "no track is mixed once no output takes one");
    }

    #[test]
    fn a_track_is_clipped_and_fades_with_the_held_program() {
        let mut m = MixState::new(Arc::new(Levels::default()));
        m.apply(MixerCmd::FadeMs(100));
        let (a, b) = (Arc::new(AudioOut::default()), Arc::new(AudioOut::default()));
        let (ia, _) = input(1, &a, 1.0, true);
        let (ib, _) = input(2, &b, 1.0, true);
        m.apply(MixerCmd::Inputs(vec![ia, ib]));
        let program = Arc::new(Fifo::new(64));
        m.apply(MixerCmd::AddOutput { id: 1, queue: Arc::clone(&program) });
        let track = take_track(&mut m, 1, 6);
        feed(&a, 0.7);
        feed(&b, 0.7);
        m.tick(Tick(0));
        assert_eq!(track.pop().unwrap().samples[0], 1.0, "a track is clipped like the program");
        program.pop();
        m.apply(MixerCmd::Hold(true));
        for i in 1..10 {
            feed(&a, 0.2);
            feed(&b, 0.1);
            m.tick(Tick(i));
            let (p, t) = (program.pop().unwrap().samples[0], track.pop().unwrap().samples[0]);
            assert!(close(p, t), "block {i}: the track follows the held program ({p} and {t})");
        }
        assert!(track.pop().is_none());
    }

    #[test]
    fn a_track_that_does_not_exist_is_not_mixed() {
        let mut m = MixState::new(Arc::new(Levels::default()));
        let a = Arc::new(AudioOut::default());
        let (ia, _) = input(1, &a, 1.0, true);
        m.apply(MixerCmd::Inputs(vec![ia]));
        let none: Vec<_> = [0, MIX_TRACKS + 1, usize::MAX].into_iter().map(|t| take_track(&mut m, 1, t)).collect();
        feed(&a, 0.5);
        m.tick(Tick(0));
        assert!(none.iter().all(|q| q.pop().is_none()));
        assert_eq!(m.tracks_taken, 0);
    }

    #[test]
    fn stalled_track_queues_do_not_make_the_mixer_allocate() {
        let mut m = MixState::new(Arc::new(Levels::default()));
        let a = Arc::new(AudioOut::default());
        let (ia, _) = input(1, &a, 1.0, true);
        m.apply(MixerCmd::Inputs(vec![ia]));
        let queues: Vec<_> = (1..=MIX_TRACKS)
            .map(|t| {
                let q = Arc::new(Fifo::new(256));
                m.apply(MixerCmd::AddTrackOutput { id: 1, track: t, queue: Arc::clone(&q) });
                q
            })
            .collect();
        let kept = m.pool.capacity();
        for i in 0..1000 {
            feed(&a, 0.1);
            m.tick(Tick(i));
        }
        assert_eq!(m.pool.made(), kept, "blocks made with no track read");
        assert!(queues.iter().all(|q| !q.is_empty()), "every track still gets its newest blocks");
    }

    #[test]
    fn inactive_inputs_fade_out_and_back_in() {
        let mut m = MixState::new(Arc::new(Levels::default()));
        m.apply(MixerCmd::FadeMs(100));
        let a = Arc::new(AudioOut::default());
        let (ia, _) = input(1, &a, 1.0, true);
        m.apply(MixerCmd::Inputs(vec![ia]));
        let out = Arc::new(Fifo::new(64));
        m.apply(MixerCmd::AddOutput { id: 1, queue: Arc::clone(&out) });
        let (ia, _) = input(1, &a, 1.0, false);
        m.apply(MixerCmd::Inputs(vec![ia]));
        let mut levels = vec![];
        for i in 0..8 {
            feed(&a, 1.0);
            m.tick(Tick(i));
            levels.push(out.pop().unwrap().samples[0]);
        }
        assert!(levels[0] > 0.5 && levels[0] < 1.0, "{levels:?}");
        assert_eq!(*levels.last().unwrap(), 0.0, "{levels:?}");
        assert!(levels.windows(2).all(|w| w[1] <= w[0]));
    }

    #[test]
    fn a_held_program_fades_out_keeps_the_hold_through_new_inputs_and_fades_back() {
        let mut m = MixState::new(Arc::new(Levels::default()));
        m.apply(MixerCmd::FadeMs(100));
        let (a, b) = (Arc::new(AudioOut::default()), Arc::new(AudioOut::default()));
        let (ia, _) = input(1, &a, 1.0, true);
        let (mut ib, _) = input(2, &b, 1.0, true);
        ib.monitor = MonitorMode::MonitorAndOutput;
        m.apply(MixerCmd::Inputs(vec![ia, ib]));
        let (set, stems, _) = tapped(&[1]);
        m.apply(MixerCmd::Taps { set: Some(set), generation: 1 });
        let (out, monitor) = (Arc::new(Fifo::new(64)), Arc::new(Fifo::new(64)));
        m.apply(MixerCmd::AddOutput { id: 1, queue: Arc::clone(&out) });
        m.apply(MixerCmd::MonitorOutput(Some(Arc::clone(&monitor))));
        // Each step gives the program's sample, the monitor's and input 1's stem.
        let step = |m: &mut MixState, i: i64| {
            feed(&a, 0.5);
            feed(&b, 0.25);
            m.tick(Tick(i));
            let first = |q: &Fifo<PooledBlock>| q.pop().map_or(-1.0, |x| x.samples[0]);
            (first(&out), first(&monitor), stems[0].pop().map_or(-1.0, |x| x.samples[0]))
        };
        assert_eq!(step(&mut m, 0), (0.75, 0.25, 0.5));
        m.apply(MixerCmd::Hold(true));
        let held: Vec<_> = (1..9).map(|i| step(&mut m, i)).collect();
        assert!(held[0].0 > 0.0 && held[0].0 < 0.75, "it fades: {held:?}");
        assert_eq!(held.last().map(|h| h.0), Some(0.0), "{held:?}");
        assert!(held.iter().all(|h| h.1 == 0.25), "a monitored input still plays on the monitor: {held:?}");
        assert!(held.iter().all(|h| h.2 == 0.5), "an input's own track keeps its audio: {held:?}");
        // A rebuilt input list, as a scene switch sends, keeps the hold; a new input stays out.
        let c = Arc::new(AudioOut::default());
        let (ia, _) = input(1, &a, 1.0, true);
        let (ic, _) = input(3, &c, 1.0, true);
        m.apply(MixerCmd::Inputs(vec![ia, ic]));
        feed(&c, 0.5);
        assert_eq!(step(&mut m, 9).0, 0.0, "the hold survives a new input list");
        m.apply(MixerCmd::Hold(false));
        let back: Vec<_> = (10..18)
            .map(|i| {
                feed(&c, 0.5);
                step(&mut m, i).0
            })
            .collect();
        assert!(back[0] > 0.0 && back[0] < 1.0, "it fades back: {back:?}");
        assert_eq!(back.last().copied(), Some(1.0), "inputs 1 and 3 are heard again: {back:?}");
    }

    #[test]
    fn a_runaway_input_is_held_to_a_few_blocks() {
        let mut m = MixState::new(Arc::new(Levels::default()));
        let a = Arc::new(AudioOut::default());
        let (ia, _) = input(1, &a, 1.0, true);
        m.apply(MixerCmd::Inputs(vec![ia]));
        for _ in 0..20 {
            feed(&a, 0.1);
        }
        m.tick(Tick(0));
        assert_eq!(a.queued(), MAX_QUEUED_BLOCKS - 1);
    }

    #[test]
    fn fader_moves_do_not_push_out_an_output_or_undo_a_newer_input_list() {
        let mut mixer = Mixer::detached();
        let mut m = MixState::new(Arc::new(Levels::default()));
        let a = Arc::new(AudioOut::default());
        let out = Arc::new(Fifo::new(8));
        mixer.send(MixerCmd::AddOutput { id: 1, queue: Arc::clone(&out) });
        for _ in 0..1000 {
            mixer.send(MixerCmd::Gain { source: SourceId(1), gain: 0.1, muted: false });
        }
        // The input list sent after the moves carries the newer gain.
        mixer.send(MixerCmd::Inputs(vec![input(1, &a, 1.0, true).0]));
        drain(mixer.queues(), &mixer.settings, &mut m);
        feed(&a, 0.5);
        m.tick(Tick(1));
        let block = out.pop();
        assert!(block.is_some_and(|b| (b.samples[0] - 0.5).abs() < 1e-6), "the output is registered and the gain is 1");
    }

    #[test]
    fn balance_and_mono_changes_do_not_push_out_an_output_or_undo_a_newer_input_list() {
        let mut mixer = Mixer::detached();
        let mut m = MixState::new(Arc::new(Levels::default()));
        let a = Arc::new(AudioOut::default());
        let out = Arc::new(Fifo::new(8));
        mixer.send(MixerCmd::AddOutput { id: 1, queue: Arc::clone(&out) });
        for _ in 0..=CONTROL_QUEUE {
            mixer.send(MixerCmd::Channels { source: SourceId(1), balance: -1.0, mono: true });
        }
        // The input list sent after the changes carries the newer balance and mono.
        mixer.send(MixerCmd::Inputs(vec![input(1, &a, 1.0, true).0]));
        drain(mixer.queues(), &mixer.settings, &mut m);
        feed(&a, 0.5);
        m.tick(Tick(1));
        let block = out.pop();
        assert!(
            block.is_some_and(|b| (b.samples[0] - 0.5).abs() < 1e-6 && (b.samples[1] - 0.5).abs() < 1e-6),
            "the output is registered and both channels keep the input list's balance"
        );
    }

    #[test]
    fn a_burst_of_fader_moves_does_not_drop_a_balance_change_or_the_reverse() {
        let mut mixer = Mixer::detached();
        let mut m = MixState::new(Arc::new(Levels::default()));
        let a = Arc::new(AudioOut::default());
        let out = Arc::new(Fifo::new(8));
        mixer.send(MixerCmd::AddOutput { id: 1, queue: Arc::clone(&out) });
        mixer.send(MixerCmd::Inputs(vec![input(1, &a, 1.0, true).0]));
        mixer.send(MixerCmd::Channels { source: SourceId(1), balance: -1.0, mono: false });
        for _ in 0..1000 {
            mixer.send(MixerCmd::Gain { source: SourceId(1), gain: 0.5, muted: false });
        }
        drain(mixer.queues(), &mixer.settings, &mut m);
        feed(&a, 0.8);
        m.tick(Tick(1));
        let got = first_frame(&out);
        assert!((got.0 - 0.4).abs() < 1e-6 && got.1.abs() < 1e-6, "the balance survives the fader moves: {got:?}");
        mixer.send(MixerCmd::Gain { source: SourceId(1), gain: 0.25, muted: false });
        for _ in 0..1000 {
            mixer.send(MixerCmd::Channels { source: SourceId(1), balance: 1.0, mono: false });
        }
        drain(mixer.queues(), &mixer.settings, &mut m);
        feed(&a, 0.8);
        m.tick(Tick(2));
        let got = first_frame(&out);
        assert!(got.0.abs() < 1e-6 && (got.1 - 0.2).abs() < 1e-6, "the gain survives the balance changes: {got:?}");
    }

    #[test]
    fn a_balance_change_reaches_the_list_in_use_when_a_newer_list_was_dropped() {
        let mut mixer = Mixer::detached();
        let mut m = MixState::new(Arc::new(Levels::default()));
        let a = Arc::new(AudioOut::default());
        let out = Arc::new(Fifo::new(8));
        mixer.send(MixerCmd::AddOutput { id: 1, queue: Arc::clone(&out) });
        mixer.send(MixerCmd::Inputs(vec![input(1, &a, 1.0, true).0]));
        drain(mixer.queues(), &mixer.settings, &mut m);
        // A newer list is pushed out of the full control queue before the mixer reads it.
        mixer.send(MixerCmd::Inputs(vec![input(1, &a, 1.0, true).0]));
        for _ in 0..CONTROL_QUEUE {
            mixer.send(MixerCmd::SyncOffset { source: SourceId(2), ms: 0 });
        }
        mixer.send(MixerCmd::Channels { source: SourceId(1), balance: -1.0, mono: false });
        drain(mixer.queues(), &mixer.settings, &mut m);
        feed(&a, 0.4);
        m.tick(Tick(1));
        let got = first_frame(&out);
        assert!((got.0 - 0.4).abs() < 1e-6 && got.1.abs() < 1e-6, "the list in use takes the change: {got:?}");
    }

    /// More input commands than the control queue holds do not push out an output, the monitoring
    /// output, the hold or the fade time sent before them. The input list that follows a drop
    /// carries only what the input commands carried.
    #[test]
    fn a_burst_of_input_commands_keeps_the_outputs_the_monitor_the_hold_and_the_fade() {
        let mut mixer = Mixer::detached();
        let mut m = MixState::new(Arc::new(Levels::default()));
        let a = Arc::new(AudioOut::default());
        let (out, monitor) = (Arc::new(Fifo::new(8)), Arc::new(Fifo::new(8)));
        mixer.send(MixerCmd::AddOutput { id: 1, queue: Arc::clone(&out) });
        mixer.send(MixerCmd::MonitorOutput(Some(Arc::clone(&monitor))));
        mixer.send(MixerCmd::FadeMs(0));
        mixer.send(MixerCmd::Hold(true));
        for _ in 0..=CONTROL_QUEUE {
            mixer.send(MixerCmd::SyncOffset { source: SourceId(1), ms: 0 });
        }
        assert!(mixer.control_dropped() > 0, "the input commands overflowed the control queue");
        let (mut i, _) = input(1, &a, 1.0, true);
        i.monitor = MonitorMode::MonitorAndOutput;
        mixer.send(MixerCmd::Inputs(vec![i]));
        drain(mixer.queues(), &mixer.settings, &mut m);
        feed(&a, 0.5);
        m.tick(Tick(1));
        let first = |q: &Fifo<PooledBlock>| q.pop().map(|b| b.samples[0]);
        assert_eq!(first(&out), Some(0.0), "the output is registered and the program is held without a fade");
        assert_eq!(first(&monitor), Some(0.5), "the monitoring output is set");
    }

    /// The mixer takes the fade time and hold from the sender's word only when it changed, and a
    /// hold comes with the fade time sent before it.
    #[test]
    fn the_fade_time_and_hold_are_read_when_they_change() {
        let mut mixer = Mixer::detached();
        let mut m = MixState::new(Arc::new(Levels::default()));
        m.apply(MixerCmd::Hold(true));
        drain(mixer.queues(), &mixer.settings, &mut m);
        assert!(m.held, "nothing new was sent");
        mixer.send(MixerCmd::FadeMs(0));
        drain(mixer.queues(), &mixer.settings, &mut m);
        assert!(!m.held && m.fade_step == 1.0, "the word holds the fade time and no hold");
        mixer.send(MixerCmd::FadeMs(300));
        mixer.send(MixerCmd::FadeMs(0));
        mixer.send(MixerCmd::Hold(true));
        drain(mixer.queues(), &mixer.settings, &mut m);
        assert!(m.held && m.fade_step == 1.0, "the hold comes with the latest fade time");
    }

    #[test]
    fn a_burst_of_balance_changes_of_other_sources_keeps_the_latest_change_of_each() {
        let mut mixer = Mixer::detached();
        let mut m = MixState::new(Arc::new(Levels::default()));
        let a = Arc::new(AudioOut::default());
        let b = Arc::new(AudioOut::default());
        let out = Arc::new(Fifo::new(8));
        mixer.send(MixerCmd::AddOutput { id: 1, queue: Arc::clone(&out) });
        mixer.send(MixerCmd::Inputs(vec![input(1, &a, 1.0, true).0, input(2, &b, 1.0, true).0]));
        mixer.send(MixerCmd::Channels { source: SourceId(1), balance: -1.0, mono: false });
        // More sources change than any queue of commands holds, source 2 many times.
        for k in 3..2000 {
            mixer.send(MixerCmd::Channels { source: SourceId(k), balance: 0.5, mono: false });
            mixer.send(MixerCmd::Channels { source: SourceId(2), balance: if k % 2 == 0 { 0.5 } else { 1.0 }, mono: false });
        }
        drain(mixer.queues(), &mixer.settings, &mut m);
        feed(&a, 0.4);
        m.tick(Tick(1));
        let got = first_frame(&out);
        assert!((got.0 - 0.4).abs() < 1e-6 && got.1.abs() < 1e-6, "source 1 stays fully left: {got:?}");
        // Source 2's last change, fully right, also applied.
        feed(&b, 0.4);
        m.tick(Tick(2));
        let got = first_frame(&out);
        assert!(got.0.abs() < 1e-6 && (got.1 - 0.4).abs() < 1e-6, "source 2 ends fully right: {got:?}");
    }

    #[test]
    fn replaced_input_lists_are_freed_by_the_sender() {
        let mut mixer = Mixer::detached();
        let mut m = MixState::new(Arc::new(Levels::default()));
        m.retired = Arc::clone(&mixer.retired);
        let a = Arc::new(AudioOut::default());
        mixer.send(MixerCmd::Inputs(vec![input(1, &a, 1.0, true).0]));
        mixer.send(MixerCmd::Inputs(vec![input(1, &a, 0.5, true).0]));
        drain(mixer.queues(), &mixer.settings, &mut m);
        assert_eq!(mixer.retired.len(), 2, "the mixer hands back the lists it replaced");
        mixer.send(MixerCmd::FadeMs(100));
        assert!(mixer.retired.is_empty(), "the next send frees them");
    }

    /// Scales its audio, and sets `dropped` when it is freed.
    struct Scale(f32, Arc<AtomicBool>);
    impl Scale {
        fn boxed(factor: f32) -> (Box<dyn FilterInstance>, Arc<AtomicBool>) {
            let dropped = Arc::new(AtomicBool::new(false));
            (Box::new(Scale(factor, Arc::clone(&dropped))), dropped)
        }
    }
    impl FilterInstance for Scale {
        fn update(&mut self, _: &studiocraft_model::Settings) -> Result<(), studiocraft_plugin::PluginError> {
            Ok(())
        }
        fn process_audio(&mut self, block: &mut [f32]) {
            block.iter_mut().for_each(|s| *s *= self.0);
        }
    }
    impl Drop for Scale {
        fn drop(&mut self) {
            self.1.store(true, Ordering::Relaxed);
        }
    }

    /// The samples of the oldest block in `out`; none when it is empty.
    fn mixed(out: &Fifo<PooledBlock>) -> Vec<f32> {
        out.pop().map(|b| b.samples.to_vec()).unwrap_or_default()
    }

    /// Plays each block one block late: a filter with a tail that outlasts its input.
    struct Echo(Vec<f32>);
    impl FilterInstance for Echo {
        fn update(&mut self, _: &studiocraft_model::Settings) -> Result<(), studiocraft_plugin::PluginError> {
            Ok(())
        }
        fn process_audio(&mut self, block: &mut [f32]) {
            for (s, held) in block.iter_mut().zip(&mut self.0) {
                std::mem::swap(s, held);
            }
        }
    }

    #[test]
    fn a_replaced_filter_crossfades_into_the_new_one_over_one_block() {
        let mut m = MixState::new(Arc::new(Levels::default()));
        let a = Arc::new(AudioOut::default());
        let (mut ia, _) = input(1, &a, 1.0, true);
        let (half, half_dropped) = Scale::boxed(0.5);
        ia.filters.push(half);
        m.apply(MixerCmd::Inputs(vec![ia]));
        let out = Arc::new(Fifo::new(8));
        m.apply(MixerCmd::AddOutput { id: 1, queue: Arc::clone(&out) });
        feed(&a, 0.8);
        m.tick(Tick(0));
        assert!(mixed(&out).iter().all(|s| (s - 0.4).abs() < 1e-6));
        let (whole, _) = Scale::boxed(1.0);
        m.apply(MixerCmd::ReplaceFilter { source: SourceId(1), index: 0, instance: whole });
        feed(&a, 0.8);
        m.tick(Tick(1));
        let block = mixed(&out);
        let left: Vec<f32> = block.iter().step_by(CHANNELS).copied().collect();
        assert_eq!(left.len(), BLOCK_FRAMES);
        // The first frame is still almost all the old filter's, the last is the new one's, and no
        // step between frames is larger than an even ramp's.
        let step = 0.4 / BLOCK_FRAMES as f32;
        assert!((left[0] - (0.4 + step)).abs() < 1e-5, "{}", left[0]);
        assert!((left[BLOCK_FRAMES - 1] - 0.8).abs() < 1e-5, "{}", left[BLOCK_FRAMES - 1]);
        assert!(left.windows(2).all(|w| w[1] >= w[0] && w[1] - w[0] < step + 1e-5));
        assert!(block.as_chunks::<CHANNELS>().0.iter().all(|f| f[0] == f[1]), "both channels fade together");
        assert!(!half_dropped.load(Ordering::Relaxed), "the mixer does not free the old filter");
        assert_eq!(m.retired_filters.len(), 1, "it goes back to the sending thread");
        feed(&a, 0.8);
        m.tick(Tick(2));
        assert!(mixed(&out).iter().all(|s| (s - 0.8).abs() < 1e-6), "the new settings only, from the next block");
    }

    /// A gain change on one filter keeps the state of the filters beside it: the tail an echo holds
    /// still plays after its input stops, where a new input list would have started it silent.
    #[test]
    fn replacing_one_filter_keeps_the_tails_of_the_others() {
        let mut m = MixState::new(Arc::new(Levels::default()));
        let a = Arc::new(AudioOut::default());
        let (mut ia, _) = input(1, &a, 1.0, true);
        ia.filters.push(Box::new(Echo(vec![0.0; BLOCK_SAMPLES])));
        ia.filters.push(Scale::boxed(0.5).0);
        m.apply(MixerCmd::Inputs(vec![ia]));
        let out = Arc::new(Fifo::new(8));
        m.apply(MixerCmd::AddOutput { id: 1, queue: Arc::clone(&out) });
        feed(&a, 0.6);
        m.tick(Tick(0));
        assert!(mixed(&out).iter().all(|s| *s == 0.0), "the echo holds the first block");
        m.apply(MixerCmd::ReplaceFilter { source: SourceId(1), index: 1, instance: Scale::boxed(0.5).0 });
        feed(&a, 0.0);
        m.tick(Tick(1));
        let tail = mixed(&out);
        assert!(tail.iter().all(|s| (s - 0.3).abs() < 1e-6), "the echo's tail plays: {:?}", tail.first());
    }

    #[test]
    fn replaced_filters_are_freed_by_the_sender_and_unknown_places_send_the_filter_back() {
        let mut mixer = Mixer::detached();
        let mut m = MixState::new(Arc::new(Levels::default()));
        m.retired_filters = Arc::clone(&mixer.retired_filters);
        let a = Arc::new(AudioOut::default());
        let (mut ia, _) = input(1, &a, 1.0, true);
        let (first, first_dropped) = Scale::boxed(0.5);
        ia.filters.push(first);
        mixer.send(MixerCmd::Inputs(vec![ia]));
        // Replaced twice before a block: the one in between was never heard and goes back at once.
        let (between, between_dropped) = Scale::boxed(0.25);
        mixer.send(MixerCmd::ReplaceFilter { source: SourceId(1), index: 0, instance: between });
        let (last, last_dropped) = Scale::boxed(1.0);
        mixer.send(MixerCmd::ReplaceFilter { source: SourceId(1), index: 0, instance: last });
        let (far, far_dropped) = Scale::boxed(1.0);
        mixer.send(MixerCmd::ReplaceFilter { source: SourceId(1), index: 1, instance: far });
        let (ghost, ghost_dropped) = Scale::boxed(1.0);
        mixer.send(MixerCmd::ReplaceFilter { source: SourceId(2), index: 0, instance: ghost });
        drain(mixer.queues(), &mixer.settings, &mut m);
        assert_eq!(mixer.retired_filters.len(), 3, "the unheard one, a place past the list and an unknown source");
        assert!(!between_dropped.load(Ordering::Relaxed) && !far_dropped.load(Ordering::Relaxed) && !ghost_dropped.load(Ordering::Relaxed));
        let out = Arc::new(Fifo::new(8));
        m.apply(MixerCmd::AddOutput { id: 1, queue: Arc::clone(&out) });
        feed(&a, 0.8);
        m.tick(Tick(0));
        // The output fades from the first filter's 0.4, never through the unheard 0.2.
        let left: Vec<f32> = mixed(&out).iter().step_by(CHANNELS).copied().collect();
        assert!(left.first().is_some_and(|s| *s > 0.4 && *s < 0.41), "{:?}", left.first());
        assert!(!first_dropped.load(Ordering::Relaxed));
        mixer.free_retired();
        assert!(mixer.retired_filters.is_empty());
        assert!(first_dropped.load(Ordering::Relaxed) && between_dropped.load(Ordering::Relaxed), "the sender frees them");
        assert!(far_dropped.load(Ordering::Relaxed) && ghost_dropped.load(Ordering::Relaxed));
        assert!(!last_dropped.load(Ordering::Relaxed), "the running filter stays");
    }

    /// A replacement made while the source sends no audio does not wait for its next block: the old
    /// filter goes back on the next tick, and the mix goes on.
    #[test]
    fn a_filter_replaced_while_its_source_is_silent_goes_back_on_the_next_block() {
        let mut m = MixState::new(Arc::new(Levels::default()));
        let (a, b) = (Arc::new(AudioOut::default()), Arc::new(AudioOut::default()));
        let (mut ia, _) = input(1, &a, 1.0, true);
        ia.filters.push(Scale::boxed(0.5).0);
        let (ib, _) = input(2, &b, 1.0, true);
        m.apply(MixerCmd::Inputs(vec![ia, ib]));
        let out = Arc::new(Fifo::new(8));
        m.apply(MixerCmd::AddOutput { id: 1, queue: Arc::clone(&out) });
        m.apply(MixerCmd::ReplaceFilter { source: SourceId(1), index: 0, instance: Scale::boxed(1.0).0 });
        feed(&b, 0.1);
        m.tick(Tick(0));
        assert_eq!(m.retired_filters.len(), 1, "the old filter is not held for a late block");
        assert!(mixed(&out).iter().all(|s| (s - 0.1).abs() < 1e-6), "the other source still plays");
        // The first block after the gap plays through the new filter alone.
        feed(&a, 0.8);
        m.tick(Tick(1));
        assert!(mixed(&out).iter().all(|s| (s - 0.8).abs() < 1e-6));
    }

    /// The room kept for the sending thread counts the filters waiting to crossfade: with the most
    /// of them waiting and a full control queue of replacements behind them, the mixer thread
    /// still frees no filter.
    #[test]
    fn filters_waiting_to_crossfade_fit_beside_a_full_control_queue() {
        let mut mixer = Mixer::detached();
        let mut m = MixState::new(Arc::new(Levels::default()));
        m.retired_filters = Arc::clone(&mixer.retired_filters);
        let dropped = Arc::new(AtomicBool::new(false));
        let scale = || -> Box<dyn FilterInstance> { Box::new(Scale(1.0, Arc::clone(&dropped))) };
        let a = Arc::new(AudioOut::default());
        let (mut ia, _) = input(1, &a, 1.0, true);
        ia.filters = (0..=FADING_FILTERS).map(|_| scale()).collect();
        mixer.send(MixerCmd::Inputs(vec![ia]));
        drain(mixer.queues(), &mixer.settings, &mut m);
        // Each filter is replaced once, a control queue at a time.
        let places: Vec<usize> = (0..=FADING_FILTERS).collect();
        for batch in places.chunks(CONTROL_QUEUE) {
            for &index in batch {
                mixer.send(MixerCmd::ReplaceFilter { source: SourceId(1), index, instance: scale() });
            }
            drain(mixer.queues(), &mixer.settings, &mut m);
        }
        assert_eq!(m.inputs[0].fading.len(), FADING_FILTERS, "the replacement past the limit does not wait");
        // Replacements of a filter that already waits go back at once.
        for _ in 0..CONTROL_QUEUE {
            mixer.send(MixerCmd::ReplaceFilter { source: SourceId(1), index: 0, instance: scale() });
        }
        // The sends freed what the mixer had handed back; from here on only the mixer runs.
        dropped.store(false, Ordering::Relaxed);
        drain(mixer.queues(), &mixer.settings, &mut m);
        feed(&a, 0.5);
        m.tick(Tick(0));
        assert_eq!(mixer.retired_filters.dropped(), 0, "the queue of retired filters overflowed");
        assert!(!dropped.load(Ordering::Relaxed), "the mixer thread freed a filter");
        mixer.free_retired();
        assert!(dropped.load(Ordering::Relaxed), "the sender frees them");
    }

    /// The most commands that can retire something between two frees by the sender: one the mixer
    /// took before the free and applies after it, a full control queue, and the sender's own
    /// command, which finds room because the mixer took one more between the free and the push.
    fn race_a_send(mixer: &Mixer, m: &mut MixState, mut cmd: impl FnMut() -> MixerCmd) {
        let taken = Cmd::from(cmd());
        for _ in 0..CONTROL_QUEUE {
            mixer.control.push((0, cmd().into()));
        }
        mixer.free_retired();
        if let Some((n, c)) = mixer.control.pop() {
            m.apply_sent(n, c);
        }
        mixer.control.push((0, cmd().into()));
        m.apply_sent(0, taken);
        drain(mixer.queues(), &mixer.settings, m);
    }

    /// With the most filters waiting to crossfade, the replacements a send that races the mixer
    /// leaves behind still go back to the sending thread.
    #[test]
    fn filters_retired_while_a_send_races_the_mixer_go_back_to_the_sender() {
        let mut mixer = Mixer::detached();
        let mut m = MixState::new(Arc::new(Levels::default()));
        m.retired_filters = Arc::clone(&mixer.retired_filters);
        let dropped = Arc::new(AtomicBool::new(false));
        let scale = || -> Box<dyn FilterInstance> { Box::new(Scale(1.0, Arc::clone(&dropped))) };
        let a = Arc::new(AudioOut::default());
        let (mut ia, _) = input(1, &a, 1.0, true);
        ia.filters = (0..FADING_FILTERS).map(|_| scale()).collect();
        mixer.send(MixerCmd::Inputs(vec![ia]));
        drain(mixer.queues(), &mixer.settings, &mut m);
        for index in 0..FADING_FILTERS {
            mixer.send(MixerCmd::ReplaceFilter { source: SourceId(1), index, instance: scale() });
            drain(mixer.queues(), &mixer.settings, &mut m);
        }
        assert_eq!(m.inputs[0].fading.len(), FADING_FILTERS);
        race_a_send(&mixer, &mut m, || MixerCmd::ReplaceFilter { source: SourceId(1), index: 0, instance: scale() });
        dropped.store(false, Ordering::Relaxed);
        feed(&a, 0.5);
        m.tick(Tick(0));
        assert_eq!(mixer.retired_filters.dropped(), 0, "the queue of retired filters overflowed");
        assert!(!dropped.load(Ordering::Relaxed), "the mixer thread freed a filter");
        mixer.free_retired();
        assert!(dropped.load(Ordering::Relaxed), "the sender frees them");
    }

    /// Commands fit while each queue holds them all: fader moves in theirs, input commands in the
    /// control queue, output commands in the output queue, and tap sets, fade times and holds in
    /// none.
    #[test]
    fn commands_fit_while_each_queue_holds_them() {
        let mixer = Mixer::detached();
        let moves = |n: usize| (0..n).map(|_| MixerCmd::Gain { source: SourceId(1), gain: 1.0, muted: false });
        let offsets = |n: usize| (0..n).map(|_| MixerCmd::SyncOffset { source: SourceId(1), ms: 0 });
        let removals = |n: usize| (0..n).map(|_| MixerCmd::RemoveOutput { id: 1 });
        let latest = |n: usize| (0..n).flat_map(|_| [MixerCmd::Taps { set: None, generation: 1 }, MixerCmd::FadeMs(0), MixerCmd::Hold(false)]);
        let gains = mixer.gains.capacity();
        let all = moves(gains).chain(offsets(CONTROL_QUEUE)).chain(removals(CONTROL_QUEUE)).chain(latest(CONTROL_QUEUE * 2));
        assert!(mixer.fits(&all.collect::<Vec<_>>()));
        assert!(!mixer.fits(&moves(gains + 1).collect::<Vec<_>>()));
        assert!(!mixer.fits(&offsets(CONTROL_QUEUE + 1).collect::<Vec<_>>()));
        assert!(!mixer.fits(&removals(CONTROL_QUEUE + 1).collect::<Vec<_>>()));
        assert!(mixer.fits(&[]));
    }

    /// Balance and mono changes go into no queue, so any number of them fits.
    #[test]
    fn balance_and_mono_changes_fit_in_any_number() {
        let mixer = Mixer::detached();
        let changes: Vec<_> = (0..CONTROL_QUEUE * 2).map(|_| MixerCmd::Channels { source: SourceId(1), balance: 0.5, mono: true }).collect();
        assert!(mixer.fits(&changes));
        let offsets = (0..CONTROL_QUEUE).map(|_| MixerCmd::SyncOffset { source: SourceId(1), ms: 0 });
        assert!(mixer.fits(&changes.into_iter().chain(offsets).collect::<Vec<_>>()));
    }

    /// The input lists a send that races the mixer leaves behind all go back to the sending thread.
    #[test]
    fn input_lists_retired_while_a_send_races_the_mixer_go_back_to_the_sender() {
        let mixer = Mixer::detached();
        let mut m = MixState::new(Arc::new(Levels::default()));
        m.retired = Arc::clone(&mixer.retired);
        let a = Arc::new(AudioOut::default());
        race_a_send(&mixer, &mut m, || MixerCmd::Inputs(vec![input(1, &a, 1.0, true).0]));
        assert_eq!(mixer.retired.dropped(), 0, "the queue of retired input lists overflowed");
    }

    /// Push `blocks` blocks of a 1 kHz tone with `peak`, the same on both channels, through `m`.
    fn play_tone(m: &mut MixState, a: &AudioOut, peak: f32, blocks: usize, first: &mut usize) {
        let frames = BLOCK_SAMPLES / CHANNELS;
        for _ in 0..blocks {
            let mut b = a.block();
            b.samples.clear();
            for i in *first..*first + frames {
                let v = peak * (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / f64::from(SAMPLE_RATE)).sin() as f32;
                b.samples.extend([v; CHANNELS]);
            }
            *first += frames;
            a.push(b);
            m.tick(Tick(*first as i64));
        }
    }

    /// EBU Tech 3341's first case: a 1 kHz stereo tone at −23 dBFS reads −23 LUFS momentary,
    /// short-term and integrated.
    #[test]
    fn the_master_meter_reads_the_loudness_of_the_mix() {
        let mut m = MixState::new(Arc::new(Levels::default()));
        assert_eq!(m.loudness.load().integrated_lufs, f64::NEG_INFINITY);
        let a = Arc::new(AudioOut::default());
        m.apply(MixerCmd::Inputs(vec![input(1, &a, 1.0, true).0]));
        let mut at = 0;
        // 470 blocks of 1024 frames at 48 kHz: 10 s.
        play_tone(&mut m, &a, 10f32.powf(-23.0 / 20.0), 470, &mut at);
        let r = m.loudness.load();
        for v in [r.momentary_lufs, r.short_term_lufs, r.integrated_lufs] {
            assert!((v - -23.0).abs() < 0.1, "{r:?}");
        }
        assert!((r.true_peak_dbtp - -23.0).abs() < 0.2, "{r:?}");
        assert!(r.range_lu.abs() < 0.1, "{r:?}");
        assert_eq!(r.generation, 470, "one publication per block");
    }

    /// A reset starts every reading again: the true peak of a −6 dB tone gives way to that of a
    /// −20 dB one, and the momentary and integrated readings wait for a full 400 ms of the new
    /// program before they read its level.
    #[test]
    fn a_reset_starts_every_reading_again() {
        let mixer = Mixer::detached();
        let mut m = MixState::new(Arc::new(Levels::default()));
        m.loudness = Arc::clone(mixer.loudness());
        let a = Arc::new(AudioOut::default());
        m.apply(MixerCmd::Inputs(vec![input(1, &a, 1.0, true).0]));
        let mut at = 0;
        play_tone(&mut m, &a, 0.5, 100, &mut at);
        let r = mixer.loudness().load();
        assert!((r.momentary_lufs - -6.0).abs() < 0.2 && (r.true_peak_dbtp - -6.0).abs() < 0.2, "{r:?}");
        mixer.loudness().request_reset();
        // 5 blocks: 5120 frames, one 100 ms step of the new program.
        play_tone(&mut m, &a, 0.1, 5, &mut at);
        let r = mixer.loudness().load();
        assert_eq!((r.integrated_lufs, r.momentary_lufs), (f64::NEG_INFINITY, f64::NEG_INFINITY), "{r:?}");
        assert!((r.true_peak_dbtp - -20.0).abs() < 0.2, "{r:?}");
        // 25 blocks in all: more than 400 ms.
        play_tone(&mut m, &a, 0.1, 20, &mut at);
        let r = mixer.loudness().load();
        assert!((r.momentary_lufs - -20.0).abs() < 0.2 && (r.integrated_lufs - -20.0).abs() < 0.2, "{r:?}");
    }

    /// A reader that runs while the mixer publishes gets readings from one block: every
    /// published set here holds its generation in all five fields.
    #[test]
    fn a_reader_never_sees_two_blocks_mixed() {
        let loudness = Arc::new(Loudness::default());
        let writer = {
            let loudness = Arc::clone(&loudness);
            std::thread::spawn(move || {
                for g in 1..=200_000u32 {
                    loudness.publish([f64::from(g); 5]);
                }
            })
        };
        let mut seen = 0;
        while !writer.is_finished() {
            let r = loudness.load();
            let all = [r.momentary_lufs, r.short_term_lufs, r.integrated_lufs, r.range_lu, r.true_peak_dbtp];
            if r.generation > 0 {
                assert!(all.iter().all(|&v| v == r.generation as f64), "{r:?}");
            }
            assert!(r.generation >= seen, "{r:?}");
            seen = r.generation;
        }
        writer.join().unwrap();
        assert_eq!(loudness.load().generation, 200_000);
    }

    /// Push one block whose samples count up from `first`, and mix it.
    fn ramp(m: &mut MixState, a: &AudioOut, first: &mut f32, at: i64) {
        let mut b = a.block();
        b.samples.clear();
        for _ in 0..BLOCK_SAMPLES {
            b.samples.push(*first);
            *first += 1.0;
        }
        a.push(b);
        m.tick(Tick(at));
    }

    /// The mix of one input, without clipping: gain 1e-6 keeps the ramp's values under 1.
    fn unclipped(out: &Fifo<PooledBlock>) -> Vec<f32> {
        out.pop().map(|b| b.samples.iter().map(|s| (s * 1e6).round()).collect()).unwrap_or_default()
    }

    #[test]
    fn a_sync_offset_delays_the_input_by_whole_frames() {
        let mut m = MixState::new(Arc::new(Levels::default()));
        let a = Arc::new(AudioOut::default());
        let (mut ia, _) = input(1, &a, 1e-6, true);
        // 10 ms: 480 frames, 960 samples, less than one block.
        ia.sync_offset_ms = 10;
        m.apply(MixerCmd::Inputs(vec![ia]));
        let out = Arc::new(Fifo::new(8));
        m.apply(MixerCmd::AddOutput { id: 1, queue: Arc::clone(&out) });
        let mut next = 1.0;
        ramp(&mut m, &a, &mut next, 0);
        let first = unclipped(&out);
        assert!(first[..960].iter().all(|s| *s == 0.0), "the delay starts silent");
        assert_eq!(first[960], 1.0, "the first sample comes 480 frames late");
        ramp(&mut m, &a, &mut next, 1);
        let second = unclipped(&out);
        // Sample `i` of the second block is input sample `2048 + i - 960`, counted from 1.
        assert_eq!(second[0], (BLOCK_SAMPLES - 960 + 1) as f32);
        assert_eq!(second[BLOCK_SAMPLES - 1], (2 * BLOCK_SAMPLES - 960) as f32);
    }

    #[test]
    fn a_changed_sync_offset_takes_effect_on_the_next_block_without_a_new_line() {
        let mut m = MixState::new(Arc::new(Levels::default()));
        let a = Arc::new(AudioOut::default());
        m.apply(MixerCmd::Inputs(vec![input(1, &a, 1e-6, true).0]));
        let out = Arc::new(Fifo::new(8));
        m.apply(MixerCmd::AddOutput { id: 1, queue: Arc::clone(&out) });
        let ring = m.inputs[0].delay.ring.as_ptr();
        let mut next = 1.0;
        ramp(&mut m, &a, &mut next, 0);
        assert_eq!(unclipped(&out)[0], 1.0, "no offset, no delay");
        // 64 ms is 3072 frames: three blocks.
        m.apply(MixerCmd::SyncOffset { source: SourceId(1), ms: 64 });
        for at in 1..5 {
            ramp(&mut m, &a, &mut next, at);
        }
        let blocks: Vec<Vec<f32>> = std::iter::from_fn(|| (!out.is_empty()).then(|| unclipped(&out))).collect();
        assert_eq!(blocks.len(), 4);
        // Block 4 (the fifth pushed) carries block 1's audio.
        assert_eq!(blocks[3][0], (BLOCK_SAMPLES + 1) as f32, "{:?}", &blocks[3][..4]);
        assert_eq!(m.inputs[0].delay.ring.as_ptr(), ring, "the delay line is the same allocation");
        // Beyond the longest offset, the mixer delays by the longest one.
        m.apply(MixerCmd::SyncOffset { source: SourceId(1), ms: u32::MAX });
        assert_eq!(m.inputs[0].delay_frames, offset_frames(MAX_SYNC_OFFSET_MS));
        assert_eq!(m.inputs[0].cfg.sync_offset_ms, MAX_SYNC_OFFSET_MS);
    }

    #[test]
    fn a_new_input_list_keeps_the_audio_a_delay_line_holds() {
        let mut mixer = Mixer::detached();
        let mut m = MixState::new(Arc::new(Levels::default()));
        m.retired = Arc::clone(&mixer.retired);
        let a = Arc::new(AudioOut::default());
        let delayed = |gain| {
            let (mut i, _) = input(1, &a, gain, true);
            // 32 ms is 1536 frames: one and a half blocks.
            i.sync_offset_ms = 32;
            i
        };
        m.apply(MixerCmd::Inputs(vec![delayed(1e-6)]));
        let out = Arc::new(Fifo::new(8));
        m.apply(MixerCmd::AddOutput { id: 1, queue: Arc::clone(&out) });
        let mut next = 1.0;
        ramp(&mut m, &a, &mut next, 0);
        ramp(&mut m, &a, &mut next, 1);
        // A filter or fader change sends a new list; the audio already delayed still plays.
        mixer.send(MixerCmd::Inputs(vec![delayed(1e-6)]));
        drain(mixer.queues(), &mixer.settings, &mut m);
        drop((out.pop(), out.pop()));
        ramp(&mut m, &a, &mut next, 2);
        assert_eq!(unclipped(&out)[0], (2 * BLOCK_SAMPLES - 1536 * CHANNELS + 1) as f32);
    }

    #[test]
    fn a_source_started_again_does_not_play_the_old_queue_s_delayed_audio() {
        let mut m = MixState::new(Arc::new(Levels::default()));
        let (old, new) = (Arc::new(AudioOut::default()), Arc::new(AudioOut::default()));
        let delayed = |q: &Arc<AudioOut>| {
            let (mut i, _) = input(1, q, 1e-6, true);
            i.sync_offset_ms = 32;
            i
        };
        m.apply(MixerCmd::Inputs(vec![delayed(&old)]));
        let out = Arc::new(Fifo::new(8));
        m.apply(MixerCmd::AddOutput { id: 1, queue: Arc::clone(&out) });
        let mut next = 1.0;
        ramp(&mut m, &old, &mut next, 0);
        ramp(&mut m, &old, &mut next, 1);
        m.apply(MixerCmd::Inputs(vec![delayed(&new)]));
        drop((out.pop(), out.pop()));
        ramp(&mut m, &new, &mut next, 2);
        // The offset is longer than a block, so the whole block is silent.
        assert!(unclipped(&out).iter().all(|s| *s == 0.0), "the new queue's line starts silent");
    }

    #[test]
    fn delayed_audio_still_plays_after_the_source_stops_sending() {
        let mut m = MixState::new(Arc::new(Levels::default()));
        let a = Arc::new(AudioOut::default());
        let (mut ia, _) = input(1, &a, 1e-6, true);
        // 32 ms is 1536 frames: one and a half blocks.
        ia.sync_offset_ms = 32;
        m.apply(MixerCmd::Inputs(vec![ia]));
        let out = Arc::new(Fifo::new(8));
        m.apply(MixerCmd::AddOutput { id: 1, queue: Arc::clone(&out) });
        let mut next = 1.0;
        ramp(&mut m, &a, &mut next, 0);
        // The source sends nothing more, as a media file at its end.
        for at in 1..4 {
            m.tick(Tick(at));
        }
        let blocks: Vec<Vec<f32>> = std::iter::from_fn(|| (!out.is_empty()).then(|| unclipped(&out))).collect();
        assert!(blocks[0].iter().all(|s| *s == 0.0));
        assert_eq!((blocks[1][BLOCK_SAMPLES / 2 - 1], blocks[1][BLOCK_SAMPLES / 2]), (0.0, 1.0), "the block plays 1536 frames late");
        assert_eq!(blocks[2][BLOCK_SAMPLES / 2 - 1], (BLOCK_SAMPLES) as f32, "the rest of the block plays after it");
        assert!(blocks[2][BLOCK_SAMPLES / 2..].iter().chain(&blocks[3]).all(|s| *s == 0.0));
        assert!(m.inputs[0].idle >= m.inputs[0].delay_frames, "once it has played, the line is not mixed again");
    }

    #[test]
    fn a_longer_offset_set_after_the_source_stops_still_plays_its_last_block() {
        let mut m = MixState::new(Arc::new(Levels::default()));
        let a = Arc::new(AudioOut::default());
        let (mut ia, _) = input(1, &a, 1e-6, true);
        ia.sync_offset_ms = 32;
        m.apply(MixerCmd::Inputs(vec![ia]));
        let out = Arc::new(Fifo::new(8));
        m.apply(MixerCmd::AddOutput { id: 1, queue: Arc::clone(&out) });
        let mut next = 1.0;
        ramp(&mut m, &a, &mut next, 0);
        // 64 ms is 3072 frames: the block's first frame is due at the start of block 3.
        m.apply(MixerCmd::SyncOffset { source: SourceId(1), ms: 64 });
        for at in 1..6 {
            m.tick(Tick(at));
        }
        let blocks: Vec<Vec<f32>> = std::iter::from_fn(|| (!out.is_empty()).then(|| unclipped(&out))).collect();
        assert!(blocks[..3].iter().flatten().all(|s| *s == 0.0));
        assert_eq!((blocks[3][0], blocks[3][BLOCK_SAMPLES - 1]), (1.0, BLOCK_SAMPLES as f32));
        assert!(blocks[4].iter().all(|s| *s == 0.0));
    }

    #[test]
    fn an_offset_set_after_a_gap_reads_silence_not_the_audio_before_it() {
        let mut m = MixState::new(Arc::new(Levels::default()));
        let a = Arc::new(AudioOut::default());
        m.apply(MixerCmd::Inputs(vec![input(1, &a, 1e-6, true).0]));
        let out = Arc::new(Fifo::new(8));
        m.apply(MixerCmd::AddOutput { id: 1, queue: Arc::clone(&out) });
        let mut next = 1.0;
        ramp(&mut m, &a, &mut next, 0);
        for at in 1..4 {
            m.tick(Tick(at));
        }
        m.apply(MixerCmd::SyncOffset { source: SourceId(1), ms: 10 });
        ramp(&mut m, &a, &mut next, 4);
        let last = std::iter::from_fn(|| (!out.is_empty()).then(|| unclipped(&out))).last().unwrap_or_default();
        assert!(last[..960].iter().all(|s| *s == 0.0), "{:?}", &last[..4]);
        assert_eq!(last[960], (BLOCK_SAMPLES + 1) as f32);
    }

    #[test]
    fn many_inputs_at_the_longest_offset_mix_without_growing() {
        let mut m = MixState::new(Arc::new(Levels::default()));
        let queues: Vec<Arc<AudioOut>> = (0..32).map(|_| Arc::new(AudioOut::default())).collect();
        let inputs = queues
            .iter()
            .enumerate()
            .map(|(n, q)| {
                let (mut i, _) = input(n as u64, q, 0.01, true);
                i.sync_offset_ms = MAX_SYNC_OFFSET_MS;
                i
            })
            .collect();
        m.apply(MixerCmd::Inputs(inputs));
        let out = Arc::new(Fifo::new(4));
        m.apply(MixerCmd::AddOutput { id: 1, queue: Arc::clone(&out) });
        let lens: Vec<usize> = m.inputs.iter().map(|i| i.delay.ring.capacity()).collect();
        // 5 s is 240,000 frames, 234.4 blocks: block 234 is the first whose end carries the
        // inputs' audio.
        for at in 0..236 {
            for q in &queues {
                feed(q, 1.0);
            }
            m.tick(Tick(at));
            let block = std::iter::from_fn(|| out.pop()).last();
            let heard = block.map(|b| b.samples[BLOCK_SAMPLES - 1]).unwrap_or_default();
            if at < 234 {
                assert_eq!(heard, 0.0, "block {at}");
            }
        }
        assert!((m.mix[BLOCK_SAMPLES - 1] - 0.32).abs() < 1e-4, "{}", m.mix[BLOCK_SAMPLES - 1]);
        assert_eq!(m.inputs.iter().map(|i| i.delay.ring.capacity()).collect::<Vec<_>>(), lens);
    }

    /// A mixer with one output and a monitoring queue, and the two queues.
    fn monitored(inputs: Vec<MixInput>) -> (MixState, Arc<Fifo<PooledBlock>>, Arc<Fifo<PooledBlock>>) {
        let mut m = MixState::new(Arc::new(Levels::default()));
        m.apply(MixerCmd::Inputs(inputs));
        let (out, mon) = (Arc::new(Fifo::new(8)), Arc::new(Fifo::new(8)));
        m.apply(MixerCmd::AddOutput { id: 1, queue: Arc::clone(&out) });
        m.apply(MixerCmd::MonitorOutput(Some(Arc::clone(&mon))));
        (m, out, mon)
    }

    fn first(q: &Fifo<PooledBlock>) -> f32 {
        q.pop().map(|b| b.samples[0]).unwrap_or(f32::NAN)
    }

    #[test]
    fn monitor_only_leaves_the_program_and_monitor_and_output_keeps_it() {
        let (a, b, c) = (Arc::new(AudioOut::default()), Arc::new(AudioOut::default()), Arc::new(AudioOut::default()));
        let (mut ia, la) = input(1, &a, 0.5, true);
        ia.monitor = MonitorMode::MonitorOnly;
        let (mut ib, _) = input(2, &b, 1.0, true);
        ib.monitor = MonitorMode::MonitorAndOutput;
        let (ic, _) = input(3, &c, 1.0, true);
        let (mut m, out, mon) = monitored(vec![ia, ib, ic]);
        feed(&a, 0.4);
        feed(&b, 0.1);
        feed(&c, 0.02);
        m.tick(Tick(7));
        assert!((first(&out) - (0.1 + 0.02)).abs() < 1e-6, "the program has the second and third inputs");
        let block = mon.pop().unwrap();
        assert_eq!(block.pts, Tick(7));
        assert!((block.samples[0] - (0.4 * 0.5 + 0.1)).abs() < 1e-6, "the monitor has the first two, at their volume");
        assert!((la.load().peak[0] - 0.2).abs() < 1e-6, "an input left out of the program is still metered");
    }

    #[test]
    fn the_monitor_honors_mute_and_the_sync_offset() {
        let a = Arc::new(AudioOut::default());
        let (mut ia, _) = input(1, &a, 1e-6, true);
        ia.monitor = MonitorMode::MonitorOnly;
        ia.sync_offset_ms = 10;
        let (mut m, out, mon) = monitored(vec![ia]);
        let mut next = 1.0;
        ramp(&mut m, &a, &mut next, 0);
        assert!(unclipped(&out).iter().all(|s| *s == 0.0), "monitor only: the program is silent");
        let heard = unclipped(&mon);
        assert!(heard[..960].iter().all(|s| *s == 0.0), "the monitor starts 480 frames late");
        assert_eq!(heard[960], 1.0);
        m.apply(MixerCmd::Gain { source: SourceId(1), gain: 1e-6, muted: true });
        ramp(&mut m, &a, &mut next, 1);
        assert!(unclipped(&mon).iter().all(|s| *s == 0.0), "a muted input is not monitored");
    }

    #[test]
    fn a_changed_mode_takes_effect_on_the_next_block() {
        let a = Arc::new(AudioOut::default());
        let (mut m, out, mon) = monitored(vec![input(1, &a, 1.0, true).0]);
        feed(&a, 0.25);
        m.tick(Tick(0));
        assert_eq!((first(&out), first(&mon)), (0.25, 0.0), "off: heard in the program only");
        m.apply(MixerCmd::Monitor { source: SourceId(1), mode: MonitorMode::MonitorOnly });
        feed(&a, 0.25);
        m.tick(Tick(1));
        assert_eq!((first(&out), first(&mon)), (0.0, 0.25));
        m.apply(MixerCmd::Monitor { source: SourceId(1), mode: MonitorMode::MonitorAndOutput });
        feed(&a, 0.25);
        m.tick(Tick(2));
        assert_eq!((first(&out), first(&mon)), (0.25, 0.25));
        m.apply(MixerCmd::MonitorOutput(None));
        feed(&a, 0.25);
        m.tick(Tick(3));
        assert_eq!((first(&out), mon.len()), (0.25, 0), "without a monitoring queue nothing is sent to it");
    }

    #[test]
    fn many_monitored_inputs_add_up_and_are_clipped() {
        let queues: Vec<Arc<AudioOut>> = (0..32).map(|_| Arc::new(AudioOut::default())).collect();
        let inputs = queues
            .iter()
            .enumerate()
            .map(|(n, q)| {
                let (mut i, _) = input(n as u64, q, 1.0, true);
                i.monitor = MonitorMode::MonitorOnly;
                i
            })
            .collect();
        let (mut m, out, mon) = monitored(inputs);
        for q in &queues {
            feed(q, 0.01);
        }
        m.tick(Tick(0));
        assert!((first(&mon) - 0.32).abs() < 1e-4);
        assert_eq!(first(&out), 0.0);
        for q in &queues {
            feed(q, 0.5);
        }
        m.tick(Tick(1));
        assert_eq!(first(&mon), 1.0, "the monitoring mix is clipped like the program");
    }

    #[test]
    fn a_monitor_that_stops_reading_does_not_make_the_mixer_allocate() {
        let a = Arc::new(AudioOut::default());
        let (mut ia, _) = input(1, &a, 1.0, true);
        ia.monitor = MonitorMode::MonitorAndOutput;
        let (mut m, out, mon) = monitored(vec![ia]);
        let kept = m.pool.capacity();
        let mut held = None;
        for i in 0..1000 {
            drop(held.take());
            m.tick(Tick(i));
            held = out.pop();
        }
        assert!(m.pool.made() <= kept, "blocks made: {}", m.pool.made());
        assert_eq!(mon.len(), mon.capacity());
        assert_eq!(mon.dropped(), 1000 - mon.capacity() as u64);
    }

    fn tapped(sources: &[u64]) -> (Arc<TapSet>, Vec<Arc<TapSlot>>, Arc<TapSlot>) {
        let slots: Vec<Arc<TapSlot>> = sources.iter().map(|_| Arc::new(TapSlot::new())).collect();
        let program = Arc::new(TapSlot::new());
        let set = TapSet { program: Some(Arc::clone(&program)), inputs: sources.iter().map(|s| SourceId(*s)).zip(slots.iter().cloned()).collect() };
        (Arc::new(set), slots, program)
    }

    /// The stems of the program's inputs, after their filters, gain and mute, add up to the
    /// program mix; a muted input's stem is silence and a monitor-only input without a tap adds
    /// nothing.
    #[test]
    fn the_stems_of_the_program_add_up_to_the_program_mix() {
        let mut m = MixState::new(Arc::new(Levels::default()));
        let queues: Vec<Arc<AudioOut>> = (0..4).map(|_| Arc::new(AudioOut::default())).collect();
        let (mut a, _) = input(1, &queues[0], 0.5, true);
        a.filters.push(Box::new(Half));
        let (b, _) = input(2, &queues[1], 0.3, true);
        let (mut c, _) = input(3, &queues[2], 1.0, true);
        c.muted = true;
        let (mut d, _) = input(4, &queues[3], 1.0, true);
        d.monitor = MonitorMode::MonitorOnly;
        m.apply(MixerCmd::Inputs(vec![a, b, c, d]));
        let (set, stems, program) = tapped(&[1, 2, 3]);
        m.apply(MixerCmd::Taps { set: Some(set), generation: 1 });
        let out = Arc::new(Fifo::new(8));
        m.apply(MixerCmd::AddOutput { id: 1, queue: Arc::clone(&out) });
        for (n, q) in queues.iter().enumerate() {
            let mut blk = q.block();
            blk.samples.clear();
            blk.samples.extend((0..BLOCK_SAMPLES).map(|i| ((i * (n + 3)) % 97) as f32 / 400.0 - 0.1));
            q.push(blk);
        }
        m.tick(Tick(77));
        let mix = out.pop().map(|b| b.samples.clone()).unwrap_or_default();
        let blocks: Vec<PooledBlock> = stems.iter().filter_map(|s| s.pop()).collect();
        assert_eq!(blocks.len(), 3, "one block per tapped input");
        assert!(blocks.iter().all(|b| b.pts == Tick(77)));
        assert!(blocks[2].samples.iter().all(|s| *s == 0.0), "the muted input records silence");
        for i in 0..BLOCK_SAMPLES {
            let sum: f32 = blocks.iter().map(|b| b.samples[i]).sum();
            assert!((sum - mix[i]).abs() < 1e-6, "sample {i}: stems {sum}, program {}", mix[i]);
        }
        let p = program.pop().map(|b| (b.pts, b.samples.clone()));
        assert_eq!(p, Some((Tick(77), mix)), "the program's tap is the clipped program the outputs get");
    }

    #[test]
    fn a_tapped_source_that_sends_nothing_records_silence_and_untapped_inputs_cost_nothing() {
        let mut m = MixState::new(Arc::new(Levels::default()));
        let (a, b) = (Arc::new(AudioOut::default()), Arc::new(AudioOut::default()));
        m.apply(MixerCmd::Inputs(vec![input(1, &a, 1.0, true).0, input(2, &b, 1.0, true).0]));
        let (set, stems, _) = tapped(&[1]);
        m.apply(MixerCmd::Taps { set: Some(set), generation: 1 });
        feed(&b, 0.5);
        m.tick(Tick(5));
        assert_eq!(stems[0].pop().map(|b| (b.pts, b.samples.iter().all(|s| *s == 0.0))), Some((Tick(5), true)));
        assert!(m.inputs[1].tap.is_none(), "an input without a slot has no tap");
    }

    #[test]
    fn a_tap_that_stops_being_read_reclaims_its_own_blocks_and_leaves_the_outputs_alone() {
        let mut m = MixState::new(Arc::new(Levels::default()));
        let a = Arc::new(AudioOut::default());
        m.apply(MixerCmd::Inputs(vec![input(1, &a, 1.0, true).0]));
        let (set, stems, program) = tapped(&[1]);
        m.apply(MixerCmd::Taps { set: Some(set), generation: 1 });
        let (stalled, reading) = (Arc::new(Fifo::new(256)), Arc::new(Fifo::new(256)));
        m.apply(MixerCmd::AddOutput { id: 1, queue: Arc::clone(&stalled) });
        m.apply(MixerCmd::AddOutput { id: 2, queue: Arc::clone(&reading) });
        let kept = m.pool.capacity();
        let mut held = None;
        for i in 0..1000 {
            feed(&a, 0.25);
            drop(held.take());
            m.tick(Tick(i));
            held = reading.pop();
        }
        assert_eq!(m.pool.made(), kept, "the outputs' pool made no more blocks");
        for t in [&stems[0], &program] {
            assert_eq!(t.made(), studiocraft_media::tap::TAP_POOL_BLOCKS, "the tap's pool made no more blocks");
            assert_eq!(t.skipped(), 0);
            assert_eq!(t.queued(), studiocraft_media::tap::TAP_QUEUE_BLOCKS);
            assert_eq!(t.dropped(), 1000 - studiocraft_media::tap::TAP_QUEUE_BLOCKS as u64);
        }
        let newest = std::iter::from_fn(|| reading.pop()).last().map(|b| b.pts);
        assert_eq!(newest.or(held.map(|b| b.pts)), Some(Tick(999)), "the reading output got every block");
    }

    #[test]
    fn a_burst_of_replacements_is_all_handed_back_and_none_is_freed_on_the_mixer() {
        let mut mixer = Mixer::detached();
        let mut m = MixState::new(Arc::new(Levels::default()));
        m.retired = Arc::clone(&mixer.retired);
        m.retired_taps = Arc::clone(&mixer.retired_taps);
        let a = Arc::new(AudioOut::default());
        // Every command waits in the queue before the mixer runs. A tap set replaced before the
        // mixer took it is freed by the sender as it sends the next.
        let burst = 200;
        for n in 0..burst {
            mixer.send(MixerCmd::Taps { set: Some(tapped(&[1]).0), generation: n + 1 });
            mixer.send(MixerCmd::Inputs(vec![input(1, &a, 1.0, true).0]));
        }
        drain(mixer.queues(), &mixer.settings, &mut m);
        assert!(mixer.retired_taps.is_empty(), "only the last tap set reached the mixer, and it is in use");
        assert_eq!(mixer.retired.len(), burst as usize, "every replaced input list waits for the sender");
        let (set, _, _) = tapped(&[1]);
        mixer.send(MixerCmd::Taps { set: Some(set), generation: burst + 1 });
        drain(mixer.queues(), &mixer.settings, &mut m);
        assert_eq!(mixer.retired_taps.len(), 1, "the set the mixer replaced waits for the sender");
        mixer.free_retired();
        assert!(mixer.retired_taps.is_empty() && mixer.retired.is_empty());
    }

    #[test]
    fn tap_sets_are_applied_kept_across_new_input_lists_and_handed_back() {
        let mut mixer = Mixer::detached();
        let mut m = MixState::new(Arc::new(Levels::default()));
        m.retired = Arc::clone(&mixer.retired);
        m.retired_taps = Arc::clone(&mixer.retired_taps);
        m.taps_applied = Arc::clone(&mixer.taps_applied);
        let a = Arc::new(AudioOut::default());
        let (set, stems, _) = tapped(&[1]);
        mixer.send(MixerCmd::Taps { set: Some(set), generation: 1 });
        mixer.send(MixerCmd::Inputs(vec![input(1, &a, 1.0, true).0]));
        drain(mixer.queues(), &mixer.settings, &mut m);
        assert_eq!(mixer.taps_applied(), 1);
        feed(&a, 0.5);
        m.tick(Tick(1));
        assert!(stems[0].pop().is_some_and(|b| b.samples[0] == 0.5), "an input list sent after the taps is tapped");
        mixer.send(MixerCmd::Taps { set: None, generation: 2 });
        drain(mixer.queues(), &mixer.settings, &mut m);
        assert_eq!(mixer.taps_applied(), 2);
        assert!(m.inputs.iter().all(|i| i.tap.is_none()) && m.taps.is_none());
        assert_eq!(mixer.retired_taps.len(), 1, "the replaced set goes back to the sender");
        assert!(Arc::strong_count(&stems[0]) > 1, "the slot is still held, so the mixer did not free it");
        mixer.free_retired();
        assert!(mixer.retired_taps.is_empty());
        feed(&a, 0.5);
        m.tick(Tick(2));
        assert!(stems[0].pop().is_none(), "nothing is tapped after the taps are removed");
    }

    #[test]
    fn a_burst_of_commands_never_drops_the_removal_of_the_taps() {
        let mut mixer = Mixer::detached();
        let mut m = MixState::new(Arc::new(Levels::default()));
        m.retired_taps = Arc::clone(&mixer.retired_taps);
        m.taps_applied = Arc::clone(&mixer.taps_applied);
        let (set, stems, _) = tapped(&[1]);
        mixer.send(MixerCmd::Taps { set: Some(set), generation: 1 });
        drain(mixer.queues(), &mixer.settings, &mut m);
        mixer.send(MixerCmd::Taps { set: None, generation: 2 });
        // More commands than the control queue holds arrive before the mixer runs again.
        for _ in 0..CONTROL_QUEUE + 10 {
            mixer.send(MixerCmd::SyncOffset { source: SourceId(1), ms: 0 });
        }
        drain(mixer.queues(), &mixer.settings, &mut m);
        assert_eq!(mixer.taps_applied(), 2);
        assert!(m.taps.is_none());
        // Of two sets sent before the mixer runs, the later one applies, and the earlier one is
        // freed on the sending thread.
        let (a, _, _) = tapped(&[1]);
        let (b, _, _) = tapped(&[2]);
        let weak = Arc::downgrade(&a);
        mixer.send(MixerCmd::Taps { set: Some(a), generation: 3 });
        mixer.send(MixerCmd::Taps { set: Some(b), generation: 4 });
        assert!(weak.upgrade().is_none(), "the replaced set was freed by the sender");
        drain(mixer.queues(), &mixer.settings, &mut m);
        assert_eq!(mixer.taps_applied(), 4);
        drop(stems);
    }

    #[test]
    fn the_thread_runs_and_stops() {
        let mut mixer = Mixer::spawn(MasterClock::new()).unwrap();
        let out = Arc::new(Fifo::new(64));
        mixer.send(MixerCmd::AddOutput { id: 7, queue: Arc::clone(&out) });
        std::thread::sleep(std::time::Duration::from_millis(150));
        assert!(out.len() >= 3, "about 7 blocks in 150 ms, got {}", out.len());
        drop(mixer);
    }
}
