//! Converting the program picture for encoders and reading it back.
//!
//! `craft-gpu-yuv`'s two passes convert the program texture to NV12 (BT.709, limited range) at
//! the output size, the planes are copied into one of three staging buffers, and the buffer is
//! mapped without waiting. A later frame copies each mapped buffer into a pooled frame
//! and hands it to every encoder queue. When all three buffers are still in flight the frame is
//! skipped and counted, so a slow read-back never stalls compositing.
//!
//! A hardware encoder on the compositor's adapter can take the frames on the GPU instead: the
//! planes are copied into an export ring's NV12 textures (`craft-gpu-interop`; on Linux, Vulkan
//! images whose memory is exported as DMA-BUFs for a VA-API encoder), and the one
//! encoder queue of [`Nv12::set_export`] gets a [`GpuFrame`]. The read-back then runs only for the queues of the
//! [`EncodeTarget`]. When the ring cannot be opened, or a copy or the encoder fails, the reason is
//! kept, the export stops, and the read-back runs again.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use craft_gpu_interop::{ExportFrame, Exporter, Fallback, Ticket};
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

/// A program frame in a texture of an export ring, and its time.
#[derive(Clone, Debug)]
pub struct GpuFrame {
    pub frame: ExportFrame,
    pub pts: Tick,
}

/// The export of the frames to the GPU queues, while it runs.
struct Export {
    exporter: Exporter,
    /// The one encoder that reads the ring: its released fence says which frames it has read.
    sink: Arc<Fifo<GpuFrame>>,
    /// The copy recorded for the frame being submitted, until it is handed out to be submitted.
    commands: Option<wgpu::CommandBuffer>,
    /// The frame of that copy and its time, until it is submitted.
    ticket: Option<(Ticket, Tick)>,
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
    export: Option<Export>,
    /// Why the frames stopped going to the GPU queues, once they have.
    pub(crate) fallback: Option<Fallback>,
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
            export: None,
            fallback: None,
        })
    }

    /// Send the frames to `sink`, one encoder's queue, on the GPU from now on, through an export
    /// ring opened on `device` at the output size; `None` stops the export. Another queue gets a
    /// new ring. When the ring cannot be opened, the reason is kept in [`Nv12::fallback`] and
    /// returned, and nothing goes to `sink`.
    pub(crate) fn set_export(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, sink: Option<Arc<Fifo<GpuFrame>>>) -> Result<(), Fallback> {
        let Some(sink) = sink else {
            self.export = None;
            return Ok(());
        };
        if self.export.as_ref().is_some_and(|e| Arc::ptr_eq(&e.sink, &sink)) {
            return Ok(());
        }
        self.export = None;
        match Exporter::open(device, queue, self.width, self.height) {
            Ok(mut exporter) => {
                exporter.reserve(device, self.planes.textures());
                self.fallback = None;
                self.export = Some(Export { exporter, sink, commands: None, ticket: None });
                Ok(())
            }
            Err(reason) => {
                self.fallback = Some(reason);
                Err(reason)
            }
        }
    }

    /// Whether frames go to GPU queues.
    pub(crate) fn exporting(&self) -> bool {
        self.export.is_some()
    }

    /// Whether this frame needs the read-back: there are read-back queues, or nothing takes the
    /// frames on the GPU.
    pub(crate) fn reads_back(&self) -> bool {
        !self.sinks.is_empty() || self.export.is_none()
    }

    /// Stop the export for `reason`, which is kept and recorded on the ring.
    fn stop_export(&mut self, reason: Fallback) {
        if let Some(export) = self.export.take() {
            export.exporter.ring().refuse(reason);
            log::info!("program frames go to the encoders through memory: {reason}");
        }
        self.fallback = Some(reason);
    }

    /// Record the copy of both planes into a free texture of the export ring, after the
    /// conversion passes in `encoder`. Returns whether a frame will reach the GPU queues; a frame
    /// for which every texture is still being encoded is counted in `lost`. A failure stops the
    /// export (see [`Nv12::fallback`]).
    pub(crate) fn export(&mut self, device: &wgpu::Device, encoder: &mut wgpu::CommandEncoder, pts: Tick, lost: &AtomicU64) -> bool {
        let Some(export) = &mut self.export else { return false };
        if let Some(reason) = export.exporter.ring().fallback() {
            self.stop_export(reason);
            return false;
        }
        export.exporter.prepare(device, encoder, self.planes.textures());
        match export.exporter.copy(device, self.planes.textures(), Instant::now()) {
            Ok(Some((commands, ticket))) => {
                export.commands = Some(commands);
                export.ticket = Some((ticket, pts));
                true
            }
            Ok(None) => {
                lost.fetch_add(1, Ordering::Relaxed);
                false
            }
            Err(reason) => {
                self.stop_export(reason);
                false
            }
        }
    }

    /// The export's command buffer for this frame, to submit right after the frame's encoder.
    pub(crate) fn export_commands(&mut self) -> Option<wgpu::CommandBuffer> {
        self.export.as_mut()?.commands.take()
    }

    /// Hand the frame whose copy was submitted to the GPU queues; the ring's ready fence is set
    /// after the copy. `accepted` is whether wgpu took the submission: a submission it rejected
    /// reported an error, and its copy never runs. A rejected submission or another failure stops
    /// the export.
    pub(crate) fn exported(&mut self, accepted: bool) {
        let Some(export) = &mut self.export else { return };
        let Some((ticket, pts)) = export.ticket.take() else { return };
        match handoff(export.commands.take().is_none(), accepted) {
            // The copy was never submitted: its texture is free again once the encoder releases a
            // later frame, since the released fence only grows.
            Handoff::Skip => {}
            Handoff::Stop(reason) => self.stop_export(reason),
            Handoff::Submitted => match export.exporter.submitted(ticket) {
                Ok(frame) => export.sink.push(GpuFrame { frame, pts }),
                Err(reason) => self.stop_export(reason),
            },
        }
    }

    /// Copy the values the GPU has reached to the export ring's ready fence, without waiting.
    pub(crate) fn poll_export(&self) {
        if let Some(export) = &self.export {
            export.exporter.poll();
        }
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

    /// Wait until every mapping started so far has reported back, for at most `timeout`. Call it
    /// after waiting for the GPU: another thread polling the same device can run a mapping's
    /// callback after that wait has returned. Not for the frame loop, as it yields the thread
    /// until the callbacks have run.
    pub(crate) fn wait_mapped(&self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while self.slots.iter().any(|s| s.state.load(Ordering::Acquire) == PENDING) && Instant::now() < deadline {
            std::thread::yield_now();
        }
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

/// What happens to the frame of an export copy once the frame's commands went to the queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Handoff {
    /// The copy was not submitted; the frame is left out.
    Skip,
    /// The frame goes to the encoder.
    Submitted,
    /// wgpu rejected the submission and the copy never runs. Its texture stays taken, so the
    /// export stops and the frames go through memory.
    Stop(Fallback),
}

/// The [`Handoff`] of an export copy whose command buffer was `submitted` in a submission wgpu
/// `accepted`.
fn handoff(submitted: bool, accepted: bool) -> Handoff {
    match (submitted, accepted) {
        (false, _) => Handoff::Skip,
        (true, true) => Handoff::Submitted,
        (true, false) => Handoff::Stop(Fallback::DeviceLost),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A copy whose submission wgpu rejected stops the export, so the ring's textures do not all
    /// end up taken by copies that never ran.
    #[test]
    fn a_rejected_export_submission_stops_the_export() {
        assert_eq!(handoff(true, true), Handoff::Submitted);
        assert_eq!(handoff(true, false), Handoff::Stop(Fallback::DeviceLost));
        assert_eq!(handoff(false, true), Handoff::Skip, "a copy that was never submitted");
        assert_eq!(handoff(false, false), Handoff::Skip);
    }

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
    fn an_export_ring_that_cannot_open_keeps_the_reason_and_the_read_back_runs() {
        // Machines without a GPU pass without checking.
        let Ok(gpu) = crate::Gpu::headless() else { return };
        let converter = crate::compositor::yuv_converter(&gpu.device);
        let mut n = Nv12::new(&gpu.device, &converter, EncodeTarget { width: 64, height: 48, sinks: Vec::new() }).unwrap();
        assert!(n.reads_back(), "without GPU queues the frames are read back");
        assert_eq!(n.set_export(&gpu.device, &gpu.queue, None), Ok(()));
        assert!(!n.exporting());
        match n.set_export(&gpu.device, &gpu.queue, Some(Arc::new(Fifo::new(2)))) {
            Ok(()) => {
                assert!(n.exporting());
                assert!(!n.reads_back(), "with no read-back queues the frames only go to the GPU");
                assert_eq!(n.fallback, None);
            }
            Err(reason) => {
                #[cfg(not(any(windows, target_os = "linux")))]
                assert_eq!(reason, Fallback::Unsupported);
                // On Linux a device that cannot export DMA-BUFs gives its own reason.
                #[cfg(target_os = "linux")]
                assert_ne!(reason, Fallback::Unsupported);
                assert_eq!(n.fallback, Some(reason));
                assert!(!n.exporting());
                assert!(n.reads_back(), "the read-back carries the frames instead");
                assert!(n.export_commands().is_none());
            }
        }
        // No GPU queue stops the export.
        assert_eq!(n.set_export(&gpu.device, &gpu.queue, None), Ok(()));
        assert!(!n.exporting());
        assert!(n.reads_back());
    }

    #[test]
    fn output_sizes_are_made_even() {
        let size = |width, height| even_size(&EncodeTarget { width, height, sinks: Vec::new() });
        assert_eq!(size(1920, 1080), (1920, 1080));
        assert_eq!(size(853, 479), (854, 480));
        assert_eq!(size(0, 1), (2, 2));
    }
}
