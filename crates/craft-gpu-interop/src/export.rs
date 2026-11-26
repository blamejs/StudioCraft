//! Program frames handed from the compositor to a hardware encoder on the GPU: the export ring of
//! NV12 textures, the frames that name one of them, and the encoder side's queue of frames.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

use crate::fallback::Fallback;
use crate::ring::{AdapterId, SLOTS};

static NEXT_EXPORT: AtomicU64 = AtomicU64::new(1);

/// The completed value `GetCompletedValue` returns for a fence whose device has been removed.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) const REMOVED: u64 = u64::MAX;

/// Raise a fence through `raise` to `reached`, the value a timeline semaphore was read at once the
/// queue had finished the work submitted before it (`None` when it could not be read), and never
/// to `target`, the newest value that work signals: a lost device also finishes the work, without
/// signaling it. Returns whether the semaphore reached `target`.
#[cfg_attr(not(all(windows, feature = "wgpu")), allow(dead_code))]
pub(crate) fn settle(reached: Option<u64>, target: u64, raise: impl FnOnce(u64)) -> bool {
    if let Some(done) = reached {
        raise(done);
    }
    reached.is_some_and(|done| done >= target)
}

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
    /// The highest value a detached ring was asked to release.
    detached_queued: AtomicU64,
    /// Set in tests while a detached ring's releases stay queued instead of completing.
    #[cfg(test)]
    pub(crate) hold_releases: AtomicBool,
    /// The newest frame of an encoder that stopped while its copy was still running, whose texture
    /// goes back once that copy has finished; 0 when there is none.
    abandoned: AtomicU64,
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
            detached_queued: AtomicU64::new(0),
            #[cfg(test)]
            hold_releases: AtomicBool::new(false),
            abandoned: AtomicU64::new(0),
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
    /// Once the device that holds the textures has been removed it is 0, and
    /// [`Fallback::DeviceRemoved`] is recorded.
    pub fn ready(&self) -> u64 {
        #[cfg(windows)]
        if let Some(nv12) = &self.nv12 {
            return self.completed(nv12.ready());
        }
        self.detached_ready.load(Ordering::Acquire)
    }

    /// `value`, a fence's completed value as Direct3D reports it. A removed device reports
    /// [`REMOVED`]: then [`Fallback::DeviceRemoved`] is recorded and 0 returned, so no frame
    /// counts as ready and no texture as handed back.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn completed(&self, value: u64) -> u64 {
        if value == REMOVED {
            self.refuse(Fallback::DeviceRemoved);
            return 0;
        }
        value
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
    /// Once the device that holds the textures has been removed it is 0, and
    /// [`Fallback::DeviceRemoved`] is recorded.
    pub fn released(&self) -> u64 {
        #[cfg(windows)]
        if let Some(nv12) = &self.nv12 {
            return self.completed(nv12.released());
        }
        self.detached_released.load(Ordering::Acquire)
    }

    /// Hand the textures of the frames up to `value` back to the compositor, after the work the
    /// encoder's device has already queued. Values only grow; a value not above the highest one
    /// queued so far changes nothing. When the signal cannot be queued, [`Fallback::NoEncoder`]
    /// is recorded, since the compositor would otherwise run out of textures without a reason.
    pub fn release(&self, value: u64) {
        #[cfg(windows)]
        if let Some(nv12) = &self.nv12 {
            if !nv12.release(value) {
                self.refuse(Fallback::NoEncoder);
            }
            return;
        }
        self.detached_queued.fetch_max(value, Ordering::AcqRel);
        #[cfg(test)]
        if self.hold_releases.load(Ordering::Acquire) {
            return;
        }
        self.detached_released.fetch_max(value, Ordering::AcqRel);
    }

    /// A detached ring's queued releases, completed now (tests).
    #[cfg(test)]
    pub(crate) fn finish_releases(&self) {
        self.detached_released.fetch_max(self.detached_queued.load(Ordering::Acquire), Ordering::AcqRel);
    }

    /// The newest frame whose texture has gone back to the compositor, or is about to: the released
    /// fence's value, a release queued but not yet run, and the newest frame of an encoder that
    /// stopped during its copy. An encoder session must not take a frame at or below it, since the
    /// compositor may write its texture again.
    pub fn handed_back(&self) -> u64 {
        #[cfg(windows)]
        let queued = match &self.nv12 {
            Some(nv12) => nv12.queued(),
            None => self.detached_queued.load(Ordering::Acquire),
        };
        #[cfg(not(windows))]
        let queued = self.detached_queued.load(Ordering::Acquire);
        self.released().max(queued).max(self.abandoned.load(Ordering::Acquire))
    }

    /// Record that an encoder stopped while the copy of the frame with value `value` was still
    /// running, so its texture goes back once the copy has finished ([`ExportRing::reclaim`]).
    pub(crate) fn abandon(&self, value: u64) {
        self.abandoned.fetch_max(value, Ordering::AcqRel);
    }

    /// Hand back the textures of a stopped encoder's frames whose copies have finished since it
    /// stopped. Without it a ring whose every texture held such a frame would stay full. The
    /// compositor calls it before it looks for a free texture.
    pub fn reclaim(&self) {
        let abandoned = self.abandoned.load(Ordering::Acquire);
        if abandoned == 0 || self.ready() < abandoned {
            return;
        }
        self.release(abandoned);
        let _ = self.abandoned.compare_exchange(abandoned, 0, Ordering::AcqRel, Ordering::Acquire);
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

/// The released fence's values, handed to a signal that only one thread may run at a time without
/// a lock. A caller that finds another one signaling leaves its value for that caller to signal and
/// returns at once.
#[derive(Debug, Default)]
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) struct ReleaseGate {
    /// The highest value asked for.
    wanted: AtomicU64,
    /// The highest value signaled. A signal still queued is not in the fence's completed value yet.
    queued: AtomicU64,
    /// Set while one caller runs `signal`.
    busy: AtomicBool,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl ReleaseGate {
    /// Ask for the fence to reach `value`. A value not above the highest one signaled so far is not
    /// signaled, so the fence never goes back. The caller that holds the gate runs `signal` with the
    /// highest value asked for, again while a higher one arrives, and returns whether every signal
    /// it ran was queued; a caller that finds the gate held returns `true` without waiting.
    pub(crate) fn release(&self, value: u64, mut signal: impl FnMut(u64) -> bool) -> bool {
        // `wanted` and `busy` are written and read in one total order (SeqCst): a caller that finds
        // the gate held asked for its value before the holder opens the gate, so the holder's check
        // after opening it sees that value.
        self.wanted.fetch_max(value, Ordering::SeqCst);
        loop {
            if self.busy.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
                return true;
            }
            let mut queued = true;
            loop {
                let wanted = self.wanted.load(Ordering::SeqCst);
                if wanted <= self.queued.load(Ordering::Relaxed) {
                    break;
                }
                if !signal(wanted) {
                    queued = false;
                    break;
                }
                self.queued.store(wanted, Ordering::Relaxed);
            }
            self.busy.store(false, Ordering::SeqCst);
            // A value asked for after the last check and before the gate opened is signaled here.
            if !queued || self.wanted.load(Ordering::SeqCst) <= self.queued.load(Ordering::Relaxed) {
                return queued;
            }
        }
    }

    /// The highest value asked for so far, from any thread, signaled or about to be.
    pub(crate) fn asked(&self) -> u64 {
        self.wanted.load(Ordering::SeqCst)
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
    /// The newest ready value offered.
    offered: u64,
}

impl Default for Feed {
    fn default() -> Self {
        Feed { waiting: VecDeque::with_capacity(SLOTS + 1), reading: VecDeque::with_capacity(SLOTS + 1), handed: 0, offered: 0 }
    }
}

impl Feed {
    /// Whether [`Feed::offer`] queues `frame`. The compositor gives each frame a ready value
    /// above every earlier one, so a frame whose value is not above the newest frame offered or
    /// handed back is a copy of a frame already given: its texture may hold a newer picture, and
    /// it is refused. A session on a ring read before also counts the releases that session
    /// queued and has not finished ([`ExportRing::handed_back`]).
    pub fn accepts(&self, frame: &ExportFrame) -> bool {
        let handed = if self.handed == 0 { frame.ring.handed_back() } else { self.handed };
        frame.ready > self.offered.max(handed)
    }

    /// Add `frame`, shown at `pts`, after the frames already waiting. A frame that
    /// [`Feed::accepts`] refuses is returned without being queued. A ring has [`SLOTS`]
    /// textures, so a frame beyond that many waiting ones means the oldest texture was written
    /// again: that frame is dropped and returned. Its texture is not handed back here, since that
    /// would also hand back the older frames still being encoded; the next frame handed back
    /// covers it.
    pub fn offer(&mut self, frame: ExportFrame, pts: i64) -> Option<ExportFrame> {
        if !self.accepts(&frame) {
            return Some(frame);
        }
        if self.handed == 0 {
            // A ring another session read before: its frames up to here went back already.
            self.handed = frame.ring.handed_back();
        }
        self.offered = frame.ready;
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
    /// back, since the compositor would write it again during the copy: the ring keeps the newest
    /// such frame, and the compositor hands its texture back once the copy has finished
    /// ([`ExportRing::reclaim`]). Returns how many waiting frames were not encoded.
    pub fn drain(&mut self) -> usize {
        let unencoded = self.waiting.len();
        let mut done: Option<&ExportFrame> = None;
        let mut running: Option<&ExportFrame> = None;
        for (frame, _) in self.reading.iter().chain(self.waiting.iter()) {
            let newest = if frame.is_ready() { &mut done } else { &mut running };
            if newest.is_none_or(|d| frame.ready > d.ready) {
                *newest = Some(frame);
            }
        }
        if let Some(frame) = done {
            frame.ring.release(frame.ready);
            self.handed = self.handed.max(frame.ready);
        }
        if let Some(frame) = running.filter(|r| done.is_none_or(|d| r.ready > d.ready)) {
            frame.ring.abandon(frame.ready);
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
    /// The compositor's queue, which runs the copies.
    queue: wgpu::Queue,
    /// The planes of the last copy, shared with every [`Ticket`] of them. The copy reads them
    /// outside wgpu's tracking, so they are kept until the queue has finished the last copy
    /// recorded from them, and only planes that change make a new entry.
    planes: Option<Arc<UntilDone<Vec<wgpu::Texture>>>>,
    /// Shared with every [`Ticket`], so the ring's textures stay open on the compositor's device
    /// until the last copy recorded into them has been submitted.
    #[cfg(windows)]
    raw: Arc<Raw>,
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

/// `value`, dropped only once the queue has finished the work submitted to it before this is
/// dropped, without waiting.
#[cfg(feature = "wgpu")]
pub(crate) struct UntilDone<T: Send + 'static> {
    queue: wgpu::Queue,
    /// `None` only while being dropped.
    value: Option<T>,
}

#[cfg(feature = "wgpu")]
impl<T: Send + 'static> UntilDone<T> {
    pub(crate) fn new(queue: &wgpu::Queue, value: T) -> UntilDone<T> {
        UntilDone { queue: queue.clone(), value: Some(value) }
    }

    pub(crate) fn get(&self) -> Option<&T> {
        self.value.as_ref()
    }
}

#[cfg(feature = "wgpu")]
impl<T: Send + 'static> Drop for UntilDone<T> {
    fn drop(&mut self) {
        let value = self.value.take();
        self.queue.on_submitted_work_done(move || drop(value));
    }
}

/// A copy recorded by [`Exporter::copy`], to hand to [`Exporter::submitted`] once its command
/// buffer has been submitted. It keeps the ring's textures open on the compositor's device, even
/// after the [`Exporter`] is dropped, and the frame's plane textures alive until the queue has
/// finished the copy, so drop it only after the command buffer has been submitted or dropped.
#[cfg(feature = "wgpu")]
#[must_use = "the frame is only handed to the encoder by `Exporter::submitted`"]
pub struct Ticket {
    slot: usize,
    ready: u64,
    #[cfg(windows)]
    _textures: Arc<Raw>,
    _planes: Arc<UntilDone<Vec<wgpu::Texture>>>,
}

/// The entry in `kept` that keeps `planes` alive for a copy from them. The entry of the same
/// planes is shared; other planes replace it with a new one, and the old entry is handed to the
/// queue once no ticket holds it.
#[cfg(feature = "wgpu")]
pub(crate) fn kept_planes(
    kept: &mut Option<Arc<UntilDone<Vec<wgpu::Texture>>>>,
    queue: &wgpu::Queue,
    planes: &[wgpu::Texture],
) -> Arc<UntilDone<Vec<wgpu::Texture>>> {
    if let Some(entry) = kept
        && entry.get().is_some_and(|kept| kept.as_slice() == planes)
    {
        return Arc::clone(entry);
    }
    Arc::clone(kept.insert(Arc::new(UntilDone::new(queue, planes.to_vec()))))
}

#[cfg(feature = "wgpu")]
impl std::fmt::Debug for Ticket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ticket").field("slot", &self.slot).field("ready", &self.ready).finish()
    }
}

/// Whether `luma` and `chroma`, the sizes of a frame's planes, are a `width × height` luma plane
/// and a chroma plane of half that, the sizes the copy into an export ring's textures needs.
#[cfg_attr(not(feature = "wgpu"), allow(dead_code))]
pub(crate) fn planes_fit(width: u32, height: u32, luma: (u32, u32), chroma: (u32, u32)) -> bool {
    luma == (width, height) && chroma == (width / 2, height / 2)
}

/// Whether `luma` and `chroma`, the formats of a frame's planes, can be copied into an NV12
/// texture's planes: an 8-bit one-channel luma plane and an 8-bit two-channel chroma plane.
#[cfg(feature = "wgpu")]
pub(crate) fn plane_formats_fit(luma: wgpu::TextureFormat, chroma: wgpu::TextureFormat) -> bool {
    use wgpu::TextureFormat as F;
    matches!(luma, F::R8Unorm | F::R8Snorm | F::R8Uint | F::R8Sint) && matches!(chroma, F::Rg8Unorm | F::Rg8Snorm | F::Rg8Uint | F::Rg8Sint)
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
            Ok(Exporter { ring, slots: crate::ring::SlotTable::default(), scratch: None, queue: queue.clone(), planes: None, raw: Arc::new(raw) })
        }
        #[cfg(not(windows))]
        {
            let _ = (backend, compositor);
            ExportRing::open(AdapterId(0), width, height).map(|ring| Exporter {
                ring,
                slots: crate::ring::SlotTable::default(),
                scratch: None,
                queue: queue.clone(),
                planes: None,
            })
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
    /// reason, also recorded on the ring, when the frames cannot stay on the GPU. Planes of other
    /// sizes than the ring's luma and chroma planes, or of other formats than 8-bit one-channel
    /// luma and 8-bit two-channel chroma, are refused with [`Fallback::ImportFailed`]. The ticket
    /// keeps the planes alive until the queue has finished the copy.
    pub fn copy(
        &mut self,
        device: &wgpu::Device,
        planes: &[wgpu::Texture],
        now: std::time::Instant,
    ) -> Result<Option<(wgpu::CommandBuffer, Ticket)>, Fallback> {
        self.poll();
        self.ring.reclaim();
        // Reading the released fence records the fallback of a removed device.
        let released = self.ring.released();
        if let Some(reason) = self.ring.fallback() {
            return Err(reason);
        }
        let size = |t: &wgpu::Texture| (t.width(), t.height());
        if !matches!(planes, [luma, chroma] if planes_fit(self.ring.width(), self.ring.height(), size(luma), size(chroma))
            && plane_formats_fit(luma.format(), chroma.format()))
        {
            self.ring.refuse(Fallback::ImportFailed);
            return Err(Fallback::ImportFailed);
        }
        // An encoder may hold a picture for longer than `RECLAIM`, so a texture is written again
        // only once the encoder has released it.
        let Some(slot) = self.slots.pick_released(released) else { return Ok(None) };
        #[cfg(windows)]
        let commands = match &*self.raw {
            Raw::Dx12(r) => r.copy(device, planes, slot),
            Raw::Vulkan(r) => r.copy(planes, slot),
        };
        #[cfg(not(windows))]
        let commands: Result<wgpu::CommandBuffer, Fallback> = {
            let _ = (device, planes);
            Err(Fallback::Unsupported)
        };
        let commands = commands.inspect_err(|&reason| self.ring.refuse(reason))?;
        let Some(ready) = self.slots.fill_released(slot, now) else {
            self.ring.refuse(Fallback::ImportFailed);
            return Err(Fallback::ImportFailed);
        };
        let ticket = Ticket {
            slot,
            ready,
            #[cfg(windows)]
            _textures: Arc::clone(&self.raw),
            _planes: kept_planes(&mut self.planes, &self.queue, planes),
        };
        Ok(Some((commands, ticket)))
    }

    /// The frame of `ticket`, once its command buffer has been submitted: the ready fence is set
    /// after the copy, and the frame is what an encoder session takes.
    pub fn submitted(&mut self, ticket: Ticket) -> Result<ExportFrame, Fallback> {
        #[cfg(windows)]
        let signaled = match &*self.raw {
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
    /// Only a Vulkan export has anything to do; [`Exporter::copy`] also calls it. When the ready
    /// fence cannot be set, [`Fallback::ImportFailed`] is recorded on the ring, as a Direct3D 12
    /// export records it when its signal fails.
    pub fn poll(&self) {
        #[cfg(windows)]
        if let Raw::Vulkan(r) = &*self.raw
            && !r.poll()
        {
            self.ring.refuse(Fallback::ImportFailed);
        }
    }
}
