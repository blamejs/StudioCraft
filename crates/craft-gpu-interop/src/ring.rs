//! The ring of shared textures, the frames that name one of them, and the bookkeeping both sides
//! do without waiting on each other.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::fallback::Fallback;

/// Textures in a ring: one the capture writes, one waiting for the compositor and one the
/// compositor reads.
pub const SLOTS: usize = 3;

/// How long a texture handed to the compositor stays reserved when the compositor never reports
/// that it read it (its source was removed, or its device was lost). After that the capture
/// writes the texture again.
pub const RECLAIM: Duration = Duration::from_millis(500);

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
    /// Held open for the ring's life; only the importer reads them.
    #[cfg(windows)]
    #[cfg_attr(not(feature = "wgpu"), allow(dead_code))]
    pub(crate) handles: crate::dxgi_ffi::Handles,
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
        SharedRing { id: NEXT_RING.fetch_add(1, Ordering::Relaxed), width, height, adapter, alpha, fallback: AtomicU8::new(0), handles }
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
            #[cfg(windows)]
            handles: crate::dxgi_ffi::Handles::none(),
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
}

/// One captured picture: a texture of a ring and the fence value at which its copy is done.
#[derive(Clone, Debug)]
pub struct SharedFrame {
    ring: Arc<SharedRing>,
    slot: u8,
    ready: u64,
}

impl SharedFrame {
    /// A frame in texture `slot` of `ring`, done when the ring's ready fence reaches `ready`.
    /// Returns `None` for a slot outside the ring.
    pub fn new(ring: Arc<SharedRing>, slot: usize, ready: u64) -> Option<SharedFrame> {
        let slot = u8::try_from(slot).ok().filter(|&s| usize::from(s) < SLOTS)?;
        Some(SharedFrame { ring, slot, ready })
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
    /// A texture to write at `now`, given the released fence's value: a free one, else one that
    /// was handed out more than [`RECLAIM`] ago. The one written longest ago is chosen. `None`
    /// when every texture is still being read.
    pub fn pick(&self, released: u64, now: Instant) -> Option<usize> {
        let free =
            |s: usize| self.ready[s] <= released || self.written[s].is_none_or(|t| now.checked_duration_since(t).is_some_and(|d| d >= RECLAIM));
        (0..SLOTS).filter(|&s| free(s)).min_by_key(|&s| self.ready[s])
    }

    /// A texture to write, given the released fence's value: only a free one, the one written
    /// longest ago, never one still being read however long ago it was handed out. `None` when
    /// every texture is still being read.
    pub fn pick_released(&self, released: u64) -> Option<usize> {
        (0..SLOTS).filter(|&s| self.ready[s] <= released).min_by_key(|&s| self.ready[s])
    }

    /// Record that texture `slot` is written at `now`; returns its new ready value, or `None`
    /// for a slot outside the ring or when the values ran out.
    pub fn fill(&mut self, slot: usize, now: Instant) -> Option<u64> {
        let next = self.last.checked_add(1)?;
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

/// The compositor side's frame waiting for its copy to finish. A newer frame replaces it, and it
/// is taken only when the ready fence has passed its value, so the compositor never waits.
#[derive(Debug, Default)]
pub struct Handoff {
    pending: Option<SharedFrame>,
}

impl Handoff {
    /// Keep `frame` until its copy is done, dropping an older frame that was not drawn.
    pub fn offer(&mut self, frame: SharedFrame) {
        self.pending = Some(frame);
    }

    pub fn pending(&self) -> Option<&SharedFrame> {
        self.pending.as_ref()
    }

    /// Forget the waiting frame.
    pub fn clear(&mut self) {
        self.pending = None;
    }

    /// The waiting frame, when `completed` (the ready fence's value now) has reached it.
    /// Otherwise the frame stays and `None` is returned.
    pub fn take_ready(&mut self, completed: u64) -> Option<SharedFrame> {
        if self.pending.as_ref().is_some_and(|f| f.ready <= completed) { self.pending.take() } else { None }
    }
}
