//! Sources: what scenes show and the mixer hears.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use studiocraft_media::audio::{AudioPool, PooledBlock, block_pool};
use studiocraft_media::clock::MasterClock;
use studiocraft_media::queue::{Fifo, Latest};
use studiocraft_media::video::{FramePool, PooledFrame, frame_pool};
use studiocraft_model::Settings;

use crate::{Plugin, PluginError};

/// What a source produces.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SourceCaps {
    pub video: bool,
    pub audio: bool,
}

/// A source type.
pub trait Source: Plugin {
    fn caps(&self) -> SourceCaps;

    /// A running instance. Called on the source's own thread, which then calls
    /// [`SourceInstance::produce`] at the instance's [`Pace`]; the instance never leaves that
    /// thread, so it may hold handles that are not `Send` (audio streams, OS capture objects).
    fn create(&self, settings: &Settings, io: &SourceIo) -> Result<Box<dyn SourceInstance>, PluginError>;
}

/// How often a source's thread calls [`SourceInstance::produce`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pace {
    /// At a fixed interval (capture sources, media playback).
    Every(Duration),
    /// Once after creation and after each settings change (colors, images, text, and sources
    /// whose own callbacks push their output, like audio devices).
    Static,
}

/// A running source.
pub trait SourceInstance {
    fn pace(&self) -> Pace;
    /// Publish what is current into `io`.
    fn produce(&mut self, io: &SourceIo) -> Result<(), PluginError>;
    /// New settings (already normalized against the schema).
    fn update(&mut self, settings: &Settings, io: &SourceIo) -> Result<(), PluginError>;
}

/// What a video source shows.
pub enum VideoContent {
    /// A frame from the source's pool.
    Frame(PooledFrame),
    /// A solid color, `[r, g, b, a]`, of a size (drawn by the GPU; no pixels move).
    Solid { rgba: [u8; 4], width: u32, height: u32 },
    /// Nothing (the source is not ready, or lost its device).
    Empty,
}

/// Where a source publishes video: a newest-wins mailbox the compositor reads, and the pool its
/// frames come from.
pub struct VideoOut {
    mailbox: Latest<VideoContent>,
    pool: Arc<FramePool>,
    /// Width in the high half, height in the low half.
    size: AtomicU64,
    /// Whether anything has been published, a picture or nothing.
    published: AtomicBool,
}

impl Default for VideoOut {
    fn default() -> Self {
        Self { mailbox: Latest::new(), pool: frame_pool(), size: AtomicU64::new(0), published: AtomicBool::new(false) }
    }
}

impl VideoOut {
    /// A frame to fill, reused from the pool.
    pub fn frame(&self) -> PooledFrame {
        self.pool.take()
    }

    pub fn publish(&self, content: VideoContent) {
        let size = match &content {
            VideoContent::Frame(f) => (f.width, f.height),
            VideoContent::Solid { width, height, .. } => (*width, *height),
            // Nothing is shown, so there is no size.
            VideoContent::Empty => (0, 0),
        };
        self.size.store((u64::from(size.0) << 32) | u64::from(size.1), Ordering::Relaxed);
        self.mailbox.publish(content);
        self.published.store(true, Ordering::Release);
    }

    /// Whether the source has published its first content, `Empty` included.
    pub fn published(&self) -> bool {
        self.published.load(Ordering::Acquire)
    }

    /// The newest content since the last call (compositor side).
    pub fn take(&self) -> Option<VideoContent> {
        self.mailbox.take()
    }

    /// The size of the last content published, if any.
    pub fn size(&self) -> Option<(u32, u32)> {
        let v = self.size.load(Ordering::Relaxed);
        let (w, h) = ((v >> 32) as u32, (v & 0xffff_ffff) as u32);
        (w > 0 && h > 0).then_some((w, h))
    }

    /// Frames the compositor never showed because a newer one replaced them.
    pub fn replaced(&self) -> u64 {
        self.mailbox.replaced()
    }

    pub fn pool(&self) -> &Arc<FramePool> {
        &self.pool
    }
}

/// Where a source publishes audio: a queue of 48 kHz stereo blocks the mixer drains.
pub struct AudioOut {
    queue: Fifo<PooledBlock>,
    pool: Arc<AudioPool>,
    /// While set, the mixer reads nothing from the queue.
    held: AtomicBool,
}

/// Blocks of audio a source may queue before the oldest is dropped (about 0.7 s).
pub const AUDIO_QUEUE_BLOCKS: usize = 32;

impl Default for AudioOut {
    fn default() -> Self {
        Self { queue: Fifo::new(AUDIO_QUEUE_BLOCKS), pool: block_pool(), held: AtomicBool::new(false) }
    }
}

impl AudioOut {
    /// A block to fill, reused from the pool.
    pub fn block(&self) -> PooledBlock {
        self.pool.take()
    }

    pub fn push(&self, block: PooledBlock) {
        self.queue.push(block);
    }

    /// The oldest queued block (mixer side), or `None` while the queue is held.
    pub fn pop(&self) -> Option<PooledBlock> {
        if self.held.load(Ordering::Acquire) {
            return None;
        }
        self.queue.pop()
    }

    /// The blocks the mixer can read: 0 while the queue is held.
    pub fn queued(&self) -> usize {
        if self.held.load(Ordering::Acquire) {
            return 0;
        }
        self.queue.len()
    }

    /// Hold the queue, or release it. While it is held the mixer reads nothing, and blocks pushed
    /// in the meantime wait in the queue (the oldest are dropped when it is full) until
    /// [`AudioOut::clear`] throws them away or the hold is released.
    pub fn hold(&self, on: bool) {
        self.held.store(on, Ordering::Release);
    }

    /// Throw away every queued block, held or not (source side).
    pub fn clear(&self) {
        while self.queue.pop().is_some() {}
    }

    /// Blocks dropped because the mixer fell behind.
    pub fn dropped(&self) -> u64 {
        self.queue.dropped()
    }

    /// Whether [`AudioOut::block`] would have to make a new block: the pool has made all it keeps
    /// and none is free.
    pub fn pool_exhausted(&self) -> bool {
        self.pool.exhausted()
    }
}

/// A source's outputs and the master clock, shared between its thread, any callback threads it
/// starts, the compositor and the mixer.
#[derive(Clone)]
pub struct SourceIo {
    pub clock: MasterClock,
    pub video: Arc<VideoOut>,
    pub audio: Arc<AudioOut>,
}

impl SourceIo {
    pub fn new(clock: MasterClock) -> Self {
        Self { clock, video: Arc::new(VideoOut::default()), audio: Arc::new(AudioOut::default()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_out_reports_the_size_of_the_last_content() {
        let v = VideoOut::default();
        assert_eq!(v.size(), None);
        v.publish(VideoContent::Solid { rgba: [0; 4], width: 640, height: 360 });
        assert_eq!(v.size(), Some((640, 360)));
        let mut f = v.frame();
        f.reshape(8, 4, studiocraft_media::video::PixelFormat::Rgba8);
        v.publish(VideoContent::Frame(f));
        assert_eq!(v.size(), Some((8, 4)));
        assert!(matches!(v.take(), Some(VideoContent::Frame(_))));
        assert_eq!(v.replaced(), 1);
    }

    #[test]
    fn a_held_audio_queue_gives_the_mixer_nothing() {
        let a = AudioOut::default();
        a.push(a.block());
        a.hold(true);
        a.push(a.block());
        assert_eq!(a.queued(), 0);
        assert!(a.pop().is_none(), "the mixer reads nothing while the queue is held");
        a.hold(false);
        assert_eq!(a.queued(), 2, "the blocks waited in the queue");
        a.hold(true);
        a.clear();
        a.hold(false);
        assert!(a.pop().is_none(), "clear throws the blocks away while the queue is held");
    }

    #[test]
    fn video_out_reports_its_first_content_even_when_it_is_empty() {
        let v = VideoOut::default();
        assert!(!v.published());
        v.publish(VideoContent::Empty);
        assert!(v.published(), "a source with nothing to show has still answered");
        assert_eq!(v.size(), None);
    }

    #[test]
    fn empty_content_clears_the_size() {
        let v = VideoOut::default();
        v.publish(VideoContent::Solid { rgba: [0; 4], width: 640, height: 360 });
        assert_eq!(v.size(), Some((640, 360)));
        v.publish(VideoContent::Empty);
        assert_eq!(v.size(), None, "a source that lost its picture has no size");
    }

    #[test]
    fn audio_out_is_a_bounded_queue() {
        let a = AudioOut::default();
        for _ in 0..(AUDIO_QUEUE_BLOCKS + 3) {
            let b = a.block();
            a.push(b);
        }
        assert_eq!(a.queued(), AUDIO_QUEUE_BLOCKS);
        assert_eq!(a.dropped(), 3);
    }
}
