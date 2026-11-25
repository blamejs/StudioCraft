//! The ring of shared textures, the frames that name one of them, and the bookkeeping both sides
//! do without waiting on each other.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::fallback::Fallback;

/// Textures in a ring: one the capture writes, one waiting for the compositor and one the
/// compositor reads.
pub const SLOTS: usize = 3;

/// How long a texture handed to the compositor stays reserved when the compositor has not started
/// to read it (its source was removed, or the frame was passed over). After that the capture takes
/// the texture back and writes it again. A texture the compositor has started to read is written
/// again only once the released fence has passed its value.
pub const RECLAIM: Duration = Duration::from_millis(500);

/// The largest ready value: a texture's claim keeps its value above two tag bits.
const MAX_READY: u64 = u64::MAX >> 2;

/// A texture's claim: its ready value shifted left two bits, with one of these tags. The capture
/// side hands each frame out, and then either the compositor marks it as read or the capture side
/// takes it back, never both.
const HANDED: u64 = 0;
const READING: u64 = 1;
const REVOKED: u64 = 2;

fn claim_of(ready: u64, tag: u64) -> u64 {
    (ready << 2) | tag
}

/// A graphics adapter, by the locally unique identifier the system gives it for the session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AdapterId(pub u64);

impl AdapterId {
    /// The identifier from its low and high parts, as Windows reports them.
    pub fn from_parts(low: u32, high: i32) -> AdapterId {
        AdapterId((u64::from(high as u32) << 32) | u64::from(low))
    }

    /// The low and high parts, as Windows takes them.
    pub fn parts(self) -> (u32, i32) {
        (self.0 as u32, (self.0 >> 32) as u32 as i32)
    }

    /// The identifier from the 8 bytes Vulkan reports as a device's `deviceLUID`: the low part
    /// first, each part in little-endian order.
    pub fn from_luid_bytes(bytes: [u8; 8]) -> AdapterId {
        AdapterId(u64::from_le_bytes(bytes))
    }
}

static NEXT_RING: AtomicU64 = AtomicU64::new(1);

/// The shared textures and fences of one capture, and what the compositor reported about them.
/// The capture side owns the textures; the ring keeps their shared handles open while any frame
/// or importer holds it.
pub struct SharedRing {
    id: u64,
    width: u32,
    height: u32,
    adapter: AdapterId,
    /// Whether the compositor keeps the pictures' alpha (a window's) instead of drawing them
    /// opaque (a display's).
    alpha: bool,
    /// 0 while the frames may stay on the GPU, else a [`Fallback`] code.
    fallback: AtomicU8,
    /// Each texture's claim, written by both sides (see [`HANDED`]).
    claims: [AtomicU64; SLOTS],
    /// Held open for the ring's life; only the importer reads them.
    #[cfg(windows)]
    #[cfg_attr(not(feature = "wgpu"), allow(dead_code))]
    pub(crate) handles: crate::dxgi_ffi::Handles,
    /// The buffers of a Linux screen-cast stream, for a ring made with [`SharedRing::dmabuf`].
    #[cfg(target_os = "linux")]
    pub(crate) dmabuf: Option<Box<crate::dmabuf::DmabufSlots>>,
}

impl std::fmt::Debug for SharedRing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedRing")
            .field("id", &self.id)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("adapter", &self.adapter)
            .field("alpha", &self.alpha)
            .field("fallback", &self.fallback())
            .finish()
    }
}

impl SharedRing {
    #[cfg(windows)]
    pub(crate) fn new(width: u32, height: u32, adapter: AdapterId, alpha: bool, handles: crate::dxgi_ffi::Handles) -> SharedRing {
        SharedRing {
            id: NEXT_RING.fetch_add(1, Ordering::Relaxed),
            width,
            height,
            adapter,
            alpha,
            fallback: AtomicU8::new(0),
            claims: Default::default(),
            handles,
        }
    }

    /// A ring for the buffers of a Linux screen-cast stream: `width` x `height` pictures in
    /// `layout`, negotiated for the compositor device `device`. Its [`MAX_BUFFERS`] slots are
    /// filled with [`SharedRing::set_plane`] as the stream hands its buffers over. The pictures are
    /// drawn opaque.
    ///
    /// [`MAX_BUFFERS`]: crate::MAX_BUFFERS
    #[cfg(target_os = "linux")]
    pub fn dmabuf(width: u32, height: u32, layout: crate::DmabufLayout, device: AdapterId) -> Arc<SharedRing> {
        SharedRing::dmabuf_released_with(width, height, layout, device, Arc::new(AtomicU64::new(0)))
    }

    /// A ring like [`SharedRing::dmabuf`] whose read frames are recorded in `released`, the value
    /// all rings of one stream share.
    #[cfg(target_os = "linux")]
    pub(crate) fn dmabuf_released_with(
        width: u32,
        height: u32,
        layout: crate::DmabufLayout,
        device: AdapterId,
        released: Arc<AtomicU64>,
    ) -> Arc<SharedRing> {
        Arc::new(SharedRing {
            id: NEXT_RING.fetch_add(1, Ordering::Relaxed),
            width,
            height,
            adapter: device,
            alpha: false,
            fallback: AtomicU8::new(0),
            claims: Default::default(),
            dmabuf: Some(Box::new(crate::dmabuf::DmabufSlots::new(layout, device, released))),
        })
    }

    /// Put the plane of the buffer in `slot`. Returns `false`, and drops the plane, for a ring that
    /// does not hold DMA-BUFs, a slot outside the ring, or a slot that already has its plane.
    #[cfg(target_os = "linux")]
    pub fn set_plane(&self, slot: usize, plane: crate::DmabufPlane) -> bool {
        self.dmabuf.as_ref().and_then(|d| d.planes.get(slot)).is_some_and(|p| p.set(plane).is_ok())
    }

    /// The plane of the buffer in `slot`, once it has been put there.
    #[cfg(target_os = "linux")]
    pub fn plane(&self, slot: usize) -> Option<&crate::DmabufPlane> {
        self.dmabuf.as_ref()?.planes.get(slot)?.get()
    }

    /// Record that the compositor has finished reading the frame `ready` of a ring of DMA-BUFs and
    /// every frame before it, so the capture side gives their buffers back to the producer. A lower
    /// value than one recorded before changes nothing.
    #[cfg(target_os = "linux")]
    pub fn release_dmabuf(&self, ready: u64) {
        if let Some(d) = &self.dmabuf {
            d.released.fetch_max(ready, Ordering::AcqRel);
        }
    }

    /// The highest ready value [`SharedRing::release_dmabuf`] has recorded; 0 for a ring of shared
    /// textures.
    #[cfg(target_os = "linux")]
    pub fn dmabuf_released(&self) -> u64 {
        self.dmabuf.as_ref().map_or(0, |d| d.released.load(Ordering::Acquire))
    }

    /// The layout of the ring's buffers, for a ring of DMA-BUFs.
    pub fn dmabuf_layout(&self) -> Option<crate::DmabufLayout> {
        #[cfg(target_os = "linux")]
        {
            self.dmabuf.as_ref().map(|d| d.layout)
        }
        #[cfg(not(target_os = "linux"))]
        {
            None
        }
    }

    /// How many slots the ring has: [`SLOTS`], or [`MAX_BUFFERS`](crate::MAX_BUFFERS) for a ring of
    /// DMA-BUFs.
    pub fn slots(&self) -> usize {
        if self.dmabuf_layout().is_some() { crate::MAX_BUFFERS } else { SLOTS }
    }

    /// A ring without textures, which no device can import: its frames go through the fallback.
    /// For tests and tools that exercise the path without a capture.
    pub fn detached(width: u32, height: u32) -> Arc<SharedRing> {
        Arc::new(SharedRing {
            id: NEXT_RING.fetch_add(1, Ordering::Relaxed),
            width,
            height,
            adapter: AdapterId(0),
            alpha: false,
            fallback: AtomicU8::new(0),
            claims: Default::default(),
            #[cfg(windows)]
            handles: crate::dxgi_ffi::Handles::none(),
            #[cfg(target_os = "linux")]
            dmabuf: None,
        })
    }

    /// A ring without textures whose frames come from `adapter`.
    #[cfg(all(test, windows, feature = "wgpu"))]
    pub(crate) fn detached_on(width: u32, height: u32, adapter: AdapterId) -> Arc<SharedRing> {
        let mut ring = SharedRing::detached(width, height);
        if let Some(r) = Arc::get_mut(&mut ring) {
            r.adapter = adapter;
        }
        ring
    }

    /// Unique in the process; a capture opened again makes a new ring.
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

    /// Whether the pictures are drawn with their own alpha: true for a window's, false for a
    /// display's, which are drawn opaque.
    pub fn keeps_alpha(&self) -> bool {
        self.alpha
    }

    /// Why the compositor cannot take this ring's frames on the GPU, once it has said so.
    pub fn fallback(&self) -> Option<Fallback> {
        Fallback::from_code(self.fallback.load(Ordering::Acquire))
    }

    /// Record why the frames cannot stay on the GPU. The first reason is kept.
    pub fn refuse(&self, reason: Fallback) {
        let _ = self.fallback.compare_exchange(0, reason.code(), Ordering::AcqRel, Ordering::Acquire);
    }

    /// Record that texture `slot` now holds the frame with value `ready`, handed to the
    /// compositor. The capture side calls it before it hands out the frame.
    fn hand_out(&self, slot: usize, ready: u64) {
        if let Some(c) = self.claims.get(slot) {
            c.store(claim_of(ready, HANDED), Ordering::Release);
        }
    }

    /// Mark the frame with value `ready` in texture `slot` as read by the compositor. False when
    /// the capture side took the texture back first, or the texture holds another frame. A ring of
    /// DMA-BUFs is never taken back: the capture side keeps each buffer until its frame is read or
    /// dropped, so its frames are always readable.
    fn start_reading(&self, slot: usize, ready: u64) -> bool {
        if self.dmabuf_layout().is_some() {
            return slot < self.slots();
        }
        self.claims
            .get(slot)
            .is_some_and(|c| c.compare_exchange(claim_of(ready, HANDED), claim_of(ready, READING), Ordering::AcqRel, Ordering::Acquire).is_ok())
    }

    /// Take texture `slot`, holding the frame with value `ready`, back from the compositor. False
    /// when the compositor has started to read it.
    fn revoke(&self, slot: usize, ready: u64) -> bool {
        let Some(c) = self.claims.get(slot) else { return false };
        match c.compare_exchange(claim_of(ready, HANDED), claim_of(ready, REVOKED), Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => true,
            Err(now) => now == claim_of(ready, REVOKED),
        }
    }
}

/// One captured picture: a texture of a ring and the fence value at which its copy is done.
#[derive(Clone, Debug)]
pub struct SharedFrame {
    ring: Arc<SharedRing>,
    slot: u8,
    ready: u64,
    /// Whether a draw has read the picture, for a frame made with [`SharedFrame::watched`].
    drawn: Option<Arc<AtomicBool>>,
}

/// The capture side's view of a frame made with [`SharedFrame::watched`]: whether every copy of the
/// frame is gone without a draw having read it, so its buffer can go back at once.
#[derive(Debug)]
pub struct FrameWatch(Arc<AtomicBool>);

impl FrameWatch {
    /// Whether no copy of the frame is left and none was drawn. A draw records itself before it
    /// lets go of its copy, so a frame that was drawn never counts.
    pub fn dropped_undrawn(&mut self) -> bool {
        Arc::get_mut(&mut self.0).is_some_and(|drawn| !*drawn.get_mut())
    }
}

impl SharedFrame {
    /// A frame in texture `slot` of `ring`, done when the ring's ready fence reaches `ready`.
    /// Returns `None` for a slot outside the ring ([`SharedRing::slots`]).
    pub fn new(ring: Arc<SharedRing>, slot: usize, ready: u64) -> Option<SharedFrame> {
        let slot = u8::try_from(slot).ok().filter(|&s| usize::from(s) < ring.slots())?;
        Some(SharedFrame { ring, slot, ready, drawn: None })
    }

    /// A frame like [`SharedFrame::new`] that records whether a draw read it, which
    /// [`SharedFrame::watch`] lets the capture side see.
    pub fn watched(ring: Arc<SharedRing>, slot: usize, ready: u64) -> Option<SharedFrame> {
        SharedFrame::new(ring, slot, ready).map(|f| SharedFrame { drawn: Some(Arc::new(AtomicBool::new(false))), ..f })
    }

    /// The capture side's view of a watched frame; `None` for one made with [`SharedFrame::new`].
    pub fn watch(&self) -> Option<FrameWatch> {
        self.drawn.as_ref().map(|d| FrameWatch(Arc::clone(d)))
    }

    /// Record that a draw reads the picture. The compositor calls it once the draw is submitted.
    pub fn mark_drawn(&self) {
        if let Some(drawn) = &self.drawn {
            drawn.store(true, Ordering::Release);
        }
    }

    pub fn ring(&self) -> &Arc<SharedRing> {
        &self.ring
    }

    pub fn slot(&self) -> usize {
        usize::from(self.slot)
    }

    /// The ready fence's value at which the texture holds this picture, and the value the
    /// compositor signals on the released fence once it has read it.
    pub fn ready(&self) -> u64 {
        self.ready
    }

    pub fn width(&self) -> u32 {
        self.ring.width
    }

    pub fn height(&self) -> u32 {
        self.ring.height
    }

    /// Mark this frame as read, before the compositor submits its draw. False when the capture
    /// side took its texture back after [`RECLAIM`]: the texture may then hold another picture,
    /// and the frame is not drawn.
    pub fn start_reading(&self) -> bool {
        self.ring.start_reading(self.slot(), self.ready)
    }
}

/// The capture side's record of its ring: the ready value written into each texture and when.
/// Fence values only grow, and the compositor reads frames in the order they were made, so a
/// texture is free when the released fence has reached its value.
#[derive(Clone, Debug)]
pub struct SlotTable {
    ready: [u64; SLOTS],
    written: [Option<Instant>; SLOTS],
    last: u64,
}

impl Default for SlotTable {
    fn default() -> Self {
        SlotTable { ready: [0; SLOTS], written: [None; SLOTS], last: 0 }
    }
}

impl SlotTable {
    /// A texture of `ring` to write at `now`, given the released fence's value: a free one, else
    /// one handed out more than [`RECLAIM`] ago that the compositor has not started to read, which
    /// is taken back from the compositor. The one written longest ago is chosen. `None` when every
    /// texture is still being read.
    pub fn pick(&self, ring: &SharedRing, released: u64, now: Instant) -> Option<usize> {
        let late = |s: usize| self.written[s].is_none_or(|t| now.checked_duration_since(t).is_some_and(|d| d >= RECLAIM));
        let free = |s: usize| self.ready[s] <= released || (late(s) && ring.revoke(s, self.ready[s]));
        (0..SLOTS).filter(|&s| free(s)).min_by_key(|&s| self.ready[s])
    }

    /// A texture to write, given the released fence's value: only a free one, the one written
    /// longest ago, never one still being read however long ago it was handed out. `None` when
    /// every texture is still being read.
    pub fn pick_released(&self, released: u64) -> Option<usize> {
        (0..SLOTS).filter(|&s| self.ready[s] <= released).min_by_key(|&s| self.ready[s])
    }

    /// Record that texture `slot` of `ring` is written at `now` and handed to the compositor;
    /// returns its new ready value, or `None` for a slot outside the ring or when the values ran
    /// out.
    pub fn fill(&mut self, ring: &SharedRing, slot: usize, now: Instant) -> Option<u64> {
        let next = self.fill_released(slot, now)?;
        ring.hand_out(slot, next);
        Some(next)
    }

    /// Record that texture `slot` is written at `now`, for a ring whose textures are picked with
    /// [`SlotTable::pick_released`] and never taken back; returns its new ready value, or `None`
    /// for a slot outside the ring or when the values ran out.
    pub fn fill_released(&mut self, slot: usize, now: Instant) -> Option<u64> {
        let next = self.last.checked_add(1).filter(|&n| n <= MAX_READY)?;
        *self.ready.get_mut(slot)? = next;
        *self.written.get_mut(slot)? = Some(now);
        self.last = next;
        Some(next)
    }

    /// The newest ready value handed out.
    pub fn last(&self) -> u64 {
        self.last
    }
}

/// The compositor side's frames waiting for their copies to finish: the newest, and the one it
/// replaced while that one was still waiting. A frame is taken only when the ready fence has passed
/// its value, so the compositor never waits.
#[derive(Debug, Default)]
pub struct Handoff {
    pending: Option<SharedFrame>,
    /// The frame `pending` replaced, from the same ring; drawn when it is ready and `pending` is
    /// not.
    older: Option<SharedFrame>,
}

impl Handoff {
    /// Keep `frame` until its copy is done. The frame it replaces stays as the older frame, so a
    /// picture whose copy finished is still drawn while the newer one's copy runs; a frame older
    /// than that, or one from another ring, is dropped.
    pub fn offer(&mut self, frame: SharedFrame) {
        let older = self.pending.take().filter(|f| f.ring.id() == frame.ring.id());
        self.older = older;
        self.pending = Some(frame);
    }

    /// The newest waiting frame.
    pub fn pending(&self) -> Option<&SharedFrame> {
        self.pending.as_ref()
    }

    /// The frame the newest one replaced, while it still waits.
    #[cfg(target_os = "linux")]
    #[cfg_attr(not(feature = "wgpu"), allow(dead_code))]
    pub(crate) fn older(&self) -> Option<&SharedFrame> {
        self.older.as_ref()
    }

    /// Forget the waiting frames.
    pub fn clear(&mut self) {
        self.pending = None;
        self.older = None;
    }

    /// The newest waiting frame that `completed` (the ready fence's value now) has reached; a
    /// frame older than it is dropped. A frame not reached stays, and `None` is returned when
    /// neither is reached.
    pub fn take_ready(&mut self, completed: u64) -> Option<SharedFrame> {
        if self.pending.as_ref().is_some_and(|f| f.ready <= completed) {
            self.older = None;
            self.pending.take()
        } else if self.older.as_ref().is_some_and(|f| f.ready <= completed) {
            self.older.take()
        } else {
            None
        }
    }
}
