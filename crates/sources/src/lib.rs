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
//! | `staged_windows` | one of up to six windows captured at once, swapped by `present.*` commands (Windows and macOS) |
//! | `camera` | a video capture device, such as a webcam or a capture card |
//! | `audio_input` | a microphone or line input |
//! | `audio_output` | what a speaker or headphone device plays (Windows) |
//! | `app_audio` | what one application plays (Windows 11 and Windows Server 2022) |
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
use studiocraft_media::audio::{BLOCK_FRAMES, PooledBlock, Reblocker, SAMPLE_RATE};
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
}

impl AudioFeed {
    /// A feed of interleaved samples at `rate` with `channels` channels into `audio`.
    pub fn new(rate: u32, channels: usize, audio: Arc<AudioOut>, clock: MasterClock) -> Self {
        Self { reblocker: Reblocker::new(rate, channels), audio, clock, pending: VecDeque::with_capacity(AUDIO_QUEUE_BLOCKS), stats: Arc::default() }
    }

    /// The feed's counters.
    pub fn stats(&self) -> Arc<FeedStats> {
        Arc::clone(&self.stats)
    }

    /// Turn `samples` into blocks and queue them for the mixer.
    pub fn push(&mut self, samples: &[f32]) {
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
        let now = self.clock.now();
        let step = Tick::from_units(BLOCK_FRAMES as i64, i64::from(SAMPLE_RATE));
        let last = self.pending.len().saturating_sub(1);
        for (i, mut block) in self.pending.drain(..).enumerate() {
            let back = i64::try_from(last - i).unwrap_or(i64::MAX);
            block.pts = now.saturating_sub(step.saturating_mul(back));
            self.audio.push(block);
        }
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

    #[test]
    fn uint_clamps() {
        let s = Settings::new().with("w", -5).with("h", 99_999);
        assert_eq!(uint(&s, "w", 10, 1, 100), 1);
        assert_eq!(uint(&s, "h", 10, 1, 100), 100);
        assert_eq!(uint(&s, "x", 10, 1, 100), 10);
    }
}
