//! Linux screen-cast pictures as DMA-BUFs: the formats and memory layouts (DRM format modifiers) a
//! compositor's device can import, the check that gives the reason it cannot, and the capture side's
//! record of a stream's buffers.
//!
//! The capture side duplicates each buffer's descriptor the first time the stream hands the buffer
//! over and puts it in a slot of a [`SharedRing`] made with [`SharedRing::dmabuf`]. A frame names
//! the slot. The stream gets its buffer back as soon as the descriptor is duplicated. The compositor
//! draws a frame only once the buffer has no pending write: [`DmabufPlane::ready`] polls the
//! descriptor, which the kernel makes readable once the producer's writes are done (implicit
//! synchronization), and never waits.

use std::sync::Arc;

use arc_swap::ArcSwapOption;

use crate::fallback::Fallback;
use crate::ring::AdapterId;
#[cfg(target_os = "linux")]
use crate::ring::{SharedFrame, SharedRing};

/// Buffers one stream format can have in a ring. A stream that hands over more starts a new ring.
pub const MAX_BUFFERS: usize = 16;

/// The DRM format modifier of a buffer laid out row after row.
pub const MOD_LINEAR: u64 = 0;

/// The DRM format modifier that means "no explicit modifier". The import needs an explicit one, so
/// it is never offered.
pub const MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

/// Most modifiers read from a driver for one format.
const MAX_MODIFIERS: usize = 64;

/// The 4-byte pixel formats a display capture takes as DMA-BUFs, by byte order in memory. The `x`
/// formats carry no alpha.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DmabufFormat {
    Bgra,
    Bgrx,
    Rgba,
    Rgbx,
}

impl DmabufFormat {
    pub const ALL: [DmabufFormat; 4] = [DmabufFormat::Bgrx, DmabufFormat::Bgra, DmabufFormat::Rgbx, DmabufFormat::Rgba];

    /// Whether the bytes are in blue, green, red order (`Bgra8Unorm`), else red, green, blue
    /// (`Rgba8Unorm`).
    pub fn blue_first(self) -> bool {
        matches!(self, DmabufFormat::Bgra | DmabufFormat::Bgrx)
    }

    pub fn has_alpha(self) -> bool {
        matches!(self, DmabufFormat::Bgra | DmabufFormat::Rgba)
    }

    /// The texture format the import opens the buffer as.
    #[cfg(feature = "wgpu")]
    pub fn texture_format(self) -> wgpu::TextureFormat {
        if self.blue_first() { wgpu::TextureFormat::Bgra8Unorm } else { wgpu::TextureFormat::Rgba8Unorm }
    }
}

/// The formats and modifiers a compositor's device can import, and the device they are for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DmabufCaps {
    pub device: AdapterId,
    /// Each format with the single-plane modifiers the device can sample. A format the device
    /// cannot import has none.
    pub formats: Vec<(DmabufFormat, Vec<u64>)>,
}

impl DmabufCaps {
    /// The modifiers the device can import `format` with.
    pub fn modifiers(&self, format: DmabufFormat) -> &[u64] {
        self.formats.iter().find(|(f, _)| *f == format).map_or(&[], |(_, m)| m.as_slice())
    }

    pub fn supports(&self, format: DmabufFormat, modifier: u64) -> bool {
        modifier != MOD_INVALID && self.modifiers(format).contains(&modifier)
    }

    /// Whether any format can be imported.
    pub fn any(&self) -> bool {
        self.formats.iter().any(|(_, m)| !m.is_empty())
    }

    /// Keep at most [`MAX_MODIFIERS`] modifiers per format, without [`MOD_INVALID`] or repeats.
    pub(crate) fn tidy(mut self) -> DmabufCaps {
        for (_, modifiers) in &mut self.formats {
            let mut kept: Vec<u64> = Vec::with_capacity(modifiers.len().min(MAX_MODIFIERS));
            for &m in modifiers.iter() {
                if m != MOD_INVALID && !kept.contains(&m) && kept.len() < MAX_MODIFIERS {
                    kept.push(m);
                }
            }
            *modifiers = kept;
        }
        self
    }
}

/// The capabilities last published. A publish swaps the pointer and a read loads it, without a lock.
static PUBLISHED: ArcSwapOption<DmabufCaps> = ArcSwapOption::const_empty();

/// Record what the compositor's device can import, or that it imports nothing (`None`). The capture
/// side reads it with [`dmabuf_caps`] when it negotiates a stream's format.
pub fn publish_dmabuf_caps(caps: Option<DmabufCaps>) {
    let caps = caps.map(DmabufCaps::tidy).filter(DmabufCaps::any).map(Arc::new);
    PUBLISHED.store(caps);
}

/// What the compositor's device can import, once it has published it.
pub fn dmabuf_caps() -> Option<Arc<DmabufCaps>> {
    PUBLISHED.load_full()
}

/// The layout all buffers of one stream format share.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DmabufLayout {
    pub format: DmabufFormat,
    pub modifier: u64,
}

/// Whether a compositor's device can import buffers in `layout` that were negotiated for the device
/// `stream_device`. `vulkan` is whether the device runs on Vulkan, `extensions` whether it was
/// opened with `VK_EXT_external_memory_dma_buf` and `VK_EXT_image_drm_format_modifier`, and `caps`
/// what it can import. The backend is checked first, then the extensions, then the device, then
/// the format and modifier.
pub fn check_dmabuf(
    vulkan: bool,
    extensions: bool,
    caps: Option<&DmabufCaps>,
    stream_device: AdapterId,
    layout: DmabufLayout,
) -> Result<(), Fallback> {
    if !vulkan {
        return Err(Fallback::UnsupportedBackend);
    }
    let Some(caps) = caps.filter(|_| extensions) else { return Err(Fallback::MissingExtension) };
    if caps.device != stream_device {
        return Err(Fallback::OtherAdapter);
    }
    if !caps.supports(layout.format, layout.modifier) {
        return Err(Fallback::Modifier);
    }
    Ok(())
}

/// A device identifier from the 16-byte UUID Vulkan reports for a physical device.
#[cfg_attr(not(all(target_os = "linux", feature = "wgpu")), allow(dead_code))]
pub(crate) fn adapter_from_uuid(uuid: [u8; 16]) -> AdapterId {
    let (low, high) = uuid.split_at(8);
    let part = |b: &[u8]| b.try_into().map(u64::from_le_bytes).unwrap_or(0);
    AdapterId(part(low) ^ part(high).wrapping_mul(0x9e37_79b9_7f4a_7c15))
}

/// One buffer's single plane: a duplicate of its descriptor, and where the picture starts and how
/// far apart its rows are.
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub struct DmabufPlane {
    pub(crate) fd: std::os::fd::OwnedFd,
    offset: u32,
    stride: u32,
}

#[cfg(target_os = "linux")]
impl DmabufPlane {
    pub fn new(fd: std::os::fd::OwnedFd, offset: u32, stride: u32) -> DmabufPlane {
        DmabufPlane { fd, offset, stride }
    }

    /// A plane with a duplicate of the open descriptor numbered `fd`, which stays the caller's. A
    /// number that is not open gives an error.
    pub fn duplicate(fd: i32, offset: u32, stride: u32) -> std::io::Result<DmabufPlane> {
        crate::dmabuf_ffi::duplicate(fd).map(|fd| DmabufPlane { fd, offset, stride })
    }

    pub fn offset(&self) -> u32 {
        self.offset
    }

    pub fn stride(&self) -> u32 {
        self.stride
    }

    /// Whether the buffer has no pending write, so a read sees the whole picture. It does not wait.
    pub fn ready(&self) -> bool {
        use std::os::fd::AsFd;
        crate::dmabuf_ffi::readable(self.fd.as_fd())
    }
}

/// The DMA-BUF part of a ring: the layout of its buffers, the device they were negotiated for, and
/// each slot's plane once the stream has handed that buffer over.
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub(crate) struct DmabufSlots {
    pub(crate) layout: DmabufLayout,
    #[cfg_attr(not(feature = "wgpu"), allow(dead_code))]
    pub(crate) device: AdapterId,
    pub(crate) planes: [std::sync::OnceLock<DmabufPlane>; MAX_BUFFERS],
    /// The highest ready value whose frame the compositor has finished reading on the GPU, together
    /// with every frame before it. All rings of one stream share it.
    pub(crate) released: Arc<std::sync::atomic::AtomicU64>,
}

#[cfg(target_os = "linux")]
impl DmabufSlots {
    pub(crate) fn new(layout: DmabufLayout, device: AdapterId, released: Arc<std::sync::atomic::AtomicU64>) -> DmabufSlots {
        DmabufSlots { layout, device, planes: std::array::from_fn(|_| std::sync::OnceLock::new()), released }
    }
}

/// Most buffers a stream keeps from its producer at once while the compositor reads them.
pub const MAX_HELD: usize = 3;

/// The buffers a stream keeps from its producer until the compositor has read them, oldest first,
/// each with the ready value of its frame. Ready values grow with every frame, and the compositor's
/// queue finishes its draws in order, so a buffer is free once the GPU has finished reading its
/// frame or any later one. It never allocates after it is made.
#[derive(Debug)]
pub struct HoldQueue<T> {
    items: std::collections::VecDeque<(T, u64)>,
}

impl<T> Default for HoldQueue<T> {
    fn default() -> Self {
        HoldQueue { items: std::collections::VecDeque::with_capacity(MAX_HELD + 1) }
    }
}

impl<T> HoldQueue<T> {
    pub fn new() -> HoldQueue<T> {
        HoldQueue::default()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Keep `item` for the frame `ready`, when fewer than `limit` items, and fewer than
    /// [`MAX_HELD`], are kept. Otherwise `item` comes back as the error: the kept items stay until
    /// the compositor has read their frames, and the caller gives `item` back unread.
    pub fn keep(&mut self, item: T, ready: u64, limit: usize) -> Result<(), T> {
        if self.items.len() >= limit.min(MAX_HELD) {
            return Err(item);
        }
        self.items.push_back((item, ready));
        Ok(())
    }

    /// Give back the items whose frames the compositor has read: those with a ready value up to
    /// `released`.
    pub fn release(&mut self, released: u64, mut give_back: impl FnMut(T)) {
        while self.items.front().is_some_and(|(_, ready)| *ready <= released) {
            if let Some((item, _)) = self.items.pop_front() {
                give_back(item);
            }
        }
    }

    /// Give back the items that `unread` picks, wherever they are in the queue: those whose frames
    /// the compositor will never read, such as a frame dropped before it was drawn.
    pub fn release_unread(&mut self, mut unread: impl FnMut(&mut T) -> bool, mut give_back: impl FnMut(T)) {
        let mut i = 0;
        while i < self.items.len() {
            if unread(&mut self.items[i].0) {
                if let Some((item, _)) = self.items.remove(i) {
                    give_back(item);
                }
            } else {
                i += 1;
            }
        }
    }

    /// Forget the first item that `matches`, without giving it back. Returns whether one did.
    pub fn forget(&mut self, matches: impl Fn(&T) -> bool) -> bool {
        match self.items.iter().position(|(item, _)| matches(item)) {
            Some(i) => self.items.remove(i).is_some(),
            None => false,
        }
    }

    /// Give back every item, oldest first.
    pub fn release_all(&mut self, give_back: impl FnMut(T)) {
        self.items.drain(..).map(|(item, _)| item).for_each(give_back);
    }
}

/// The ready value at which the compositor may read `frame`: its own value once its buffer has no
/// pending write, else 0, so the frame keeps waiting.
#[cfg(target_os = "linux")]
#[cfg_attr(not(feature = "wgpu"), allow(dead_code))]
pub(crate) fn completed(frame: &SharedFrame) -> u64 {
    if frame.ring().plane(frame.slot()).is_some_and(DmabufPlane::ready) { frame.ready() } else { 0 }
}

/// The capture side's record of one stream's DMA-BUF buffers: the format it negotiated, the ring
/// its buffers go into, and the slot of each buffer, by its descriptor number. PipeWire keeps a
/// buffer's descriptor for the buffer's life, so the number names the buffer until the stream's
/// buffers change.
#[cfg(target_os = "linux")]
#[derive(Debug, Default)]
pub struct DmabufStream {
    format: Option<(u32, u32, DmabufLayout, AdapterId)>,
    ring: Option<Arc<SharedRing>>,
    known: [Option<(i32, u32, u32)>; MAX_BUFFERS],
    ready: u64,
    /// What the compositor has read, shared by all of the stream's rings.
    released: Arc<std::sync::atomic::AtomicU64>,
}

#[cfg(target_os = "linux")]
impl DmabufStream {
    pub fn new() -> DmabufStream {
        DmabufStream::default()
    }

    /// The stream now has `width` x `height` buffers in `layout`, negotiated for the device
    /// `device`, or (`None`) buffers in memory. The buffers seen so far are forgotten.
    pub fn set_format(&mut self, format: Option<(u32, u32, DmabufLayout, AdapterId)>) {
        self.format = format;
        self.buffers_changed();
    }

    /// The stream added or removed buffers: the next buffer starts a new ring.
    pub fn buffers_changed(&mut self) {
        self.ring = None;
        self.known = [None; MAX_BUFFERS];
    }

    /// Whether the stream's buffers are DMA-BUFs.
    pub fn active(&self) -> bool {
        self.format.is_some()
    }

    /// The layout of the stream's DMA-BUFs, while they are DMA-BUFs.
    pub fn layout(&self) -> Option<DmabufLayout> {
        self.format.map(|(_, _, layout, _)| layout)
    }

    /// Why the compositor refused the current ring's frames, once it has.
    pub fn refused(&self) -> Option<Fallback> {
        self.ring.as_ref().and_then(|r| r.fallback())
    }

    /// The highest ready value whose frame, and every frame before it, the compositor has finished
    /// reading, from any of the stream's rings; 0 before the first.
    pub fn released(&self) -> u64 {
        self.released.load(std::sync::atomic::Ordering::Acquire)
    }

    /// The frame for the buffer whose plane is the descriptor numbered `fd`, with the picture
    /// `offset` bytes in and rows `stride` bytes apart. `None` when the stream's buffers are not
    /// DMA-BUFs. A buffer seen for the first time has its descriptor duplicated into the ring; a
    /// buffer whose offset or stride changed, or one more than the ring holds, starts a new ring.
    /// An error means the descriptor could not be duplicated.
    pub fn frame(&mut self, fd: i32, offset: u32, stride: u32) -> Result<Option<SharedFrame>, Fallback> {
        let Some((width, height, layout, device)) = self.format else { return Ok(None) };
        if fd < 0 || stride == 0 {
            return Err(Fallback::ImportFailed);
        }
        let mut slot = self.known.iter().position(|k| k.is_some_and(|(f, _, _)| f == fd));
        if slot.is_some_and(|s| self.known[s] != Some((fd, offset, stride))) {
            self.buffers_changed();
            slot = None;
        }
        if slot.is_none() && self.known.iter().all(Option::is_some) {
            self.buffers_changed();
        }
        let released = &self.released;
        let ring = Arc::clone(self.ring.get_or_insert_with(|| SharedRing::dmabuf_released_with(width, height, layout, device, Arc::clone(released))));
        let slot = match slot {
            Some(s) => s,
            None => {
                let Some(free) = self.known.iter().position(Option::is_none) else { return Err(Fallback::ImportFailed) };
                let plane = DmabufPlane::duplicate(fd, offset, stride).map_err(|_| Fallback::ImportFailed)?;
                if !ring.set_plane(free, plane) {
                    return Err(Fallback::ImportFailed);
                }
                self.known[free] = Some((fd, offset, stride));
                free
            }
        };
        self.ready = self.ready.checked_add(1).ok_or(Fallback::ImportFailed)?;
        SharedFrame::watched(ring, slot, self.ready).map(Some).ok_or(Fallback::ImportFailed)
    }
}
