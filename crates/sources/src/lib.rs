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
//! | `audio_input` | a microphone or line input |
//! | `audio_output` | what a speaker or headphone device plays (Windows) |
//!
//! Filters: `gain`, `noise_gate`, `compressor`, `limiter`, `equalizer` (audio), `color_correction`,
//! `chroma_key`, `color_key`, `crop_pad`, `sharpen`, `image_mask`, `scroll` (video).

mod audio;
mod capture;
mod color;
pub mod filters;
mod image;
mod media;
mod text;

use std::sync::{Arc, RwLock};

use studiocraft_media::Tick;
use studiocraft_media::audio::{BLOCK_FRAMES, PooledBlock, Reblocker, SAMPLE_RATE};
use studiocraft_media::clock::MasterClock;
use studiocraft_model::Settings;
use studiocraft_plugin::{AudioOut, PluginError, Registry};

pub use audio::{AudioInput, AudioOutput};
pub use capture::{DisplayCapture, WindowCapture};
pub use color::ColorSource;
pub use image::ImageSource;
pub use media::MediaSource;
pub use text::TextSource;

/// The ffmpeg location from Settings › Output (empty: search for it), shared with the sources
/// that run ffmpeg.
pub type FfmpegSetting = Arc<RwLock<String>>;

/// Register every built-in source and filter.
pub fn register_all(reg: &mut Registry, ffmpeg: &FfmpegSetting) -> Result<(), PluginError> {
    reg.register_source(Arc::new(ColorSource))?;
    reg.register_source(Arc::new(ImageSource))?;
    reg.register_source(Arc::new(TextSource))?;
    reg.register_source(Arc::new(MediaSource::new(Arc::clone(ffmpeg))))?;
    reg.register_source(Arc::new(DisplayCapture))?;
    reg.register_source(Arc::new(WindowCapture))?;
    reg.register_source(Arc::new(AudioInput))?;
    reg.register_source(Arc::new(AudioOutput))?;
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

/// Blocks one device callback usually completes; more grow the buffer once.
const FEED_BLOCKS: usize = 8;

/// Device samples to pipeline blocks. The last block a callback completes is stamped with the
/// clock, and each earlier one a block's duration before the one after it.
pub(crate) struct AudioFeed {
    reblocker: Reblocker,
    audio: Arc<AudioOut>,
    clock: MasterClock,
    pending: Vec<PooledBlock>,
}

impl AudioFeed {
    pub(crate) fn new(rate: u32, channels: usize, audio: Arc<AudioOut>, clock: MasterClock) -> Self {
        Self { reblocker: Reblocker::new(rate, channels), audio, clock, pending: Vec::with_capacity(FEED_BLOCKS) }
    }

    pub(crate) fn push(&mut self, samples: &[f32]) {
        let (audio, pending) = (&self.audio, &mut self.pending);
        self.reblocker.push(samples, |b| {
            let mut block = audio.block();
            block.samples.clear();
            block.samples.extend_from_slice(b);
            pending.push(block);
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
        register_all(&mut reg, &ffmpeg).unwrap();
        assert!(register_all(&mut reg, &ffmpeg).is_err(), "a second registration is refused");
        for id in ["color", "image", "text", "media", "display_capture", "window_capture", "audio_input", "audio_output"] {
            assert!(reg.source(id).is_ok(), "{id}");
        }
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
    fn uint_clamps() {
        let s = Settings::new().with("w", -5).with("h", 99_999);
        assert_eq!(uint(&s, "w", 10, 1, 100), 1);
        assert_eq!(uint(&s, "h", 10, 1, 100), 100);
        assert_eq!(uint(&s, "x", 10, 1, 100), 10);
    }
}
