//! Converting the program picture for encoders and reading it back.
//!
//! `craft-gpu-yuv`'s two passes convert the program texture to NV12 (BT.709, limited range) at
//! the output size, the planes are copied into one of three staging buffers, and the buffer is
//! mapped without waiting. A later frame copies each mapped buffer into a pooled frame
//! and hands it to every encoder queue. When all three buffers are still in flight the frame is
//! skipped and counted, so a slow read-back never stalls compositing.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use craft_gpu_yuv::{Converter, Planes};
use studiocraft_media::Tick;
use studiocraft_media::pool::Pool;
use studiocraft_media::queue::Fifo;
use studiocraft_media::video::{FramePool, PixelFormat, PooledFrame, VideoFrame};

/// Where converted frames go and at what size.
#[derive(Clone)]
pub struct EncodeTarget {
    /// The output size; both sides even.
    pub width: u32,
    pub height: u32,
    /// One queue per running recording or stream.
    pub sinks: Vec<Arc<Fifo<Arc<PooledFrame>>>>,
}

const FREE: u8 = 0;
const PENDING: u8 = 1;
const MAPPED: u8 = 2;
const FAILED: u8 = 3;

/// Staging buffers in the ring.
pub const RING: usize = 3;

struct Slot {
    buffer: wgpu::Buffer,
    state: Arc<AtomicU8>,
    pts: Tick,
}

pub(crate) struct Nv12 {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) planes: Planes,
    slots: Vec<Slot>,
    /// The slot the next frame is copied into, and the oldest slot still to be read.
    next: usize,
    read: usize,
    pool: Arc<FramePool>,
    /// Shared handles for the frames given to the sinks, reused once every sink has let go.
    shared: Vec<Arc<PooledFrame>>,
    /// Most handles kept: as many as the sinks can hold at once, plus one.
    shared_keep: usize,
    pub(crate) sinks: Vec<Arc<Fifo<Arc<PooledFrame>>>>,
}

/// How many frames `sinks` can hold at once: each queue's capacity and the one its reader has
/// taken.
fn in_flight(sinks: &[Arc<Fifo<Arc<PooledFrame>>>]) -> usize {
    sinks.iter().map(|s| s.capacity() + 1).sum()
}

/// `frame` in a shared handle from `kept` that no sink holds any more, or in a new one while
/// fewer than `keep` have been made. The frame the reused handle held goes back to its pool.
fn share(kept: &mut Vec<Arc<PooledFrame>>, keep: usize, frame: PooledFrame) -> Arc<PooledFrame> {
    if let Some(handle) = kept.iter_mut().find(|h| Arc::strong_count(h) == 1 && Arc::weak_count(h) == 0)
        && let Some(inner) = Arc::get_mut(handle)
    {
        *inner = frame;
        return Arc::clone(handle);
    }
    let handle = Arc::new(frame);
    if kept.len() < keep {
        kept.push(Arc::clone(&handle));
    }
    handle
}

/// `target`'s size made even, with each side at least 2.
pub(crate) fn even_size(target: &EncodeTarget) -> (u32, u32) {
    ((target.width.max(2) + 1) & !1, (target.height.max(2) + 1) & !1)
}

impl Nv12 {
    /// A converter for `target`, or the reason the device cannot hold its planes.
    pub(crate) fn new(device: &wgpu::Device, converter: &Converter, target: EncodeTarget) -> Result<Nv12, craft_gpu_yuv::Error> {
        let (width, height) = even_size(&target);
        let planes = converter.planes(device, width, height)?;
        let size = planes.layout().buffer_size;
        let shared_keep = in_flight(&target.sinks) + 1;
        let slots = (0..RING)
            .map(|_| Slot {
                buffer: device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("nv12 readback"),
                    size,
                    usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                    mapped_at_creation: false,
                }),
                state: Arc::new(AtomicU8::new(FREE)),
                pts: Tick(0),
            })
            .collect();
        Ok(Nv12 {
            width,
            height,
            planes,
            slots,
            next: 0,
            read: 0,
            pool: Pool::new(shared_keep + RING + 1, VideoFrame::default),
            shared: Vec::with_capacity(shared_keep),
            shared_keep,
            sinks: target.sinks,
        })
    }

    /// Send frames to `sinks` from now on. When they can hold more frames than the current
    /// sinks, the pool and the kept handles grow to match, so full queues still do not make the
    /// frame loop allocate.
    pub(crate) fn set_sinks(&mut self, sinks: Vec<Arc<Fifo<Arc<PooledFrame>>>>) {
        let keep = in_flight(&sinks) + 1;
        if keep > self.shared_keep {
            self.pool = Pool::new(keep + RING + 1, VideoFrame::default);
            self.shared.reserve(keep.saturating_sub(self.shared.len()));
            self.shared_keep = keep;
        }
        self.sinks = sinks;
    }

    /// Whether a staging buffer is free for this frame.
    pub(crate) fn has_room(&self) -> bool {
        self.slots.get(self.next).is_some_and(|s| s.state.load(Ordering::Acquire) == FREE)
    }

    /// Record the copies of both planes into the next staging buffer; whether they were recorded.
    /// After a `true`, call [`Nv12::submitted`] once the encoder is submitted.
    pub(crate) fn copy(&mut self, encoder: &mut wgpu::CommandEncoder, pts: Tick) -> bool {
        let Some(slot) = self.slots.get_mut(self.next) else { return false };
        slot.pts = pts;
        self.planes.copy_to_buffer(encoder, &slot.buffer, 0).is_ok()
    }

    /// Start mapping the buffer [`Nv12::copy`] filled.
    pub(crate) fn submitted(&mut self) {
        let Some(slot) = self.slots.get(self.next) else { return };
        slot.state.store(PENDING, Ordering::Release);
        let state = Arc::clone(&slot.state);
        slot.buffer.map_async(wgpu::MapMode::Read, .., move |r| state.store(if r.is_ok() { MAPPED } else { FAILED }, Ordering::Release));
        self.next = (self.next + 1) % RING;
    }

    /// Hand every mapped buffer, oldest first, to the sinks.
    pub(crate) fn collect(&mut self, lost: &AtomicU64) {
        loop {
            let Some(slot) = self.slots.get(self.read) else { return };
            match slot.state.load(Ordering::Acquire) {
                MAPPED => {
                    let frame = self.read_slot(slot);
                    slot.buffer.unmap();
                    slot.state.store(FREE, Ordering::Release);
                    match frame {
                        Some(f) => {
                            let shared = share(&mut self.shared, self.shared_keep, f);
                            for s in &self.sinks {
                                s.push(Arc::clone(&shared));
                            }
                        }
                        None => {
                            lost.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                FAILED => {
                    slot.state.store(FREE, Ordering::Release);
                    lost.fetch_add(1, Ordering::Relaxed);
                }
                _ => return,
            }
            self.read = (self.read + 1) % RING;
        }
    }

    fn read_slot(&self, slot: &Slot) -> Option<PooledFrame> {
        let view = slot.buffer.get_mapped_range(..).ok()?;
        let mut frame = self.pool.take();
        if !frame.reshape(self.width, self.height, PixelFormat::Nv12) {
            return None;
        }
        frame.pts = slot.pts;
        self.planes.layout().unpack(&view, &mut frame.data).ok()?;
        Some(frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_handles_are_reused_once_the_sinks_let_go() {
        let pool: Arc<FramePool> = Pool::new(4, VideoFrame::default);
        let mut kept = Vec::new();
        let first = share(&mut kept, 4, pool.take());
        let held = share(&mut kept, 4, pool.take());
        let first_ptr = Arc::as_ptr(&first);
        drop(first);
        for _ in 0..100 {
            let again = share(&mut kept, 4, pool.take());
            assert_eq!(Arc::as_ptr(&again), first_ptr, "the handle the sinks let go of is reused");
        }
        assert_eq!(kept.len(), 2);
        assert_eq!(pool.made(), 3, "the frame a reused handle held goes back to the pool");
        drop(held);
    }

    #[test]
    fn enough_handles_are_kept_for_full_sink_queues() {
        let sinks = vec![Arc::new(Fifo::new(8)), Arc::new(Fifo::new(30))];
        let keep = in_flight(&sinks) + 1;
        let pool: Arc<FramePool> = Pool::new(keep + RING + 1, VideoFrame::default);
        let mut kept = Vec::new();
        // The second sink's reader stops: its queue fills and drops the oldest frames.
        let mut reading = None;
        let mut new_handles = 0;
        for _ in 0..500 {
            let handle = share(&mut kept, keep, pool.take());
            if !kept.iter().any(|k| Arc::ptr_eq(k, &handle)) {
                new_handles += 1;
            }
            for s in &sinks {
                s.push(Arc::clone(&handle));
            }
            reading = sinks[0].pop();
        }
        drop(reading);
        assert_eq!(new_handles, 0, "every handle came from the kept ones");
        assert!(pool.made() <= keep + 1, "{} frames made", pool.made());
    }

    #[test]
    fn a_new_sink_grows_the_pool_and_the_kept_handles() {
        // Machines without a GPU pass without checking.
        let Ok(gpu) = crate::Gpu::headless() else { return };
        let one = vec![Arc::new(Fifo::new(4))];
        let converter = crate::compositor::yuv_converter(&gpu.device);
        let mut n = Nv12::new(&gpu.device, &converter, EncodeTarget { width: 64, height: 48, sinks: one.clone() }).unwrap();
        let (keep, capacity) = (n.shared_keep, n.pool.capacity());
        n.set_sinks(vec![one[0].clone(), Arc::new(Fifo::new(30))]);
        assert_eq!(n.shared_keep, keep + 31);
        assert_eq!(n.pool.capacity(), capacity + 31);
        assert_eq!(n.sinks.len(), 2);
        // Fewer sinks keep what is already there.
        n.set_sinks(one);
        assert_eq!(n.shared_keep, keep + 31);
    }

    #[test]
    fn output_sizes_are_made_even() {
        let size = |width, height| even_size(&EncodeTarget { width, height, sinks: Vec::new() });
        assert_eq!(size(1920, 1080), (1920, 1080));
        assert_eq!(size(853, 479), (854, 480));
        assert_eq!(size(0, 1), (2, 2));
    }
}
