//! Program frames handed from the compositor to a hardware encoder on the GPU: the export ring of
//! NV12 textures, the frames that name one of them, and the encoder side's queue of frames.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use crate::fallback::Fallback;
use crate::ring::{AdapterId, SLOTS};

static NEXT_EXPORT: AtomicU64 = AtomicU64::new(1);

/// The NV12 textures and fences one encoder reads the compositor's program frames from. On
/// Windows they live on a Direct3D 11 device of the compositor's adapter, the device a hardware
/// encoder opens its session on, and are shared through NT handles with the compositor's device.
pub struct ExportRing {
    id: u64,
    width: u32,
    height: u32,
    adapter: AdapterId,
    /// 0 while the frames may stay on the GPU, else a [`Fallback`] code.
    fallback: AtomicU8,
    /// A detached ring's fence values, which [`ExportRing::set_ready`] sets.
    detached_ready: AtomicU64,
    detached_released: AtomicU64,
    #[cfg(windows)]
    pub(crate) nv12: Option<crate::dxgi_ffi::Nv12Ring>,
}

impl std::fmt::Debug for ExportRing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExportRing")
            .field("id", &self.id)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("adapter", &self.adapter)
            .field("fallback", &self.fallback())
            .finish()
    }
}

impl ExportRing {
    /// A ring of `width × height` NV12 textures on the adapter `adapter`. Both sides must be even
    /// and not zero ([`Fallback::ImportFailed`] otherwise). Elsewhere than Windows it returns
    /// [`Fallback::Unsupported`].
    pub fn open(adapter: AdapterId, width: u32, height: u32) -> Result<Arc<ExportRing>, Fallback> {
        if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(Fallback::ImportFailed);
        }
        #[cfg(windows)]
        {
            let nv12 = crate::dxgi_ffi::nv12_ring(adapter, width, height)?;
            let mut ring = ExportRing::new(width, height, adapter);
            ring.nv12 = Some(nv12);
            Ok(Arc::new(ring))
        }
        #[cfg(not(windows))]
        {
            let _ = (adapter, width, height);
            Err(Fallback::Unsupported)
        }
    }

    fn new(width: u32, height: u32, adapter: AdapterId) -> ExportRing {
        ExportRing {
            id: NEXT_EXPORT.fetch_add(1, Ordering::Relaxed),
            width,
            height,
            adapter,
            fallback: AtomicU8::new(0),
            detached_ready: AtomicU64::new(0),
            detached_released: AtomicU64::new(0),
            #[cfg(windows)]
            nv12: None,
        }
    }

    /// A ring without textures, which no device opens and no encoder reads: its fences are two
    /// counters, the ready one set with [`ExportRing::set_ready`]. For tests and tools that
    /// exercise the frame handoff without a GPU.
    pub fn detached(width: u32, height: u32) -> Arc<ExportRing> {
        Arc::new(ExportRing::new(width, height, AdapterId(0)))
    }

    /// Unique in the process; a ring opened again is a new ring.
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// The adapter whose device holds the textures.
    pub fn adapter(&self) -> AdapterId {
        self.adapter
    }

    /// Why the frames cannot stay on the GPU, once either side has said so.
    pub fn fallback(&self) -> Option<Fallback> {
        Fallback::from_code(self.fallback.load(Ordering::Acquire))
    }

    /// Record why the frames cannot stay on the GPU. The first reason is kept; the compositor
    /// then sends its frames through CPU memory.
    pub fn refuse(&self, reason: Fallback) {
        let _ = self.fallback.compare_exchange(0, reason.code(), Ordering::AcqRel, Ordering::Acquire);
    }

    /// The ready fence's value now, without waiting: the newest frame whose copy has finished.
    pub fn ready(&self) -> u64 {
        #[cfg(windows)]
        if let Some(nv12) = &self.nv12 {
            return nv12.ready();
        }
        self.detached_ready.load(Ordering::Acquire)
    }

    /// Set a detached ring's ready fence to `value`. A ring with textures is not changed: its
    /// ready fence is set by the compositor's device.
    pub fn set_ready(&self, value: u64) {
        #[cfg(windows)]
        if self.nv12.is_some() {
            return;
        }
        self.detached_ready.fetch_max(value, Ordering::AcqRel);
    }

    /// The released fence's value now, without waiting: the newest frame the encoder has read.
    pub fn released(&self) -> u64 {
        #[cfg(windows)]
        if let Some(nv12) = &self.nv12 {
            return nv12.released();
        }
        self.detached_released.load(Ordering::Acquire)
    }

    /// Hand the textures of the frames up to `value` back to the compositor, after the work the
    /// encoder's device has already queued. Values only grow; a lower one changes nothing. When
    /// the signal cannot be queued, [`Fallback::NoEncoder`] is recorded, since the compositor
    /// would otherwise run out of textures without a reason.
    pub fn release(&self, value: u64) {
        #[cfg(windows)]
        if let Some(nv12) = &self.nv12 {
            if value > nv12.released() && !nv12.release(value) {
                self.refuse(Fallback::NoEncoder);
            }
            return;
        }
        self.detached_released.fetch_max(value, Ordering::AcqRel);
    }

    /// The Direct3D 11 device that holds the textures, which an encoder opens its session on.
    /// `None` for a detached ring.
    #[cfg(windows)]
    pub fn device(&self) -> Option<windows::Win32::Graphics::Direct3D11::ID3D11Device> {
        self.nv12.as_ref().map(|n| n.device().clone())
    }

    /// The NV12 texture of `slot`. `None` for a detached ring or a slot outside the ring.
    #[cfg(windows)]
    pub fn texture(&self, slot: usize) -> Option<windows::Win32::Graphics::Direct3D11::ID3D11Texture2D> {
        self.nv12.as_ref()?.texture(slot).cloned()
    }
}

/// One program frame in a texture of an export ring, and the ready fence value at which the copy
/// into it is done.
#[derive(Clone, Debug)]
pub struct ExportFrame {
    ring: Arc<ExportRing>,
    slot: u8,
    ready: u64,
}

impl ExportFrame {
    /// A frame in texture `slot` of `ring`, done when the ring's ready fence reaches `ready`.
    /// Returns `None` for a slot outside the ring.
    pub fn new(ring: Arc<ExportRing>, slot: usize, ready: u64) -> Option<ExportFrame> {
        let slot = u8::try_from(slot).ok().filter(|&s| usize::from(s) < SLOTS)?;
        Some(ExportFrame { ring, slot, ready })
    }

    pub fn ring(&self) -> &Arc<ExportRing> {
        &self.ring
    }

    pub fn slot(&self) -> usize {
        usize::from(self.slot)
    }

    /// The ready fence's value at which the texture holds this frame, and the value the encoder
    /// sets on the released fence once it has read it.
    pub fn ready(&self) -> u64 {
        self.ready
    }

    /// Whether the copy into the texture has finished.
    pub fn is_ready(&self) -> bool {
        self.ring.ready() >= self.ready
    }

    pub fn width(&self) -> u32 {
        self.ring.width
    }

    pub fn height(&self) -> u32 {
        self.ring.height
    }
}

/// The encoder side's frames, in the order they were made: those whose copy has not finished, and
/// those being encoded until their packets come out. Nothing waits on the GPU: a frame whose copy
/// is late stays in the queue and is taken on a later call, and the caller goes on.
#[derive(Debug)]
pub struct Feed {
    waiting: VecDeque<(ExportFrame, i64)>,
    reading: VecDeque<(ExportFrame, i64)>,
    /// The newest ready value handed back.
    handed: u64,
}

impl Default for Feed {
    fn default() -> Self {
        Feed { waiting: VecDeque::with_capacity(SLOTS + 1), reading: VecDeque::with_capacity(SLOTS + 1), handed: 0 }
    }
}

impl Feed {
    /// Add `frame`, shown at `pts`, after the frames already waiting. A ring has [`SLOTS`]
    /// textures, so a frame beyond that many waiting ones means the oldest texture was written
    /// again: that frame is dropped and returned. Its texture is not handed back here, since that
    /// would also hand back the older frames still being encoded; the next frame handed back
    /// covers it.
    pub fn offer(&mut self, frame: ExportFrame, pts: i64) -> Option<ExportFrame> {
        if self.handed == 0 {
            // A ring another session read before: its frames up to here went back already.
            self.handed = frame.ring.released();
        }
        self.waiting.push_back((frame, pts));
        if self.waiting.len() > SLOTS {
            return self.waiting.pop_front().map(|(dropped, _)| dropped);
        }
        None
    }

    /// The oldest waiting frame and its time, once its copy has finished; it is then counted as
    /// being read. `None` when nothing waits or the oldest frame's copy has not finished.
    pub fn take_ready(&mut self) -> Option<(ExportFrame, i64)> {
        if !self.waiting.front()?.0.is_ready() {
            return None;
        }
        let (frame, pts) = self.waiting.pop_front()?;
        self.reading.push_back((frame.clone(), pts));
        Some((frame, pts))
    }

    /// `packets` more packets have come out. The encoder writes one packet per picture, in the
    /// order it took them, so the textures of that many of the oldest frames being read are
    /// handed back. Times are not used: two frames may share one. Returns how many were handed
    /// back.
    pub fn out(&mut self, packets: usize) -> usize {
        let handed = packets.min(self.reading.len());
        if let Some((frame, _)) = self.reading.drain(..handed).next_back() {
            frame.ring.release(frame.ready);
            self.handed = self.handed.max(frame.ready);
        }
        handed
    }

    /// Forget every frame, waiting or being read, for an encoder that stops, and hand back the
    /// textures whose copies have finished. A texture whose copy is still running is not handed
    /// back, since the compositor would write it again during the copy; a later frame's release
    /// covers it. Returns how many waiting frames were not encoded.
    pub fn drain(&mut self) -> usize {
        let unencoded = self.waiting.len();
        let mut done: Option<&ExportFrame> = None;
        for (frame, _) in self.reading.iter().chain(self.waiting.iter()) {
            if frame.is_ready() && done.is_none_or(|d| frame.ready > d.ready) {
                done = Some(frame);
            }
        }
        if let Some(frame) = done {
            frame.ring.release(frame.ready);
            self.handed = self.handed.max(frame.ready);
        }
        self.reading.clear();
        self.waiting.clear();
        unencoded
    }

    /// Whether every texture of the ring holds a frame the encoder has not handed back, while it
    /// has nothing left to encode: the frames it is reading, and frames the queue between the
    /// compositor and the encoder dropped (their textures go back only with a later frame). The
    /// compositor then has no texture to export into, so an encoder that buffers more pictures
    /// than the ring holds would never get the next one.
    pub fn stalled(&self) -> bool {
        self.waiting.is_empty() && self.reading.back().is_some_and(|(frame, _)| frame.ready.saturating_sub(self.handed) >= SLOTS as u64)
    }

    /// Whether the oldest waiting frame's copy is still running on the compositor's queue.
    pub fn copy_running(&self) -> bool {
        self.waiting.front().is_some_and(|(frame, _)| !frame.is_ready())
    }

    /// Frames whose copy has not finished or that were not taken yet.
    pub fn waiting(&self) -> usize {
        self.waiting.len()
    }

    /// Frames taken whose packets have not come out.
    pub fn reading(&self) -> usize {
        self.reading.len()
    }
}

/// The compositor side of an export ring: it copies each program frame's two planes into a free
/// NV12 texture of the ring on the compositor's queue and sets the ready fence after the copy.
/// Nothing waits: a frame for which every texture is still being encoded is not exported. One
/// encoder session reads a ring: the released fence says which frames that session has read.
#[cfg(feature = "wgpu")]
pub struct Exporter {
    ring: Arc<ExportRing>,
    slots: crate::ring::SlotTable,
    /// One-texel textures of the planes' formats that [`Exporter::prepare`] copies into.
    scratch: Option<Vec<wgpu::Texture>>,
    #[cfg(windows)]
    raw: Raw,
}

#[cfg(all(feature = "wgpu", windows))]
enum Raw {
    Dx12(crate::d3d12_ffi::Exported),
    /// Boxed: it holds the device's function table.
    Vulkan(Box<crate::vulkan_ffi::Exported>),
}

/// One-texel textures of the formats of `planes`, one each, for [`Exporter::prepare`] to copy
/// into: a copy needs both sides in one format. `kept` is returned when its formats still match.
#[cfg(feature = "wgpu")]
pub(crate) fn scratch_for(device: &wgpu::Device, planes: &[wgpu::Texture], kept: Option<Vec<wgpu::Texture>>) -> Vec<wgpu::Texture> {
    if let Some(kept) = kept
        && kept.len() == planes.len()
        && kept.iter().zip(planes).all(|(s, p)| s.format() == p.format())
    {
        return kept;
    }
    planes
        .iter()
        .map(|plane| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some("program to encoder scratch"),
                size: wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: plane.format(),
                usage: wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            })
        })
        .collect()
}

/// A copy recorded by [`Exporter::copy`], to hand to [`Exporter::submitted`] once its command
/// buffer has been submitted.
#[cfg(feature = "wgpu")]
#[derive(Debug)]
#[must_use = "the frame is only handed to the encoder by `Exporter::submitted`"]
pub struct Ticket {
    slot: usize,
    ready: u64,
}

/// The graphics API and adapter of `device`, as [`check_export`](crate::check_export) takes them.
#[cfg(feature = "wgpu")]
pub fn device_backend(device: &wgpu::Device) -> (crate::Backend, Option<AdapterId>) {
    #[cfg(windows)]
    {
        if let Some(adapter) = crate::d3d12_ffi::adapter(device) {
            return (crate::Backend::Dx12, Some(adapter));
        }
        if let Some((adapter, timeline)) = crate::vulkan_ffi::adapter(device) {
            let external_memory = device.features().contains(wgpu::Features::VULKAN_EXTERNAL_MEMORY_WIN32);
            return (crate::Backend::Vulkan { external_memory, timeline }, adapter);
        }
    }
    let _ = device;
    (crate::Backend::Other, None)
}

#[cfg(feature = "wgpu")]
impl Exporter {
    /// An export ring of `width × height` on the adapter of `device`, opened on that device. The
    /// device must run on Direct3D 12, or on Vulkan with `VK_KHR_external_memory_win32` and
    /// timeline semaphores; the reason is returned otherwise. Elsewhere than Windows it returns
    /// [`Fallback::Unsupported`].
    pub fn open(device: &wgpu::Device, queue: &wgpu::Queue, width: u32, height: u32) -> Result<Exporter, Fallback> {
        let (backend, compositor) = device_backend(device);
        #[cfg(windows)]
        {
            let adapter = crate::fallback::check_export(backend, compositor)?;
            let ring = ExportRing::open(adapter, width, height)?;
            let raw = match backend {
                crate::Backend::Dx12 => Raw::Dx12(crate::d3d12_ffi::export(device, queue, &ring)?),
                _ => Raw::Vulkan(Box::new(crate::vulkan_ffi::export(device, queue, &ring)?)),
            };
            Ok(Exporter { ring, slots: crate::ring::SlotTable::default(), scratch: None, raw })
        }
        #[cfg(not(windows))]
        {
            let _ = (backend, compositor, queue);
            ExportRing::open(AdapterId(0), width, height).map(|ring| Exporter { ring, slots: crate::ring::SlotTable::default(), scratch: None })
        }
    }

    /// The ring an encoder session reads from.
    pub fn ring(&self) -> &Arc<ExportRing> {
        &self.ring
    }

    /// Make the one-texel textures that [`Exporter::prepare`] copies `planes` into, outside the
    /// frame loop.
    pub fn reserve(&mut self, device: &wgpu::Device, planes: &[wgpu::Texture]) {
        let kept = self.scratch.take();
        self.scratch = Some(scratch_for(device, planes, kept));
    }

    /// Record, as the last use of `planes` in `encoder`, a one-texel copy from each of them, which
    /// leaves them in the copy-source state that [`Exporter::copy`] needs. The planes need
    /// `COPY_SRC` usage, and `encoder`'s command buffer is submitted before the copy's.
    pub fn prepare(&mut self, device: &wgpu::Device, encoder: &mut wgpu::CommandEncoder, planes: &[wgpu::Texture]) {
        let kept = self.scratch.take();
        // Made by `Exporter::reserve` when the planes keep their formats, so nothing is made here.
        let scratch = self.scratch.insert(scratch_for(device, planes, kept));
        for (plane, scratch) in planes.iter().zip(scratch.iter()) {
            encoder.copy_texture_to_texture(
                plane.as_image_copy(),
                scratch.as_image_copy(),
                wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
            );
        }
    }

    /// Record the copy of `planes`, a frame's luma (R8, the ring's size) and chroma (RG8, half of
    /// it) textures, into a free texture of the ring. The planes must be in the copy-source state:
    /// [`Exporter::prepare`] was the caller's last use of them, in a command buffer submitted
    /// before the one returned. Returns the command buffer to submit and its ticket, or
    /// `None` when every texture is still being encoded (the frame is not exported). Returns the
    /// reason, also recorded on the ring, when the frames cannot stay on the GPU.
    pub fn copy(
        &mut self,
        device: &wgpu::Device,
        planes: &[wgpu::Texture],
        now: std::time::Instant,
    ) -> Result<Option<(wgpu::CommandBuffer, Ticket)>, Fallback> {
        if let Some(reason) = self.ring.fallback() {
            return Err(reason);
        }
        self.poll();
        // An encoder may hold a picture for longer than `RECLAIM`, so a texture is written again
        // only once the encoder has released it.
        let Some(slot) = self.slots.pick_released(self.ring.released()) else { return Ok(None) };
        #[cfg(windows)]
        let commands = match &self.raw {
            Raw::Dx12(r) => r.copy(device, planes, slot),
            Raw::Vulkan(r) => r.copy(planes, slot),
        };
        #[cfg(not(windows))]
        let commands: Result<wgpu::CommandBuffer, Fallback> = {
            let _ = (device, planes);
            Err(Fallback::Unsupported)
        };
        let commands = commands.inspect_err(|&reason| self.ring.refuse(reason))?;
        let Some(ready) = self.slots.fill(slot, now) else {
            self.ring.refuse(Fallback::ImportFailed);
            return Err(Fallback::ImportFailed);
        };
        Ok(Some((commands, Ticket { slot, ready })))
    }

    /// The frame of `ticket`, once its command buffer has been submitted: the ready fence is set
    /// after the copy, and the frame is what an encoder session takes.
    pub fn submitted(&mut self, ticket: Ticket) -> Result<ExportFrame, Fallback> {
        #[cfg(windows)]
        let signaled = match &self.raw {
            Raw::Dx12(r) => r.signal(ticket.ready),
            Raw::Vulkan(r) => {
                r.signal(ticket.ready);
                Ok(())
            }
        };
        #[cfg(not(windows))]
        let signaled: Result<(), Fallback> = Err(Fallback::Unsupported);
        signaled.inspect_err(|&reason| self.ring.refuse(reason))?;
        ExportFrame::new(Arc::clone(&self.ring), ticket.slot, ticket.ready).ok_or(Fallback::ImportFailed)
    }

    /// Copy the values the compositor's queue has reached to the ready fence, without waiting.
    /// Only a Vulkan export has anything to do; [`Exporter::copy`] also calls it.
    pub fn poll(&self) {
        #[cfg(windows)]
        if let Raw::Vulkan(r) = &self.raw {
            r.poll();
        }
    }
}
