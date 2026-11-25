//! Program frames handed from the compositor to a hardware encoder on the GPU: the export ring of
//! NV12 textures, the frames that name one of them, and the encoder side's queue of frames.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};

use crate::fallback::Fallback;
use crate::ring::{AdapterId, SLOTS};

static NEXT_EXPORT: AtomicU64 = AtomicU64::new(1);

/// The compositor's count of finished copies, which a ring without a shared ready fence reads
/// without waiting: on Linux, a timeline semaphore of the compositor's device.
pub(crate) trait ReadyCounter: Send + Sync {
    /// The newest frame value whose copy has finished, or `None` when it cannot be read now.
    fn reached(&self) -> Option<u64>;
}

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
/// On Linux they are NV12 images of the compositor's Vulkan device, shared as DMA-BUFs
/// ([`ExportRing::buffer`]).
pub struct ExportRing {
    id: u64,
    width: u32,
    height: u32,
    adapter: AdapterId,
    /// 0 while the frames may stay on the GPU, else a [`Fallback`] code.
    fallback: AtomicU8,
    /// A detached ring's fence values, which [`ExportRing::set_ready`] sets. A Linux ring keeps
    /// the newest value its counter has reached here, and the released value set by the encoder.
    detached_ready: AtomicU64,
    detached_released: AtomicU64,
    /// The counter [`ExportRing::ready`] reads, while the compositor side that owns it exists.
    counter: OnceLock<Weak<dyn ReadyCounter>>,
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
    #[cfg(target_os = "linux")]
    pub(crate) dmabuf: Option<DmabufBuffers>,
}

/// The DMA-BUF buffers of a Linux export ring, one per slot, and the DRM render node of the device
/// that made them.
#[cfg(target_os = "linux")]
pub(crate) struct DmabufBuffers {
    pub(crate) buffers: Vec<ExportBuffer>,
    pub(crate) render_node: Option<u64>,
}

/// One NV12 picture of a Linux export ring, shared as a DMA-BUF: a single memory object of `size`
/// bytes in the memory layout `modifier`, its luma plane at `offsets[0]` with rows `pitches[0]`
/// bytes apart, and its interleaved chroma plane at `offsets[1]` with rows `pitches[1]` apart.
/// The image is [`ExportRing::padded_size`], the ring's size rounded up to whole 16 x 16 blocks,
/// so an encoder that codes such blocks reads every row and column of them from the buffer. The
/// picture is at its top left, and each copy fills the padding with the picture's last column and
/// row.
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub struct ExportBuffer {
    fd: std::os::fd::OwnedFd,
    size: u64,
    modifier: u64,
    offsets: [u32; 2],
    pitches: [u32; 2],
    plane_sizes: [u64; 2],
    image: (u32, u32),
}

#[cfg(target_os = "linux")]
impl ExportBuffer {
    /// A buffer of the DMA-BUF `fd` holding a `width` x `height` NV12 image, or `None` when its two
    /// planes, `plane_sizes` bytes long as the driver reports them, do not hold such an image
    /// inside `size` bytes ([`nv12_planes_fit`]).
    #[cfg_attr(not(feature = "wgpu"), allow(dead_code))]
    pub(crate) fn new(
        fd: std::os::fd::OwnedFd,
        size: u64,
        modifier: u64,
        offsets: [u32; 2],
        pitches: [u32; 2],
        plane_sizes: [u64; 2],
        (width, height): (u32, u32),
    ) -> Option<ExportBuffer> {
        nv12_planes_fit(width, height, size, offsets, pitches, plane_sizes, modifier == crate::dmabuf::MOD_LINEAR).then_some(ExportBuffer {
            fd,
            size,
            modifier,
            offsets,
            pitches,
            plane_sizes,
            image: (width, height),
        })
    }

    /// The width and height of the NV12 image in the buffer: [`ExportRing::padded_size`] of the
    /// ring's size.
    pub fn image_size(&self) -> (u32, u32) {
        self.image
    }

    /// The DMA-BUF's descriptor, which stays the ring's. An encoder that imports the buffer takes
    /// its own reference through it.
    pub fn fd(&self) -> std::os::fd::BorrowedFd<'_> {
        use std::os::fd::AsFd;
        self.fd.as_fd()
    }

    /// The size of the memory object in bytes.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// The DRM format modifier of the picture's memory layout.
    pub fn modifier(&self) -> u64 {
        self.modifier
    }

    /// Where the luma and chroma planes start, in bytes.
    pub fn offsets(&self) -> [u32; 2] {
        self.offsets
    }

    /// The distance between two rows of the luma and chroma planes, in bytes.
    pub fn pitches(&self) -> [u32; 2] {
        self.pitches
    }

    /// The bytes of the luma and chroma planes, as the driver reports them.
    pub fn plane_sizes(&self) -> [u64; 2] {
        self.plane_sizes
    }
}

/// The Linux device number (`dev_t`, as `st_rdev` reports it) of the device `major`:`minor`, in
/// glibc's and musl's encoding: the low 8 bits of the minor number, the low 12 bits of the major
/// number, then the rest of the minor number and the rest of the major number.
#[cfg_attr(not(all(target_os = "linux", feature = "wgpu")), allow(dead_code))]
pub(crate) fn dev_number(major: u32, minor: u32) -> u64 {
    let (major, minor) = (u64::from(major), u64::from(minor));
    ((major & 0xffff_f000) << 32) | ((major & 0x0fff) << 8) | ((minor & 0xffff_ff00) << 12) | (minor & 0xff)
}

/// Whether a `width` x `height` NV12 picture fits `size` bytes with its luma plane at `offsets[0]`
/// and its chroma plane (half as many rows of the same width) at `offsets[1]`, rows `pitches`
/// bytes apart and `plane_sizes` bytes long as the driver reports them, without the sizes
/// overflowing. Each plane's reported bytes must lie inside `size`. In a linear layout (`linear`)
/// each luma row must also hold the picture's width, each chroma row its whole interleaved pairs
/// (two bytes for every two pixels, an odd last pixel included), and every row must lie inside
/// `size` and inside its plane's reported bytes; in a tiled one the driver lays the rows out, and
/// its plane sizes are the planes' extent.
pub fn nv12_planes_fit(width: u32, height: u32, size: u64, offsets: [u32; 2], pitches: [u32; 2], plane_sizes: [u64; 2], linear: bool) -> bool {
    let chroma_row = 2 * u64::from(width.div_ceil(2));
    if width == 0
        || height == 0
        || pitches.contains(&0)
        || plane_sizes.contains(&0)
        || (linear && (pitches[0] < width || u64::from(pitches[1]) < chroma_row))
    {
        return false;
    }
    let reported = |plane: usize| u64::from(offsets[plane]).checked_add(plane_sizes[plane]).is_some_and(|end| end <= size);
    if !reported(0) || !reported(1) {
        return false;
    }
    if !linear {
        return true;
    }
    let end = |offset: u32, pitch: u32, rows: u32| u64::from(pitch).checked_mul(u64::from(rows)).and_then(|n| n.checked_add(u64::from(offset)));
    let (Some(luma), Some(chroma)) = (end(offsets[0], pitches[0], height), end(offsets[1], pitches[1], height.div_ceil(2))) else { return false };
    let rows = [(height, u64::from(width)), (height.div_ceil(2), chroma_row)];
    luma <= size && chroma <= size && (0..2).all(|plane| linear_rows_fit(pitches[plane], rows[plane].0, rows[plane].1, plane_sizes[plane]))
}

/// Whether `rows` linear rows `pitch` bytes apart, the last one `row_bytes` long, lie inside a
/// plane of `plane_size` bytes.
fn linear_rows_fit(pitch: u32, rows: u32, row_bytes: u64, plane_size: u64) -> bool {
    u64::from(pitch).checked_mul(u64::from(rows.saturating_sub(1))).and_then(|n| n.checked_add(row_bytes)).is_some_and(|end| end <= plane_size)
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
            counter: OnceLock::new(),
            detached_queued: AtomicU64::new(0),
            #[cfg(test)]
            hold_releases: AtomicBool::new(false),
            abandoned: AtomicU64::new(0),
            #[cfg(windows)]
            nv12: None,
            #[cfg(target_os = "linux")]
            dmabuf: None,
        }
    }

    /// A Linux ring of the DMA-BUF `buffers`, one per slot, made on `adapter`, whose device has the
    /// DRM render node `render_node`.
    #[cfg(target_os = "linux")]
    #[cfg_attr(not(feature = "wgpu"), allow(dead_code))]
    pub(crate) fn of_dmabufs(adapter: AdapterId, width: u32, height: u32, buffers: Vec<ExportBuffer>, render_node: Option<u64>) -> ExportRing {
        let mut ring = ExportRing::new(width, height, adapter);
        ring.dmabuf = Some(DmabufBuffers { buffers, render_node });
        ring
    }

    /// Read the ready value from `counter` while it exists. A ring takes one counter; a later one
    /// is ignored.
    #[cfg_attr(not(all(target_os = "linux", feature = "wgpu")), allow(dead_code))]
    pub(crate) fn count_ready_with(&self, counter: Weak<dyn ReadyCounter>) {
        let _ = self.counter.set(counter);
    }

    /// The DMA-BUF of `slot` on Linux. `None` for a ring of another kind, a detached ring, or a
    /// slot outside the ring.
    #[cfg(target_os = "linux")]
    pub fn buffer(&self, slot: usize) -> Option<&ExportBuffer> {
        self.dmabuf.as_ref()?.buffers.get(slot)
    }

    /// On Linux, the device number (`st_rdev`) of the DRM render node of the device that made the
    /// ring's DMA-BUFs, for an encoder to open the same GPU. `None` when the driver does not report
    /// it, or for a ring without DMA-BUFs.
    #[cfg(target_os = "linux")]
    pub fn render_node(&self) -> Option<u64> {
        self.dmabuf.as_ref()?.render_node
    }

    /// A ring without textures, which no device opens and no encoder reads: its fences are two
    /// counters, the ready one set with [`ExportRing::set_ready`]. For tests and tools that
    /// exercise the frame handoff without a GPU.
    pub fn detached(width: u32, height: u32) -> Arc<ExportRing> {
        Arc::new(ExportRing::new(width, height, AdapterId(0)))
    }

    /// Unique in the process; a ring opened again is a new ring.
    /// The size of a Linux export ring's images: the ring's size rounded up to multiples of 16,
    /// or `None` when that does not fit in a `u32`.
    pub fn padded_size(width: u32, height: u32) -> Option<(u32, u32)> {
        Some((width.checked_next_multiple_of(16)?, height.checked_next_multiple_of(16)?))
    }

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
        if let Some(reached) = self.counter.get().and_then(Weak::upgrade).and_then(|c| c.reached()) {
            return self.detached_ready.fetch_max(reached, Ordering::AcqRel).max(reached);
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
        #[cfg(target_os = "linux")]
        if self.dmabuf.is_some() {
            return;
        }
        self.observe_ready(value);
    }

    /// Keep `value`, a ready value read from the compositor's device, for [`ExportRing::ready`]
    /// to return once the ring's counter is gone. Values only grow.
    #[cfg_attr(not(all(target_os = "linux", feature = "wgpu")), allow(dead_code))]
    pub(crate) fn observe_ready(&self, value: u64) {
        self.detached_ready.fetch_max(value, Ordering::AcqRel);
    }

    /// Keep the ready value the compositor's device reached (`None` when it could not be read)
    /// after the last copy, which signals `target`. When the value cannot be read or falls short
    /// of `target`, the frames after it never become ready, and [`Fallback::DeviceLost`] is
    /// recorded.
    #[cfg_attr(not(all(target_os = "linux", feature = "wgpu")), allow(dead_code))]
    pub(crate) fn settle_ready(&self, reached: Option<u64>, target: u64) {
        if let Some(done) = reached {
            self.observe_ready(done);
        }
        if reached.is_none_or(|done| done < target) {
            self.refuse(Fallback::DeviceLost);
        }
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
    /// The planes each texture of the ring was last copied from. The copy's commands name the
    /// planes' images without wgpu tracking them, so the planes are kept until the texture is
    /// written again, which happens only after the copy has finished, and when the exporter is
    /// dropped, until the queue has finished the work submitted so far.
    held: Held<wgpu::Texture>,
    /// The ready value of the newest ticket handed to [`Exporter::submitted`].
    signaled: u64,
    queue: wgpu::Queue,
    /// Shared with every [`Ticket`], so the ring's textures stay open on the compositor's device
    /// until the last copy recorded into them has been submitted.
    #[cfg(windows)]
    raw: Arc<Raw>,
    /// Boxed: it holds the device's function tables.
    #[cfg(target_os = "linux")]
    raw: Box<crate::dmabuf_ffi::Exported>,
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
/// buffer has been submitted. It keeps the ring's textures open on the compositor's device, even
/// after the [`Exporter`] is dropped, so drop it only after the command buffer has been submitted
/// or dropped.
#[cfg(feature = "wgpu")]
#[must_use = "the frame is only handed to the encoder by `Exporter::submitted`"]
pub struct Ticket {
    slot: usize,
    ready: u64,
    #[cfg(windows)]
    _textures: Arc<Raw>,
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

/// The two planes a texture of a ring was last copied from, for each texture.
#[derive(Debug)]
#[cfg_attr(not(feature = "wgpu"), allow(dead_code))]
pub(crate) struct Held<T> {
    slots: [[Option<T>; 2]; SLOTS],
}

impl<T> Default for Held<T> {
    fn default() -> Self {
        Held { slots: std::array::from_fn(|_| [None, None]) }
    }
}

#[cfg_attr(not(feature = "wgpu"), allow(dead_code))]
impl<T: Clone + PartialEq> Held<T> {
    /// Keep the first two of `planes` for texture `slot`, in place of the planes kept for it
    /// before. A plane already kept is not cloned again. A slot outside the ring keeps nothing.
    pub(crate) fn keep(&mut self, slot: usize, planes: &[T]) {
        let Some(held) = self.slots.get_mut(slot) else { return };
        for (i, kept) in held.iter_mut().enumerate() {
            let plane = planes.get(i);
            if kept.as_ref() != plane {
                *kept = plane.cloned();
            }
        }
    }

    /// Every plane kept, which the caller drops once the copies from them have finished.
    pub(crate) fn take(&mut self) -> Held<T> {
        std::mem::take(self)
    }

    /// Whether any plane is kept.
    pub(crate) fn is_empty(&self) -> bool {
        self.slots.iter().flatten().all(Option::is_none)
    }
}

/// Whether a ticket whose ready value is `ready` may be signaled after the newest one signaled,
/// `signaled`, which then becomes `ready`. Tickets are signaled in the order `Exporter::copy`
/// handed them out; a ticket that was never submitted may be skipped. An older ticket is refused:
/// the newer ticket's signal went out before its copy, so its ready value could be reached while
/// the copy still runs.
#[cfg_attr(not(feature = "wgpu"), allow(dead_code))]
pub(crate) fn in_order(signaled: &mut u64, ready: u64) -> bool {
    if ready <= *signaled {
        return false;
    }
    *signaled = ready;
    true
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
    #[cfg(target_os = "linux")]
    if let Some((adapter, timeline, dmabuf)) = crate::dmabuf_ffi::export_backend(device) {
        return (crate::Backend::Vulkan { external_memory: dmabuf, timeline }, adapter);
    }
    let _ = device;
    (crate::Backend::Other, None)
}

#[cfg(feature = "wgpu")]
impl Exporter {
    /// An export ring of `width × height` on the adapter of `device`, opened on that device. On
    /// Windows the device must run on Direct3D 12, or on Vulkan with
    /// `VK_KHR_external_memory_win32` and timeline semaphores. On Linux it must run on Vulkan 1.2
    /// with timeline semaphores, `VK_KHR_external_memory_fd`, `VK_EXT_external_memory_dma_buf` and
    /// `VK_EXT_image_drm_format_modifier` enabled (wgpu enables them whenever the adapter has
    /// them), and
    /// export an NV12 image of that size in a memory layout of two planes
    /// ([`check_dmabuf_export`](crate::check_dmabuf_export)). The reason is returned otherwise.
    /// Elsewhere it returns [`Fallback::Unsupported`].
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
            Ok(Exporter {
                ring,
                slots: crate::ring::SlotTable::default(),
                scratch: None,
                held: Held::default(),
                signaled: 0,
                queue: queue.clone(),
                raw: Arc::new(raw),
            })
        }
        #[cfg(target_os = "linux")]
        {
            if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
                return Err(Fallback::ImportFailed);
            }
            crate::fallback::check_export(backend, compositor)?;
            let (ring, raw) = crate::dmabuf_ffi::export(device, queue, width, height)?;
            Ok(Exporter {
                ring,
                slots: crate::ring::SlotTable::default(),
                scratch: None,
                held: Held::default(),
                signaled: 0,
                queue: queue.clone(),
                raw: Box::new(raw),
            })
        }
        #[cfg(not(any(windows, target_os = "linux")))]
        {
            let _ = (backend, compositor);
            ExportRing::open(AdapterId(0), width, height).map(|ring| Exporter {
                ring,
                slots: crate::ring::SlotTable::default(),
                scratch: None,
                held: Held::default(),
                signaled: 0,
                queue: queue.clone(),
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
    /// sizes than the ring's luma and chroma planes are refused with [`Fallback::ImportFailed`].
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
        if !matches!(planes, [luma, chroma] if planes_fit(self.ring.width(), self.ring.height(), size(luma), size(chroma))) {
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
        #[cfg(target_os = "linux")]
        let commands = {
            let _ = device;
            self.raw.copy(planes, slot)
        };
        #[cfg(not(any(windows, target_os = "linux")))]
        let commands: Result<wgpu::CommandBuffer, Fallback> = {
            let _ = (device, planes);
            Err(Fallback::Unsupported)
        };
        let commands = commands.inspect_err(|&reason| self.ring.refuse(reason))?;
        let Some(ready) = self.slots.fill_released(slot, now) else {
            self.ring.refuse(Fallback::ImportFailed);
            return Err(Fallback::ImportFailed);
        };
        self.held.keep(slot, planes);
        let ticket = Ticket {
            slot,
            ready,
            #[cfg(windows)]
            _textures: Arc::clone(&self.raw),
        };
        Ok(Some((commands, ticket)))
    }

    /// The frame of `ticket`, once its command buffer has been submitted: the ready fence is set
    /// after the copy, and the frame is what an encoder session takes. Tickets are handed in in the
    /// order [`Exporter::copy`] returned them, and one whose command buffer was not submitted is
    /// left out. A ticket older than one already handed in is refused with
    /// [`Fallback::ImportFailed`], which stops the export.
    pub fn submitted(&mut self, ticket: Ticket) -> Result<ExportFrame, Fallback> {
        if !in_order(&mut self.signaled, ticket.ready) {
            self.ring.refuse(Fallback::ImportFailed);
            return Err(Fallback::ImportFailed);
        }
        #[cfg(windows)]
        let signaled = match &*self.raw {
            Raw::Dx12(r) => r.signal(ticket.ready),
            Raw::Vulkan(r) => {
                r.signal(ticket.ready);
                Ok(())
            }
        };
        #[cfg(target_os = "linux")]
        let signaled: Result<(), Fallback> = {
            self.raw.signal(ticket.ready);
            Ok(())
        };
        #[cfg(not(any(windows, target_os = "linux")))]
        let signaled: Result<(), Fallback> = Err(Fallback::Unsupported);
        signaled.inspect_err(|&reason| self.ring.refuse(reason))?;
        ExportFrame::new(Arc::clone(&self.ring), ticket.slot, ticket.ready).ok_or(Fallback::ImportFailed)
    }

    /// Copy the values the compositor's queue has reached to the ready fence, without waiting.
    /// Only a Vulkan export has anything to do; [`Exporter::copy`] also calls it. When the ready
    /// fence cannot be set, [`Fallback::ImportFailed`] is recorded on the ring, as a Direct3D 12
    /// export records it when its signal fails. A Linux ring's encoder also reads the queue's value
    /// itself ([`ExportRing::ready`]).
    pub fn poll(&self) {
        #[cfg(windows)]
        if let Raw::Vulkan(r) = &*self.raw
            && !r.poll()
        {
            self.ring.refuse(Fallback::ImportFailed);
        }
        #[cfg(target_os = "linux")]
        self.raw.poll();
    }
}

#[cfg(feature = "wgpu")]
impl Drop for Exporter {
    fn drop(&mut self) {
        // A copy from the planes may still run: they are dropped once the queue has finished the
        // work submitted so far, without waiting here.
        if !self.held.is_empty() {
            let held = self.held.take();
            self.queue.on_submitted_work_done(move || drop(held));
        }
    }
}
