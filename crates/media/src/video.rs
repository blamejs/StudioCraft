//! Video frames on the CPU side: what capture sources hand to the compositor and what the
//! compositor's read-back hands to encoders.

use std::sync::Arc;

use craft_time::Tick;

use crate::pool::{Pool, Pooled};

/// Pixel layouts used in the pipeline.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum PixelFormat {
    /// 8-bit red, green, blue, alpha; straight alpha.
    #[default]
    Rgba8,
    /// 8-bit blue, green, red, alpha (what Windows capture APIs return).
    Bgra8,
    /// 8-bit 4:2:0 luma plane followed by an interleaved chroma plane (what encoders take).
    Nv12,
}

impl PixelFormat {
    /// Bytes of a tightly packed `width` x `height` frame, or `None` on overflow.
    pub fn frame_bytes(self, width: u32, height: u32) -> Option<usize> {
        let (w, h) = (usize::try_from(width).ok()?, usize::try_from(height).ok()?);
        match self {
            PixelFormat::Rgba8 | PixelFormat::Bgra8 => w.checked_mul(h)?.checked_mul(4),
            PixelFormat::Nv12 => w.checked_mul(h)?.checked_mul(3).map(|n| n / 2),
        }
    }

    /// The name ffmpeg uses for this layout.
    pub fn ffmpeg_name(self) -> &'static str {
        match self {
            PixelFormat::Rgba8 => "rgba",
            PixelFormat::Bgra8 => "bgra",
            PixelFormat::Nv12 => "nv12",
        }
    }
}

/// Largest frame side accepted from a source.
pub const MAX_FRAME_SIDE: u32 = 16_384;

/// A tightly packed frame.
#[derive(Clone, Debug, Default)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub data: Vec<u8>,
    pub pts: Tick,
}

impl VideoFrame {
    /// Make the frame `width` x `height` in `format`, reusing its buffer; `false` when the size is
    /// out of range (the frame is then left empty).
    pub fn reshape(&mut self, width: u32, height: u32, format: PixelFormat) -> bool {
        let bytes =
            if width == 0 || height == 0 || width > MAX_FRAME_SIDE || height > MAX_FRAME_SIDE { None } else { format.frame_bytes(width, height) };
        match bytes {
            Some(n) => {
                self.width = width;
                self.height = height;
                self.format = format;
                self.data.resize(n, 0);
                true
            }
            None => {
                self.width = 0;
                self.height = 0;
                self.data.clear();
                false
            }
        }
    }

    /// Whether the buffer matches the size and format.
    pub fn is_consistent(&self) -> bool {
        self.width > 0 && self.height > 0 && self.format.frame_bytes(self.width, self.height) == Some(self.data.len())
    }
}

/// A pool of frames, shared by a source and the compositor that returns them.
pub type FramePool = Pool<VideoFrame>;

/// A frame on loan from a [`FramePool`].
pub type PooledFrame = Pooled<VideoFrame>;

/// A pool sized for a source that keeps a few frames in flight.
pub fn frame_pool() -> Arc<FramePool> {
    Pool::new(4, VideoFrame::default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reshape_reuses_and_checks() {
        let mut f = VideoFrame::default();
        assert!(f.reshape(4, 2, PixelFormat::Rgba8));
        assert_eq!(f.data.len(), 32);
        assert!(f.is_consistent());
        assert!(f.reshape(4, 2, PixelFormat::Nv12));
        assert_eq!(f.data.len(), 12);
        assert!(!f.reshape(0, 2, PixelFormat::Rgba8));
        assert!(!f.reshape(MAX_FRAME_SIDE + 1, 2, PixelFormat::Rgba8));
        assert!(!f.is_consistent());
    }
}
