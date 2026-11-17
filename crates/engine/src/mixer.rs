//! The audio mixer thread.
//!
//! Once per block (1024 frames at 48 kHz, every 21.3 ms) on the master clock, the mixer takes
//! one block from each input's queue, runs the input's audio filters, applies its volume, mute
//! and transition fade, adds it to the mix and publishes the input's levels. The clipped mix
//! goes to every output's queue, stamped with the block's time.
//!
//! The interface talks to the thread through a lock-free command queue, so moving a fader never
//! waits on the mixer.
//!
//! The clipped mix also feeds a loudness meter (ITU-R BS.1770-4, EBU R 128), whose readings are
//! published after every block in [`Loudness`].

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use craft_audio_dsp::LoudnessMeter;
use studiocraft_media::audio::{AudioPool, BLOCK_SAMPLES, CHANNELS, PooledBlock, SAMPLE_RATE, block_pool, block_rate};
use studiocraft_media::clock::{MasterClock, Pacer};
use studiocraft_media::mix::{BlockLevels, Levels, clip, measure, mix_into};
use studiocraft_media::queue::Fifo;
use studiocraft_model::SourceId;
use studiocraft_plugin::{AudioOut, FilterInstance};

/// Blocks an input may have queued before the oldest are dropped, which bounds its latency to
/// about 85 ms when a device runs faster than the master clock.
pub const MAX_QUEUED_BLOCKS: usize = 4;

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
    /// How long inputs take to fade in or out when the program scene changes, in milliseconds.
    FadeMs(u32),
    AddOutput {
        id: u64,
        queue: Arc<Fifo<PooledBlock>>,
    },
    RemoveOutput {
        id: u64,
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
            MixerCmd::Inputs(list) => Cmd::Inputs(list.into_iter().map(|cfg| Input { cfg, fade: 0.0 }).collect()),
            other => Cmd::Mix(other),
        }
    }
}

/// Commands numbered in the order they were sent.
type CmdQueue = Fifo<(u64, Cmd)>;

/// Input lists the mixer replaced, freed by the sending thread.
type Retired = Fifo<Vec<Input>>;

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
    /// Everything else: inputs, outputs and the fade time. These are few, and none is dropped
    /// for a burst of fader moves.
    control: Arc<CmdQueue>,
    retired: Arc<Retired>,
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
        state.loudness = Arc::clone(&m.loudness);
        let (g, c, s) = (Arc::clone(&m.gains), Arc::clone(&m.control), Arc::clone(&m.stop));
        std::thread::Builder::new().name("audio mixer".into()).spawn(move || run(clock, &c, &g, &s, state))?;
        Ok(m)
    }

    /// A mixer without a thread, for when no thread can be started: it takes commands and
    /// mixes nothing.
    pub fn detached() -> Mixer {
        Mixer {
            gains: Arc::new(Fifo::new(256)),
            control: Arc::new(Fifo::new(1024)),
            retired: Arc::new(Fifo::new(RETIRED_LISTS)),
            sent: AtomicU64::new(0),
            master: Arc::new(Levels::default()),
            loudness: Arc::new(Loudness::default()),
            stop: Arc::new(AtomicBool::new(true)),
        }
    }

    pub fn send(&self, cmd: MixerCmd) {
        while let Some(list) = self.retired.pop() {
            drop(list);
        }
        let n = self.sent.fetch_add(1, Ordering::Relaxed) + 1;
        match cmd {
            MixerCmd::Gain { .. } => self.gains.push((n, cmd.into())),
            _ => self.control.push((n, cmd.into())),
        }
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
}

/// Replaced input lists held for the sending thread to free. A full queue frees the oldest on
/// the mixer thread.
const RETIRED_LISTS: usize = 16;

/// Outputs the mixer holds room for without allocating.
const OUTPUT_ROOM: usize = 8;

/// The mixer's state, separate from the thread so it can be tested block by block.
pub struct MixState {
    inputs: Vec<Input>,
    retired: Arc<Retired>,
    outputs: Vec<(u64, Arc<Fifo<PooledBlock>>)>,
    master: Arc<Levels>,
    meter: LoudnessMeter,
    loudness: Arc<Loudness>,
    pool: Arc<AudioPool>,
    mix: Vec<f32>,
    fade_step: f32,
    /// The number of the input list in use: a fader move sent before it is older than the
    /// gains the list carries.
    inputs_sent: u64,
}

impl MixState {
    pub fn new(master: Arc<Levels>) -> Self {
        let mut s = Self {
            inputs: Vec::new(),
            retired: Arc::new(Fifo::new(RETIRED_LISTS)),
            outputs: Vec::with_capacity(OUTPUT_ROOM),
            master,
            meter: LoudnessMeter::new(f64::from(SAMPLE_RATE), CHANNELS),
            loudness: Arc::new(Loudness::default()),
            pool: block_pool(),
            mix: vec![0.0; BLOCK_SAMPLES],
            fade_step: 1.0,
            inputs_sent: 0,
        };
        s.set_fade_ms(300);
        s
    }

    fn set_fade_ms(&mut self, ms: u32) {
        let blocks = (f64::from(ms) / 1000.0) * block_rate().as_f64();
        self.fade_step = if blocks < 1.0 { 1.0 } else { (1.0 / blocks) as f32 };
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
                    i.fade = self.inputs.iter().find(|o| o.cfg.source == i.cfg.source).map_or(active, |o| o.fade);
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
            MixerCmd::FadeMs(ms) => self.set_fade_ms(ms),
            MixerCmd::AddOutput { id, queue } => {
                self.outputs.retain(|(o, _)| *o != id);
                self.outputs.push((id, queue));
            }
            MixerCmd::RemoveOutput { id } => self.outputs.retain(|(o, _)| *o != id),
        }
    }

    /// Mix one block stamped `pts`.
    pub fn tick(&mut self, pts: studiocraft_media::Tick) {
        self.mix.iter_mut().for_each(|s| *s = 0.0);
        for input in &mut self.inputs {
            while input.cfg.audio.queued() > MAX_QUEUED_BLOCKS {
                drop(input.cfg.audio.pop());
            }
            let target = if input.cfg.active { 1.0 } else { 0.0 };
            input.fade = if input.fade < target { (input.fade + self.fade_step).min(target) } else { (input.fade - self.fade_step).max(target) };
            let Some(mut block) = input.cfg.audio.pop() else {
                input.cfg.levels.store(BlockLevels::default());
                continue;
            };
            if block.samples.len() != BLOCK_SAMPLES {
                block.clear();
            }
            for f in &mut input.cfg.filters {
                f.process_audio(&mut block.samples);
            }
            let gain = input.cfg.gain * input.fade;
            let levels = if input.cfg.muted || input.fade <= 0.0 {
                measure(&block.samples, input.cfg.gain)
            } else {
                mix_into(&mut self.mix, &block.samples, gain)
            };
            input.cfg.levels.store(levels);
        }
        clip(&mut self.mix);
        self.master.store(measure(&self.mix, 1.0));
        if self.loudness.reset.swap(false, Ordering::Relaxed) {
            self.meter.reset_program();
        }
        self.meter.process_interleaved(&self.mix);
        self.loudness.store(&self.meter);
        for (_, q) in &self.outputs {
            // An output that stops reading would make the pool allocate a block every tick; its
            // oldest unread block is dropped and reused instead. When this output holds none, the
            // oldest block of the output holding the most is reused.
            let reclaimed = if self.pool.exhausted() {
                q.reclaim().or_else(|| self.outputs.iter().max_by_key(|(_, o)| o.len()).and_then(|(_, o)| o.reclaim()))
            } else {
                None
            };
            let mut out = reclaimed.unwrap_or_else(|| self.pool.take());
            out.samples.clear();
            out.samples.extend_from_slice(&self.mix);
            out.pts = pts;
            q.push(out);
        }
    }
}

/// Apply the waiting commands: the control commands first, then the fader moves, skipping the
/// moves older than the input list in use.
fn drain(control: &CmdQueue, gains: &CmdQueue, state: &mut MixState) {
    while let Some((n, c)) = control.pop() {
        state.apply_sent(n, c);
    }
    while let Some((n, c)) = gains.pop() {
        state.apply_sent(n, c);
    }
}

fn run(clock: MasterClock, control: &CmdQueue, gains: &CmdQueue, stop: &AtomicBool, mut state: MixState) {
    let mut pacer = Pacer::new(clock, block_rate());
    while !stop.load(Ordering::Relaxed) {
        let t = pacer.wait();
        drain(control, gains, &mut state);
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
            MixInput { source: SourceId(source), audio: Arc::clone(audio), gain, muted: false, active, levels: Arc::clone(&levels), filters: vec![] },
            levels,
        )
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
        let mixer = Mixer::detached();
        let mut m = MixState::new(Arc::new(Levels::default()));
        let a = Arc::new(AudioOut::default());
        let out = Arc::new(Fifo::new(8));
        mixer.send(MixerCmd::AddOutput { id: 1, queue: Arc::clone(&out) });
        for _ in 0..1000 {
            mixer.send(MixerCmd::Gain { source: SourceId(1), gain: 0.1, muted: false });
        }
        // The input list sent after the moves carries the newer gain.
        mixer.send(MixerCmd::Inputs(vec![input(1, &a, 1.0, true).0]));
        drain(&mixer.control, &mixer.gains, &mut m);
        feed(&a, 0.5);
        m.tick(Tick(1));
        let block = out.pop();
        assert!(block.is_some_and(|b| (b.samples[0] - 0.5).abs() < 1e-6), "the output is registered and the gain is 1");
    }

    #[test]
    fn replaced_input_lists_are_freed_by_the_sender() {
        let mixer = Mixer::detached();
        let mut m = MixState::new(Arc::new(Levels::default()));
        m.retired = Arc::clone(&mixer.retired);
        let a = Arc::new(AudioOut::default());
        mixer.send(MixerCmd::Inputs(vec![input(1, &a, 1.0, true).0]));
        mixer.send(MixerCmd::Inputs(vec![input(1, &a, 0.5, true).0]));
        drain(&mixer.control, &mixer.gains, &mut m);
        assert_eq!(mixer.retired.len(), 2, "the mixer hands back the lists it replaced");
        mixer.send(MixerCmd::FadeMs(100));
        assert!(mixer.retired.is_empty(), "the next send frees them");
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

    #[test]
    fn the_thread_runs_and_stops() {
        let mixer = Mixer::spawn(MasterClock::new()).unwrap();
        let out = Arc::new(Fifo::new(64));
        mixer.send(MixerCmd::AddOutput { id: 7, queue: Arc::clone(&out) });
        std::thread::sleep(std::time::Duration::from_millis(150));
        assert!(out.len() >= 3, "about 7 blocks in 150 ms, got {}", out.len());
        drop(mixer);
    }
}
