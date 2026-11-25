//! Audio monitoring: the mixer's monitoring mix played on an output device.
//!
//! The mixer pushes one block of the monitoring mix (48 kHz stereo) per tick into a bounded
//! queue. The device pulls samples from it on its own thread through a [`Player`], which converts
//! the rate to the device's by linear interpolation and maps the two channels onto the device's
//! channels, without allocating or locking. The player waits for [`PRIME_BLOCKS`] blocks before it
//! plays, and plays silence when the queue runs dry until it holds that many again.
//!
//! [`Monitor`] opens the device while a source is monitored and closes it when none is. The sink
//! opens and closes the device on a thread of its own, so the studio's tick does not wait for the
//! device. A device that fails to open, or that fails or disappears while it plays, is reported on
//! a later tick; monitoring stops and the mixer and the outputs carry on. The same device is not
//! tried again until the monitoring device setting or the set of monitored sources changes. When
//! the sink is still busy with earlier opens and closes, a later tick opens the device again once
//! the sink has room, without a notice.

use std::sync::Arc;

use studiocraft_engine::mixer::{Mixer, MixerCmd};
use studiocraft_media::audio::{CHANNELS, PooledBlock, SAMPLE_RATE};
use studiocraft_media::queue::Fifo;
use studiocraft_platform::PlatformError;
use studiocraft_platform::audio::{AudioSink, Format, Playing};

use crate::Notice;

/// Blocks of the monitoring mix queued for the device at most (about 85 ms); past it the oldest
/// is dropped.
pub const MONITOR_QUEUE_BLOCKS: usize = 4;

/// Blocks the player waits for before it plays, after it starts or runs dry (about 43 ms).
pub const PRIME_BLOCKS: usize = 2;

/// Errors a playing device reports, kept until the studio reads them.
const ERRORS_KEPT: usize = 4;

/// Pulls the monitoring mix from its queue in the device's format.
pub(crate) struct Player {
    queue: Arc<Fifo<PooledBlock>>,
    block: Option<PooledBlock>,
    /// The next frame of `block` to read.
    at: usize,
    /// The two mix frames the next device frame falls between, and how far it is from `prev`
    /// toward `cur` (0 to 1).
    prev: [f32; CHANNELS],
    cur: [f32; CHANNELS],
    phase: f64,
    rate: u32,
    /// Mix frames per device frame.
    step: f64,
    /// Whether the queue held [`PRIME_BLOCKS`] blocks since the player last ran dry.
    primed: bool,
}

impl Player {
    pub(crate) fn new(queue: Arc<Fifo<PooledBlock>>) -> Player {
        Player { queue, block: None, at: 0, prev: [0.0; CHANNELS], cur: [0.0; CHANNELS], phase: 1.0, rate: SAMPLE_RATE, step: 1.0, primed: false }
    }

    /// The next frame of the mix, or silence when the player is not primed or the queue is dry.
    fn next_frame(&mut self) -> [f32; CHANNELS] {
        loop {
            if let Some(b) = &self.block {
                let i = self.at.saturating_mul(CHANNELS);
                if let (Some(l), Some(r)) = (b.samples.get(i), b.samples.get(i + 1)) {
                    self.at += 1;
                    return [*l, *r];
                }
                // The block has played; dropping it returns it to the mixer's pool.
                self.block = None;
            }
            if !self.primed {
                return [0.0; CHANNELS];
            }
            match self.queue.pop() {
                Some(b) => {
                    self.block = Some(b);
                    self.at = 0;
                }
                None => {
                    self.primed = false;
                    return [0.0; CHANNELS];
                }
            }
        }
    }

    /// Fill `out`, interleaved samples in `format`.
    pub(crate) fn fill(&mut self, out: &mut [f32], format: Format) {
        let rate = format.rate.max(1);
        if rate != self.rate {
            self.rate = rate;
            self.step = f64::from(SAMPLE_RATE) / f64::from(rate);
        }
        if !self.primed && self.block.is_none() && self.queue.len() >= PRIME_BLOCKS {
            self.primed = true;
        }
        let channels = usize::from(format.channels.max(1));
        for frame in out.chunks_mut(channels) {
            while self.phase >= 1.0 {
                self.prev = self.cur;
                self.cur = self.next_frame();
                self.phase -= 1.0;
            }
            let t = self.phase as f32;
            let l = self.prev[0] + (self.cur[0] - self.prev[0]) * t;
            let r = self.prev[1] + (self.cur[1] - self.prev[1]) * t;
            match frame {
                [m] => *m = (l + r) * 0.5,
                [a, b, rest @ ..] => {
                    *a = l;
                    *b = r;
                    rest.iter_mut().for_each(|s| *s = 0.0);
                }
                [] => {}
            }
            self.phase += self.step;
        }
    }
}

/// The monitoring device: opened while a source is monitored.
pub(crate) struct Monitor {
    sink: Arc<dyn AudioSink>,
    playing: Option<Box<dyn Playing>>,
    /// The device `playing` plays on.
    device: String,
    /// What the playing device reported.
    errors: Arc<Fifo<String>>,
    /// The device that failed, not opened again until the setting or the monitored sources change.
    failed: Option<String>,
    /// The device whose open the sink refused as busy. [`Monitor::poll`] opens it again once the
    /// sink has room.
    busy: Option<String>,
}

impl Monitor {
    pub(crate) fn new(sink: Arc<dyn AudioSink>) -> Monitor {
        Monitor { sink, playing: None, device: String::new(), errors: Arc::new(Fifo::new(ERRORS_KEPT)), failed: None, busy: None }
    }

    /// Whether a device has started and is playing.
    pub(crate) fn playing(&self) -> bool {
        self.playing.as_ref().is_some_and(|p| p.started())
    }

    /// The playing device's format, once it has started.
    pub(crate) fn format(&self) -> Option<Format> {
        self.playing.as_ref().filter(|p| p.started()).map(|p| p.format())
    }

    /// Play on another sink from now on; the device is opened again on the next [`Monitor::sync`].
    pub(crate) fn set_sink(&mut self, sink: Arc<dyn AudioSink>, mixer: &Mixer) {
        self.stop(mixer);
        self.failed = None;
        self.busy = None;
        self.sink = sink;
    }

    fn stop(&mut self, mixer: &Mixer) {
        if self.playing.take().is_some() {
            mixer.send(MixerCmd::MonitorOutput(None));
        }
    }

    /// Play on `device` while `wanted` (a source is monitored), else close the device.
    pub(crate) fn sync(&mut self, wanted: bool, device: &str, mixer: &Mixer, notices: &mut Vec<Notice>) {
        if !wanted {
            self.stop(mixer);
            self.failed = None;
            self.busy = None;
            return;
        }
        // A busy device waits for its next try in `poll`.
        if self.busy.as_deref() == Some(device) {
            return;
        }
        self.busy = None;
        if self.failed.as_deref() == Some(device) {
            return;
        }
        if self.playing.is_some() {
            if self.device == device {
                return;
            }
            self.stop(mixer);
        }
        let queue = Arc::new(Fifo::new(MONITOR_QUEUE_BLOCKS));
        let errors = Arc::new(Fifo::new(ERRORS_KEPT));
        let mut player = Player::new(Arc::clone(&queue));
        let reported = Arc::clone(&errors);
        match self.sink.play(device, Box::new(move |out, format| player.fill(out, format)), Box::new(move |e| reported.push(e))) {
            Ok(p) => {
                mixer.send(MixerCmd::MonitorOutput(Some(queue)));
                self.playing = Some(p);
                self.device = device.to_owned();
                self.errors = errors;
                self.failed = None;
            }
            // The sink is still busy with earlier opens and closes; a poll tries again once it has
            // room.
            Err(PlatformError::Busy) => {
                self.busy = Some(device.to_owned());
                self.failed = None;
            }
            Err(e) => {
                self.failed = Some(device.to_owned());
                notices.push(Notice::new("Audio monitoring could not start: {error}", vec![("error", e.to_string())], true));
            }
        }
    }

    /// Open again a device the sink refused as busy, once the sink has room. Stop monitoring when
    /// the device could not open, or when the playing device reported an error.
    pub(crate) fn poll(&mut self, mixer: &Mixer, notices: &mut Vec<Notice>) {
        if self.busy.is_some()
            && !self.sink.busy()
            && let Some(device) = self.busy.take()
        {
            self.sync(true, &device, mixer, notices);
        }
        if let Some(e) = self.playing.as_ref().and_then(|p| p.open_error()).map(str::to_owned) {
            self.stop(mixer);
            self.failed = Some(self.device.clone());
            notices.push(Notice::new("Audio monitoring could not start: {error}", vec![("error", e)], true));
            return;
        }
        let Some(first) = self.errors.pop() else { return };
        while self.errors.pop().is_some() {}
        if self.playing.is_some() {
            self.stop(mixer);
            self.failed = Some(self.device.clone());
            notices.push(Notice::new("Audio monitoring stopped: {error}", vec![("error", first)], true));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, OnceLock};

    use studiocraft_media::audio::{BLOCK_FRAMES, BLOCK_SAMPLES, block_pool};
    use studiocraft_platform::audio::{Fill, OnError};

    use super::*;

    /// A queue holding `blocks` blocks whose frames count up from 1 (left) and down from -1 (right).
    fn queue_of(blocks: usize) -> Arc<Fifo<PooledBlock>> {
        let pool = block_pool();
        let q = Arc::new(Fifo::new(16));
        for b in 0..blocks {
            let mut block = pool.take();
            block.samples.clear();
            for f in 0..BLOCK_FRAMES {
                let n = (b * BLOCK_FRAMES + f + 1) as f32;
                block.samples.extend([n, -n]);
            }
            q.push(block);
        }
        q
    }

    const STEREO_48K: Format = Format { rate: 48_000, channels: 2 };

    #[test]
    fn at_the_mix_rate_the_frames_play_unchanged_one_frame_late() {
        let q = queue_of(2);
        let mut p = Player::new(Arc::clone(&q));
        let mut out = vec![9.0; 8];
        p.fill(&mut out, STEREO_48K);
        assert_eq!(out, [0.0, 0.0, 1.0, -1.0, 2.0, -2.0, 3.0, -3.0]);
        let mut rest = vec![0.0; (2 * BLOCK_FRAMES - 4) * 2];
        p.fill(&mut rest, STEREO_48K);
        assert_eq!(rest[rest.len() - 2..], [2047.0, -2047.0], "every frame of both blocks plays in order");
        assert!(q.is_empty());
    }

    #[test]
    fn the_player_waits_for_two_blocks_before_it_plays() {
        let q = queue_of(1);
        let mut p = Player::new(Arc::clone(&q));
        let mut out = vec![9.0; 64];
        p.fill(&mut out, STEREO_48K);
        assert!(out.iter().all(|s| *s == 0.0), "one block is not enough");
        assert_eq!(q.len(), 1, "the block waits in the queue");
    }

    #[test]
    fn a_dry_queue_plays_silence_and_the_player_waits_for_two_blocks_again() {
        let q = queue_of(2);
        let mut p = Player::new(Arc::clone(&q));
        let mut out = vec![0.0; (2 * BLOCK_FRAMES + 100) * 2];
        p.fill(&mut out, STEREO_48K);
        assert!(out[(2 * BLOCK_FRAMES + 1) * 2..].iter().all(|s| *s == 0.0), "after the last frame the player plays silence");
        let one = queue_of(1);
        q.push(one.pop().unwrap_or_else(|| panic!("a block")));
        let mut out = vec![9.0; 64];
        p.fill(&mut out, STEREO_48K);
        assert!(out.iter().all(|s| *s == 0.0), "one block after running dry is not enough");
    }

    #[test]
    fn another_device_rate_keeps_the_duration() {
        // 10 blocks at 48 kHz are 10240 frames; at 44.1 kHz they play as about 9408.
        let q = queue_of(10);
        let mut p = Player::new(Arc::clone(&q));
        let format = Format { rate: 44_100, channels: 2 };
        let mut out = vec![0.0; 20_000 * 2];
        p.fill(&mut out, format);
        let played = out.chunks(2).rposition(|f| f[0] != 0.0).map_or(0, |i| i + 1);
        assert!((9405..=9410).contains(&played), "{played}");
        // The interpolated ramp rises by 48000 / 44100 frames per device frame.
        let rise = out[200] - out[198];
        assert!((f64::from(rise) - 48_000.0 / 44_100.0).abs() < 1e-3, "{rise}");
    }

    #[test]
    fn mono_devices_get_both_channels_and_more_channels_get_silence() {
        let mut p = Player::new(queue_of(2));
        let mut mono = vec![9.0; 3];
        p.fill(&mut mono, Format { rate: 48_000, channels: 1 });
        assert_eq!(mono, [0.0, 0.0, 0.0], "left and right are 1 and -1");
        let mut p = Player::new(queue_of(2));
        let mut six = vec![9.0; 12];
        p.fill(&mut six, Format { rate: 48_000, channels: 6 });
        assert_eq!(six[6..], [1.0, -1.0, 0.0, 0.0, 0.0, 0.0]);
        let mut p = Player::new(queue_of(2));
        let mut odd = vec![9.0; 3];
        p.fill(&mut odd, Format { rate: 0, channels: 0 });
        assert!(odd.iter().all(|s| s.is_finite()), "a device format of zeros does not divide by zero");
    }

    /// How a held device's open ended: `None` while it is still opening.
    type Opened = Arc<OnceLock<Result<(), String>>>;

    /// A sink whose devices stay opening until the test sets how each open ended.
    #[derive(Default)]
    struct HeldSink {
        opened: Mutex<Vec<Opened>>,
    }

    impl HeldSink {
        fn count(&self) -> usize {
            self.opened.lock().unwrap().len()
        }

        fn last(&self) -> Opened {
            Arc::clone(self.opened.lock().unwrap().last().unwrap())
        }
    }

    struct HeldPlaying(Opened);

    impl Playing for HeldPlaying {
        fn format(&self) -> Format {
            STEREO_48K
        }

        fn started(&self) -> bool {
            self.0.get().is_some_and(|r| r.is_ok())
        }

        fn open_error(&self) -> Option<&str> {
            self.0.get().and_then(|r| r.as_ref().err()).map(String::as_str)
        }
    }

    impl AudioSink for HeldSink {
        fn play(&self, _: &str, _: Fill, _: OnError) -> Result<Box<dyn Playing>, PlatformError> {
            let opened = Arc::new(OnceLock::new());
            self.opened.lock().unwrap().push(Arc::clone(&opened));
            Ok(Box::new(HeldPlaying(opened)))
        }
    }

    fn held() -> (Monitor, Arc<HeldSink>, Mixer) {
        let sink = Arc::new(HeldSink::default());
        (Monitor::new(Arc::clone(&sink) as Arc<dyn AudioSink>), sink, Mixer::detached())
    }

    #[test]
    fn a_device_still_opening_is_not_playing_until_it_starts() {
        let (mut m, sink, mixer) = held();
        let mut notices = Vec::new();
        m.sync(true, "default", &mixer, &mut notices);
        assert_eq!(sink.count(), 1, "sync returns while the device opens");
        assert_eq!((m.playing(), m.format()), (false, None));
        m.poll(&mixer, &mut notices);
        m.sync(true, "default", &mixer, &mut notices);
        assert!(notices.is_empty(), "a device still opening is not an error");
        assert_eq!(sink.count(), 1, "a device still opening is not opened again");
        sink.last().set(Ok(())).unwrap();
        assert_eq!((m.playing(), m.format()), (true, Some(STEREO_48K)));
        m.sync(false, "default", &mixer, &mut notices);
        assert!(!m.playing());
    }

    #[test]
    fn a_device_that_fails_while_opening_is_reported_as_not_started() {
        let (mut m, sink, mixer) = held();
        let mut notices = Vec::new();
        m.sync(true, "default", &mixer, &mut notices);
        sink.last().set(Err("the device is busy".into())).unwrap();
        m.poll(&mixer, &mut notices);
        let texts: Vec<String> = notices.iter().filter(|n| n.error).map(|n| n.text()).collect();
        assert_eq!(texts, ["Audio monitoring could not start: the device is busy"]);
        assert!(!m.playing());
        m.poll(&mixer, &mut notices);
        m.sync(true, "default", &mixer, &mut notices);
        assert_eq!((notices.len(), sink.count()), (1, 1), "the failed device is reported once and not tried again");
        m.sync(false, "default", &mixer, &mut notices);
        m.sync(true, "default", &mixer, &mut notices);
        assert_eq!(sink.count(), 2, "turning monitoring off and on tries again");
    }

    /// A sink whose first opens return the given errors, in order, and whose later opens play.
    /// `busy()` reports `full`, which the test sets.
    struct BusySink {
        errors: Mutex<Vec<PlatformError>>,
        opens: Mutex<usize>,
        full: Mutex<bool>,
    }

    impl BusySink {
        /// Busy for the first `busy` opens.
        fn new(busy: usize) -> Arc<BusySink> {
            BusySink::failing(vec![PlatformError::Busy; busy])
        }

        fn failing(mut errors: Vec<PlatformError>) -> Arc<BusySink> {
            errors.reverse();
            Arc::new(BusySink { errors: Mutex::new(errors), opens: Mutex::new(0), full: Mutex::new(false) })
        }

        fn opens(&self) -> usize {
            *self.opens.lock().unwrap()
        }

        fn set_full(&self, full: bool) {
            *self.full.lock().unwrap() = full;
        }
    }

    impl AudioSink for BusySink {
        fn play(&self, _: &str, _: Fill, _: OnError) -> Result<Box<dyn Playing>, PlatformError> {
            *self.opens.lock().unwrap() += 1;
            match self.errors.lock().unwrap().pop() {
                Some(e) => Err(e),
                None => Ok(Box::new(FixedPlaying)),
            }
        }

        fn busy(&self) -> bool {
            *self.full.lock().unwrap()
        }
    }

    struct FixedPlaying;

    impl Playing for FixedPlaying {
        fn format(&self) -> Format {
            STEREO_48K
        }
    }

    #[test]
    fn a_busy_device_is_tried_again_on_a_later_tick_without_a_notice() {
        let sink = BusySink::new(2);
        let mut m = Monitor::new(Arc::clone(&sink) as Arc<dyn AudioSink>);
        let mixer = Mixer::detached();
        let mut notices = Vec::new();
        sink.set_full(true);
        m.sync(true, "default", &mixer, &mut notices);
        assert!(!m.playing());
        m.poll(&mixer, &mut notices);
        m.poll(&mixer, &mut notices);
        m.sync(true, "default", &mixer, &mut notices);
        assert_eq!(sink.opens(), 1, "a busy device is not opened again while the sink is full");
        sink.set_full(false);
        m.poll(&mixer, &mut notices);
        assert_eq!((sink.opens(), m.playing()), (2, false), "another open filled the sink first");
        m.poll(&mixer, &mut notices);
        assert_eq!((sink.opens(), m.playing()), (3, true), "the device is opened once the sink has room, without another sync");
        assert!(notices.is_empty(), "a busy device is not reported as a failure");
    }

    #[test]
    fn a_busy_device_is_not_tried_again_once_monitoring_is_off() {
        let sink = BusySink::new(1);
        let mut m = Monitor::new(Arc::clone(&sink) as Arc<dyn AudioSink>);
        let mixer = Mixer::detached();
        let mut notices = Vec::new();
        m.sync(true, "default", &mixer, &mut notices);
        m.sync(false, "default", &mixer, &mut notices);
        m.poll(&mixer, &mut notices);
        assert_eq!((sink.opens(), m.playing()), (1, false), "the device is not opened while nothing is monitored");
    }

    #[test]
    fn a_failed_device_is_tried_again_after_switching_through_a_busy_one() {
        let sink = BusySink::failing(vec![PlatformError::Failed("the device is gone".into()), PlatformError::Busy]);
        let mut m = Monitor::new(Arc::clone(&sink) as Arc<dyn AudioSink>);
        let mixer = Mixer::detached();
        let mut notices = Vec::new();
        m.sync(true, "a", &mixer, &mut notices);
        m.sync(true, "b", &mixer, &mut notices);
        m.sync(true, "a", &mixer, &mut notices);
        assert_eq!((sink.opens(), m.playing()), (3, true), "switching back to the failed device tries it again");
    }

    #[test]
    fn played_blocks_go_back_to_the_pool() {
        let pool = block_pool();
        let q = Arc::new(Fifo::new(8));
        for _ in 0..3 {
            q.push(pool.take());
        }
        let before = pool.free();
        let mut p = Player::new(Arc::clone(&q));
        let mut out = vec![0.0; BLOCK_SAMPLES * 2 + 2];
        p.fill(&mut out, STEREO_48K);
        assert_eq!(pool.free(), before + 2, "two played blocks are free again");
    }
}
