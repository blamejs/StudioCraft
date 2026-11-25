//! StudioCraft's built-in sources and filters.
//!
//! | Id | What it shows or plays |
//! |---|---|
//! | `color` | a solid color of a size |
//! | `image` | a picture file |
//! | `text` | text in an installed font, with an outline and a background |
//! | `media` | a video or audio file, played through ffmpeg |
//! | `display_capture` | a display |
//! | `window_capture` | one program window |
//! | `staged_windows` | one of up to six windows or displays captured at once, swapped by `present.*` commands (Windows and macOS) |
//! | `camera` | a video capture device, such as a webcam or a capture card |
//! | `audio_input` | a microphone or line input |
//! | `audio_output` | what a speaker or headphone device plays (Windows) |
//! | `app_audio` | what one application plays (Windows 11, Windows Server 2022 and Linux with PipeWire) |
//! | `captions` | the live captions of the program audio, from the studio's [`CaptionBoard`] |
//!
//! Filters: `gain`, `noise_gate`, `compressor`, `limiter`, `equalizer` (audio), `color_correction`,
//! `chroma_key`, `color_key`, `crop_pad`, `sharpen`, `image_mask`, `scroll` (video).

mod app_audio;
mod audio;
mod camera;
pub mod captions;
mod capture;
mod color;
pub mod filters;
mod image;
mod media;
mod staged;
mod text;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use studiocraft_media::Tick;
use studiocraft_media::audio::{BLOCK_FRAMES, MAX_RATIO_OFFSET, PooledBlock, Reblocker, SAMPLE_RATE};
use studiocraft_media::clock::MasterClock;
use studiocraft_model::Settings;
use studiocraft_plugin::{AUDIO_QUEUE_BLOCKS, AudioOut, PluginError, Registry};

pub use app_audio::AppAudioSource;
pub use audio::{AudioInput, AudioOutput};
pub use camera::CameraSource;
pub use captions::{CaptionBoard, CaptionsSource};
pub use capture::{DisplayCapture, WindowCapture};
pub use color::ColorSource;
pub use image::ImageSource;
pub use media::MediaSource;
pub use staged::StagedWindows;
pub use text::TextSource;

/// The ffmpeg location from Settings › Output (empty: search for it), shared with the sources
/// that run ffmpeg.
pub type FfmpegSetting = Arc<RwLock<String>>;

/// Register every built-in source and filter. The Live Captions sources draw the text on `captions`.
pub fn register_all(reg: &mut Registry, ffmpeg: &FfmpegSetting, captions: &Arc<CaptionBoard>) -> Result<(), PluginError> {
    reg.register_source(Arc::new(ColorSource))?;
    reg.register_source(Arc::new(ImageSource))?;
    reg.register_source(Arc::new(TextSource))?;
    reg.register_source(Arc::new(MediaSource::new(Arc::clone(ffmpeg))))?;
    reg.register_source(Arc::new(DisplayCapture))?;
    reg.register_source(Arc::new(WindowCapture))?;
    // Elsewhere the system picker chooses each captured window, so there is no window list to stage from.
    if cfg!(any(windows, target_os = "macos")) {
        reg.register_source(Arc::new(StagedWindows))?;
    }
    reg.register_source(Arc::new(CameraSource))?;
    reg.register_source(Arc::new(AudioInput))?;
    reg.register_source(Arc::new(AudioOutput))?;
    reg.register_source(Arc::new(AppAudioSource::new(craft_app_audio::Discovery::system())))?;
    reg.register_source(Arc::new(CaptionsSource::new(Arc::clone(captions))))?;
    reg.register_filter(Arc::new(filters::Gain))?;
    reg.register_filter(Arc::new(filters::NoiseGate))?;
    reg.register_filter(Arc::new(filters::Compressor))?;
    reg.register_filter(Arc::new(filters::Limiter))?;
    reg.register_filter(Arc::new(filters::Equalizer))?;
    reg.register_filter(Arc::new(filters::ColorCorrection))?;
    reg.register_filter(Arc::new(filters::ChromaKey))?;
    reg.register_filter(Arc::new(filters::CropPad))?;
    reg.register_filter(Arc::new(filters::Sharpen))?;
    reg.register_filter(Arc::new(filters::ColorKey))?;
    reg.register_filter(Arc::new(filters::ImageMask))?;
    reg.register_filter(Arc::new(filters::Scroll))?;
    Ok(())
}

/// A whole-number setting as `u32`, clamped to `min..=max`.
pub(crate) fn uint(s: &Settings, key: &str, default: u32, min: u32, max: u32) -> u32 {
    s.i64(key).map_or(default, |v| u32::try_from(v.clamp(i64::from(min), i64::from(max))).unwrap_or(default))
}

/// A feed's counters, shared with the source that reports them.
#[derive(Debug, Default)]
pub struct FeedStats {
    /// Blocks dropped because one call completed more than [`AUDIO_QUEUE_BLOCKS`].
    pub dropped: AtomicU64,
    /// Blocks made past the block pool's capacity because every block it keeps was in use. The
    /// blocks the pool makes while it fills up to its capacity are not counted.
    pub pool_exhausted: AtomicU64,
}

/// The queue depth, in blocks, that a feed keeping in step holds on average: halfway between an
/// empty queue and the four blocks the mixer keeps.
const DRIFT_TARGET_BLOCKS: f64 = 2.5;
/// A queue deeper than this means the mixer is not reading the source, and the feed keeps its ratio.
const DRIFT_UNREAD_BLOCKS: usize = 8;
/// Seconds over which the measured queue depth is averaged.
const DRIFT_SMOOTHING_SECONDS: f64 = 2.0;
/// Ratio change per block of queue error.
const DRIFT_GAIN: f64 = 8.5e-4;
/// Ratio change per second per block of queue error.
const DRIFT_INTEGRAL_GAIN: f64 = 8.5e-6;
/// The fastest the ratio moves: 20 parts per million per second.
const DRIFT_SLEW_PER_SECOND: f64 = 20e-6;
/// The longest gap between two pushes that counts in full.
const DRIFT_MAX_STEP_SECONDS: f64 = 1.0;

/// Keeps a feed's queue for the mixer at [`DRIFT_TARGET_BLOCKS`] by nudging its reblocker's ratio,
/// for a device whose clock runs a little fast or slow against the mix.
#[derive(Debug, Default)]
struct DriftLoop {
    /// The smoothed queue depth in blocks, and the time of the push that measured it. `None` until
    /// the mixer reads the queue, and again after it stops.
    measured: Option<(f64, Tick)>,
    integral: f64,
}

impl DriftLoop {
    /// Blocks to add (positive) or take away (negative) before the first measurement is used, to
    /// start the queue at the target.
    fn start(level: f64) -> i64 {
        let off = DRIFT_TARGET_BLOCKS - level;
        if off.abs() > 0.5 { off.round() as i64 } else { 0 }
    }

    /// The ratio after a push that measured `level` blocks on average at `now`, from `ratio`.
    fn update(&mut self, level: f64, now: Tick, ratio: f64) -> f64 {
        let Some((smoothed, at)) = self.measured else {
            self.measured = Some((level, now));
            return ratio;
        };
        let dt = now.saturating_sub(at).seconds();
        if dt <= 0.0 {
            return ratio;
        }
        let dt = dt.min(DRIFT_MAX_STEP_SECONDS);
        let smoothed = smoothed + (level - smoothed) * (dt / DRIFT_SMOOTHING_SECONDS).min(1.0);
        self.measured = Some((smoothed, now));
        let error = DRIFT_TARGET_BLOCKS - smoothed;
        self.integral = (self.integral + DRIFT_INTEGRAL_GAIN * error * dt).clamp(-MAX_RATIO_OFFSET, MAX_RATIO_OFFSET);
        let wanted = 1.0 + (DRIFT_GAIN * error + self.integral).clamp(-MAX_RATIO_OFFSET, MAX_RATIO_OFFSET);
        let slew = DRIFT_SLEW_PER_SECOND * dt;
        ratio + (wanted - ratio).clamp(-slew, slew)
    }
}

/// Device samples to pipeline blocks. The last block a callback completes is stamped with the
/// clock, and each earlier one a block's duration before the one after it. After the block pool
/// has warmed up, a push does not allocate.
pub struct AudioFeed {
    reblocker: Reblocker,
    audio: Arc<AudioOut>,
    clock: MasterClock,
    /// The blocks of one callback, at most [`AUDIO_QUEUE_BLOCKS`]: the newest ones.
    pending: VecDeque<PooledBlock>,
    stats: Arc<FeedStats>,
    /// Set by [`AudioFeed::keep_in_step`].
    drift: Option<DriftLoop>,
}

impl AudioFeed {
    /// A feed of interleaved samples at `rate` with `channels` channels into `audio`.
    pub fn new(rate: u32, channels: usize, audio: Arc<AudioOut>, clock: MasterClock) -> Self {
        Self {
            reblocker: Reblocker::new(rate, channels),
            audio,
            clock,
            pending: VecDeque::with_capacity(AUDIO_QUEUE_BLOCKS),
            stats: Arc::default(),
            drift: None,
        }
    }

    /// Keep the feed in step with the mixer, for a device that runs on its own clock. While the
    /// mixer reads the queue, the feed keeps 2.5 blocks waiting on average. When the mixer starts
    /// reading, the feed adds silent blocks or drops the oldest ones to start there. After that it
    /// makes up to 0.1% more or fewer frames from the device's samples, and changes that amount by
    /// at most 20 parts per million per second. While the mixer is not reading, the feed keeps the
    /// ratio it had.
    pub fn keep_in_step(mut self) -> Self {
        self.drift = Some(DriftLoop::default());
        self
    }

    /// The ratio of output frames to the device's frames at its stated rate: 1 for a feed that does
    /// not keep in step.
    pub fn ratio(&self) -> f64 {
        self.reblocker.ratio()
    }

    /// The feed's counters.
    pub fn stats(&self) -> Arc<FeedStats> {
        Arc::clone(&self.stats)
    }

    /// Turn `samples` into blocks and queue them for the mixer.
    pub fn push(&mut self, samples: &[f32]) {
        let now = self.clock.now();
        self.push_at(samples, now);
    }

    /// [`AudioFeed::push`] with the clock read as `now`.
    fn push_at(&mut self, samples: &[f32], now: Tick) {
        let queued = self.audio.queued();
        let frames_before = self.reblocker.pending_frames();
        let (audio, pending, stats) = (&self.audio, &mut self.pending, &self.stats);
        self.reblocker.push(samples, |b| {
            // The queue keeps only its newest blocks, so the older blocks of a callback that
            // completes more are dropped here, and the block goes back to the pool.
            if pending.len() >= AUDIO_QUEUE_BLOCKS {
                pending.pop_front();
                stats.dropped.fetch_add(1, Ordering::Relaxed);
            }
            // Checked after a dropped block went back to the pool, so it counts only real misses.
            if audio.pool_exhausted() {
                stats.pool_exhausted.fetch_add(1, Ordering::Relaxed);
            }
            let mut block = audio.block();
            block.samples.clear();
            block.samples.extend_from_slice(b);
            pending.push_back(block);
        });
        if self.drift.is_some() {
            self.keep_step(queued, frames_before, now);
        }
        let step = Tick::from_units(BLOCK_FRAMES as i64, i64::from(SAMPLE_RATE));
        let last = self.pending.len().saturating_sub(1);
        for (i, mut block) in self.pending.drain(..).enumerate() {
            let back = i64::try_from(last - i).unwrap_or(i64::MAX);
            block.pts = now.saturating_sub(step.saturating_mul(back));
            self.audio.push(block);
        }
    }

    /// Measure the queue for this push's blocks and steer the ratio. `queued` blocks waited before
    /// the push and the reblocker held `frames_before` frames.
    fn keep_step(&mut self, queued: usize, frames_before: usize, now: Tick) {
        let Some(drift) = self.drift.as_mut() else { return };
        if queued > DRIFT_UNREAD_BLOCKS {
            drift.measured = None;
            return;
        }
        let block = BLOCK_FRAMES as f64;
        let before = queued as f64 + frames_before as f64 / block;
        let made = self.pending.len() as f64 + (self.reblocker.pending_frames() as f64 - frames_before as f64) / block;
        let mut level = before + made / 2.0;
        if drift.measured.is_none() {
            let mut change = DriftLoop::start(level);
            // Silent blocks go ahead of this push's blocks, so the times stay in order.
            while change > 0 && self.pending.len() < AUDIO_QUEUE_BLOCKS {
                let mut silent = self.audio.block();
                silent.clear();
                self.pending.push_front(silent);
                change -= 1;
                level += 1.0;
            }
            while change < 0 && self.audio.pop().is_some() {
                change += 1;
                level -= 1.0;
            }
        }
        let ratio = drift.update(level, now, self.reblocker.ratio());
        self.reblocker.set_ratio(ratio);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn everything_registers_once() {
        let mut reg = Registry::new();
        let ffmpeg = FfmpegSetting::default();
        let board = Arc::new(CaptionBoard::new());
        register_all(&mut reg, &ffmpeg, &board).unwrap();
        assert!(register_all(&mut reg, &ffmpeg, &board).is_err(), "a second registration is refused");
        for id in
            ["color", "image", "text", "media", "display_capture", "window_capture", "camera", "audio_input", "audio_output", "app_audio", "captions"]
        {
            assert!(reg.source(id).is_ok(), "{id}");
        }
        assert_eq!(reg.source("staged_windows").is_ok(), cfg!(any(windows, target_os = "macos")), "Staged windows where windows are listed");
    }

    #[test]
    fn blocks_from_one_callback_get_consecutive_times() {
        let audio = Arc::new(AudioOut::default());
        let clock = MasterClock::new();
        let mut feed = AudioFeed::new(SAMPLE_RATE, 2, Arc::clone(&audio), clock);
        let before = clock.now();
        // Three and a half blocks of stereo samples in one callback.
        feed.push(&vec![0.25; BLOCK_FRAMES * 2 * 7 / 2 + 2]);
        let after = clock.now();
        let pts: Vec<Tick> = std::iter::from_fn(|| audio.pop()).map(|b| b.pts).collect();
        assert_eq!(pts.len(), 3);
        let step = Tick::from_units(BLOCK_FRAMES as i64, i64::from(SAMPLE_RATE));
        assert_eq!(pts[1] - pts[0], step);
        assert_eq!(pts[2] - pts[1], step);
        assert!(pts[2] >= before && pts[2] <= after, "the last block is stamped when the callback ends");
    }

    #[test]
    fn a_callback_past_the_feed_capacity_drops_the_oldest_blocks_and_counts_them() {
        let audio = Arc::new(AudioOut::default());
        let mut feed = AudioFeed::new(SAMPLE_RATE, 2, Arc::clone(&audio), MasterClock::new());
        let capacity = feed.pending.capacity();
        // Four blocks more than the feed keeps in one callback, each filled with its own number, and
        // one frame more, since the reblocker holds the last frame back.
        let blocks = AUDIO_QUEUE_BLOCKS + 4;
        let samples: Vec<f32> = (0..=blocks).flat_map(|k| std::iter::repeat_n(k as f32 / 100.0, BLOCK_FRAMES * 2)).collect();
        feed.push(&samples);
        let kept: Vec<f32> = std::iter::from_fn(|| audio.pop()).map(|b| b.samples[0]).collect();
        assert_eq!(kept, (4..blocks).map(|k| k as f32 / 100.0).collect::<Vec<_>>(), "the newest blocks are kept");
        assert_eq!(feed.stats().dropped.load(Ordering::Relaxed), 4);
        assert_eq!(feed.pending.capacity(), capacity, "the buffer did not grow");
        assert_eq!(capacity, AUDIO_QUEUE_BLOCKS);
    }

    #[test]
    fn a_feed_counts_blocks_made_after_the_pool_ran_dry() {
        let audio = Arc::new(AudioOut::default());
        let mut feed = AudioFeed::new(SAMPLE_RATE, 2, Arc::clone(&audio), MasterClock::new());
        // Hold every block the pool keeps, so the next push finds it empty.
        let held: Vec<_> = std::iter::from_fn(|| (!audio.pool_exhausted()).then(|| audio.block())).collect();
        assert!(!held.is_empty());
        // Two blocks, and the frame the reblocker holds back.
        feed.push(&vec![0.0; (BLOCK_FRAMES * 2 + 1) * 2]);
        assert_eq!(feed.stats().pool_exhausted.load(Ordering::Relaxed), 2);
        drop(held);
    }

    #[test]
    fn a_block_dropped_for_capacity_is_reused_without_counting_a_miss() {
        let audio = Arc::new(AudioOut::default());
        let mut feed = AudioFeed::new(SAMPLE_RATE, 2, Arc::clone(&audio), MasterClock::new());
        // Leave exactly as many free blocks as the feed keeps.
        let mut held: Vec<_> = std::iter::from_fn(|| (!audio.pool_exhausted()).then(|| audio.block())).collect();
        held.truncate(held.len() - AUDIO_QUEUE_BLOCKS);
        // One block more than the feed keeps: the last one reuses the block the oldest one returns.
        feed.push(&vec![0.0; (BLOCK_FRAMES * (AUDIO_QUEUE_BLOCKS + 1) + 1) * 2]);
        assert_eq!(feed.stats().dropped.load(Ordering::Relaxed), 1);
        assert_eq!(feed.stats().pool_exhausted.load(Ordering::Relaxed), 0);
        drop(held);
    }

    #[test]
    fn a_callback_holds_at_most_a_queue_of_blocks() {
        let audio = Arc::new(AudioOut::default());
        let mut feed = AudioFeed::new(SAMPLE_RATE, 2, Arc::clone(&audio), MasterClock::new());
        let capacity = feed.pending.capacity();
        // Ten queues' worth of blocks in one callback, each block's samples set to its number over
        // 1000, within the range the reblocker keeps.
        let blocks = AUDIO_QUEUE_BLOCKS * 10;
        let level = |n: usize| n as f32 / 1000.0;
        let samples: Vec<f32> = (0..blocks * BLOCK_FRAMES * 2).map(|i| level(i / (BLOCK_FRAMES * 2))).collect();
        feed.push(&samples);
        // One frame more completes the last block: the reblocker holds one back.
        feed.push(&[0.0; 2]);
        assert_eq!(feed.pending.capacity(), capacity, "the callback's blocks are not all held at once");
        let first: Vec<f32> = std::iter::from_fn(|| audio.pop()).map(|b| b.samples[0]).collect();
        let newest = (blocks - AUDIO_QUEUE_BLOCKS..blocks).map(level).collect::<Vec<_>>();
        assert_eq!(first, newest, "the newest blocks are kept, in order");
    }

    /// The blocks the mixer keeps waiting (`studiocraft_engine::mixer::MAX_QUEUED_BLOCKS`).
    const MIXER_KEEPS: usize = 4;

    /// What a mixer reading a drifting device's feed saw over the last half of a run.
    struct DriftRun {
        feed: AudioFeed,
        /// Mixer ticks that found the queue empty.
        missing: u32,
        /// Blocks the mixer dropped because more than [`MIXER_KEEPS`] waited.
        dropped: u32,
    }

    /// A device `ppm` parts per million fast (negative: slow) sends callbacks of `chunk` frames to
    /// `feed` for `seconds` of test-clock time. From `read_from` seconds on, a mixer reads one block
    /// per block period and drops the oldest while more than [`MIXER_KEEPS`] wait. Counts start
    /// halfway through the run.
    fn drift_run(mut feed: AudioFeed, audio: &AudioOut, ppm: f64, chunk: usize, seconds: f64, read_from: f64) -> DriftRun {
        let device_rate = f64::from(SAMPLE_RATE) * (1.0 + ppm * 1e-6);
        let period = BLOCK_FRAMES as f64 / f64::from(SAMPLE_RATE);
        let samples = vec![0.25f32; chunk * 2];
        let (mut device, mut mixer) = (0.0, read_from);
        let (mut missing, mut dropped) = (0, 0);
        while device < seconds {
            let counted = device > seconds / 2.0;
            if mixer <= device {
                while audio.queued() > MIXER_KEEPS {
                    drop(audio.pop());
                    dropped += u32::from(counted);
                }
                if audio.pop().is_none() {
                    missing += u32::from(counted);
                }
                mixer += period;
            } else {
                feed.push_at(&samples, Tick::from_seconds_f64(device));
                device += chunk as f64 / device_rate;
            }
        }
        DriftRun { feed, missing, dropped }
    }

    fn drifting_feed(keep_in_step: bool) -> (AudioFeed, Arc<AudioOut>) {
        let audio = Arc::new(AudioOut::default());
        let feed = AudioFeed::new(SAMPLE_RATE, 2, Arc::clone(&audio), MasterClock::new());
        (if keep_in_step { feed.keep_in_step() } else { feed }, audio)
    }

    /// The feed's ratio is within `within` parts per million of the one that matches a device `ppm`
    /// fast, its smoothed queue depth is within a block of the target, and the mixer found every
    /// block it read in the last half.
    fn assert_settled(run: &DriftRun, ppm: f64, within: f64) {
        let true_ratio = 1.0 / (1.0 + ppm * 1e-6);
        let off = (run.feed.ratio() - true_ratio) * 1e6;
        assert!(off.abs() < within, "ratio {} is {off:.1} ppm from {true_ratio}", run.feed.ratio());
        let level = run.feed.drift.as_ref().and_then(|d| d.measured).map(|(level, _)| level);
        assert!(level.is_some_and(|l| (l - DRIFT_TARGET_BLOCKS).abs() <= 1.0), "queue depth {level:?}");
        assert_eq!((run.missing, run.dropped), (0, 0), "blocks missing and dropped in the last half");
    }

    #[test]
    fn a_fast_device_stays_in_step_with_the_mixer() {
        let (feed, audio) = drifting_feed(true);
        let run = drift_run(feed, &audio, 300.0, 480, 400.0, 0.0);
        assert_settled(&run, 300.0, 50.0);
    }

    #[test]
    fn a_slow_device_stays_in_step_with_the_mixer() {
        let (feed, audio) = drifting_feed(true);
        let run = drift_run(feed, &audio, -300.0, 480, 400.0, 0.0);
        assert_settled(&run, -300.0, 50.0);
    }

    #[test]
    fn a_device_near_the_limit_with_two_block_callbacks_stays_in_step() {
        // Two blocks a callback beat against the mixer's reads, so the ratio wanders more.
        let (feed, audio) = drifting_feed(true);
        let run = drift_run(feed, &audio, 900.0, 2048, 400.0, 0.0);
        assert_settled(&run, 900.0, 100.0);
    }

    #[test]
    fn a_feed_that_does_not_keep_in_step_loses_blocks_to_a_fast_device() {
        let (feed, audio) = drifting_feed(false);
        let run = drift_run(feed, &audio, 300.0, 480, 400.0, 0.0);
        assert_eq!(run.feed.ratio(), 1.0);
        assert!(run.dropped > 0, "the mixer dropped the extra blocks");
    }

    #[test]
    fn a_feed_starts_at_the_target_depth() {
        let (mut feed, audio) = drifting_feed(true);
        // The mixer is reading, and the queue is empty.
        feed.push_at(&[0.25; 480 * 2], Tick::ZERO);
        assert_eq!(audio.queued(), 2, "two silent blocks wait ahead of the first audio");
        let first = audio.pop().unwrap();
        assert!(first.samples.iter().all(|s| *s == 0.0));
        assert_eq!(first.samples.len(), studiocraft_media::audio::BLOCK_SAMPLES);
    }

    #[test]
    fn a_feed_the_mixer_is_not_reading_keeps_its_ratio_and_its_pushes_carry_on() {
        let (feed, audio) = drifting_feed(true);
        // The mixer is late: it starts reading after 60 s. Until then every push returns and the
        // queue fills to its capacity.
        let run = drift_run(feed, &audio, 300.0, 480, 60.0, f64::INFINITY);
        assert_eq!(audio.queued(), AUDIO_QUEUE_BLOCKS);
        // The ratio moved only while the first few blocks filled the queue.
        assert!((run.feed.ratio() - 1.0).abs() < 10e-6, "the ratio is kept while nothing reads the queue: {}", run.feed.ratio());
        assert!(run.feed.drift.as_ref().is_some_and(|d| d.measured.is_none()));
        // Once the mixer reads, the feed comes back to the target and stays in step.
        let run = drift_run(run.feed, &audio, 300.0, 480, 400.0, 0.0);
        assert_settled(&run, 300.0, 50.0);
    }

    #[test]
    fn a_clock_that_stands_still_or_goes_back_leaves_the_ratio_alone() {
        let mut drift = DriftLoop::default();
        assert_eq!(drift.update(0.0, Tick::from_seconds_f64(1.0), 1.0), 1.0, "the first measurement only starts the loop");
        assert_eq!(drift.update(0.0, Tick::from_seconds_f64(1.0), 1.0), 1.0);
        assert_eq!(drift.update(0.0, Tick::from_seconds_f64(0.5), 1.0), 1.0);
        // A long gap counts as one second: the ratio moves by 20 parts per million at most.
        let r = drift.update(0.0, Tick::from_seconds_f64(100.0), 1.0);
        assert!((r - 1.0 - DRIFT_SLEW_PER_SECOND).abs() < 1e-12, "{r}");
    }

    #[test]
    fn uint_clamps() {
        let s = Settings::new().with("w", -5).with("h", 99_999);
        assert_eq!(uint(&s, "w", 10, 1, 100), 1);
        assert_eq!(uint(&s, "h", 10, 1, 100), 100);
        assert_eq!(uint(&s, "x", 10, 1, 100), 10);
    }
}
