//! Encoded packets shared by reference, and the pool that recycles them.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering, fence};

use crossbeam_queue::ArrayQueue;

use crate::ring::{ConfigError, MAX_BUFFER_BYTES, MAX_POOL_BYTES, MAX_SLOTS};

/// What a packet carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    /// A compressed picture.
    Video,
    /// A compressed audio frame.
    Audio,
    /// Anything else the stream carries, such as metadata.
    Data,
}

/// A packet's type and timing.
///
/// `pts` and `dts` are in one time base that the producer chooses. Every packet of a stream, video,
/// audio and data alike, uses the same time base, and packets are published in `dts` order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Info {
    /// What the packet carries.
    pub kind: Kind,
    /// When the packet is presented.
    pub pts: i64,
    /// When the packet is decoded.
    pub dts: i64,
    /// For video, whether the picture decodes without earlier pictures.
    pub keyframe: bool,
}

impl Info {
    /// A compressed picture.
    pub fn video(pts: i64, dts: i64, keyframe: bool) -> Self {
        Self { kind: Kind::Video, pts, dts, keyframe }
    }

    /// A compressed audio frame, decoded when it is presented.
    pub fn audio(pts: i64) -> Self {
        Self { kind: Kind::Audio, pts, dts: pts, keyframe: false }
    }

    /// Data such as metadata, at `pts`.
    pub fn data(pts: i64) -> Self {
        Self { kind: Kind::Data, pts, dts: pts, keyframe: false }
    }
}

/// The smallest buffer [`PacketPool::share_copy`] makes, in bytes.
const MIN_BUFFER: usize = 64;

/// A packet keeps a buffer of at most this many times its length (or [`MIN_BUFFER`], if larger).
/// [`PacketPool::share`] copies a packet whose buffer is larger than that.
const SLACK: usize = 4;

/// The free lists: list 0 holds buffers with no capacity, and list `c` above 0 holds buffers whose
/// capacity is at least 2^(c-1) and below 2^c. [`MAX_BUFFER_BYTES`] is 2^24, in list 25.
const CLASSES: usize = 26;

/// The end of a free list.
const NONE: u32 = u32::MAX;

/// How many packets of a free list [`PacketPool::share_copy`] looks at for a buffer that holds a
/// packet, before it takes one from the list above, whose buffers all hold it. When the list above
/// is empty, it looks through the rest of the list before it allocates.
const SEARCH: usize = 8;

/// The free list of a buffer of `cap` bytes.
fn class(cap: usize) -> usize {
    ((usize::BITS - cap.leading_zeros()) as usize).min(CLASSES - 1)
}

/// One slot of a [`PacketPool`]: the bytes and what they are.
struct Slot {
    data: Vec<u8>,
    info: Info,
    seq: u64,
    /// The slot's place in its pool.
    index: u32,
    /// How many [`SharedPacket`] clones of the slot exist.
    users: AtomicUsize,
    /// The pool's queue of slots whose last clone has been dropped.
    home: Arc<ArrayQueue<u32>>,
}

/// Why [`PacketPool::share`] or [`PacketPool::share_copy`] made no packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShareError {
    /// [`MAX_SLOTS`] packets are in flight, or the packet would take the pool's buffers past
    /// [`MAX_POOL_BYTES`].
    PoolFull,
    /// The system could not allocate the packet.
    OutOfMemory,
    /// [`PacketPool::share_copy`] was given more than [`MAX_BUFFER_BYTES`] bytes of data.
    TooLarge(usize),
    /// [`PacketPool::share`] was given a buffer whose capacity, the value here, is more than
    /// [`MAX_BUFFER_BYTES`], whatever its length.
    BufferTooLarge(usize),
}

impl fmt::Display for ShareError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ShareError::PoolFull => {
                write!(f, "the packet pool is full: at most {MAX_SLOTS} packets and {MAX_POOL_BYTES} bytes of buffers")
            }
            ShareError::OutOfMemory => f.write_str("there is not enough memory for the packet"),
            ShareError::TooLarge(n) => write!(f, "a packet of {n} bytes is larger than {MAX_BUFFER_BYTES} bytes"),
            ShareError::BufferTooLarge(n) => {
                write!(f, "a packet buffer with a capacity of {n} bytes is larger than {MAX_BUFFER_BYTES} bytes; shrink its capacity")
            }
        }
    }
}

impl std::error::Error for ShareError {}

/// An encoded packet shared by reference.
///
/// Cloning it adds one to two reference counts; the bytes are never copied. When the last clone is
/// dropped, the thread that drops it puts the packet on its [`PacketPool`]'s queue of free
/// packets, without allocating or taking a lock.
pub struct SharedPacket(Arc<Slot>);

impl Clone for SharedPacket {
    fn clone(&self) -> Self {
        self.0.users.fetch_add(1, Ordering::Relaxed);
        SharedPacket(Arc::clone(&self.0))
    }
}

impl Drop for SharedPacket {
    fn drop(&mut self) {
        if self.0.users.fetch_sub(1, Ordering::Release) == 1 {
            // Pairs with the release above in the other clones' drops: every read of the bytes
            // happens before the pool reuses the buffer.
            fence(Ordering::Acquire);
            // The queue has room for every slot the pool can make, and a slot is in it at most
            // once, so the push always succeeds.
            let _ = self.0.home.push(self.0.index);
        }
    }
}

impl SharedPacket {
    /// The encoded bytes.
    pub fn data(&self) -> &[u8] {
        &self.0.data
    }

    /// How many encoded bytes the packet holds.
    pub fn len(&self) -> usize {
        self.0.data.len()
    }

    /// Whether the packet holds no bytes.
    pub fn is_empty(&self) -> bool {
        self.0.data.is_empty()
    }

    /// The capacity of the packet's buffer: the memory it keeps from its pool while it is in
    /// flight. It is at most 4 times the packet's length, or 256 bytes, whichever is larger.
    pub fn capacity(&self) -> usize {
        self.0.data.capacity()
    }

    /// The packet's type and timing.
    pub fn info(&self) -> Info {
        self.0.info
    }

    /// What the packet carries.
    pub fn kind(&self) -> Kind {
        self.0.info.kind
    }

    /// When the packet is presented.
    pub fn pts(&self) -> i64 {
        self.0.info.pts
    }

    /// When the packet is decoded.
    pub fn dts(&self) -> i64 {
        self.0.info.dts
    }

    /// Whether the packet is a video keyframe.
    pub fn keyframe(&self) -> bool {
        self.0.info.keyframe
    }

    /// The packet's number: its pool numbers packets 1, 2, 3 and on in the order they are shared,
    /// so a gap shows where packets were dropped.
    pub fn seq(&self) -> u64 {
        self.0.seq
    }

    /// Whether `a` and `b` are clones of one packet.
    pub fn ptr_eq(a: &Self, b: &Self) -> bool {
        Arc::ptr_eq(&a.0, &b.0)
    }

    /// The number of the pool the packet came from. Each packet keeps its pool's queue alive, so
    /// no other pool has this number while the packet exists.
    pub(crate) fn pool_id(&self) -> usize {
        Arc::as_ptr(&self.0.home) as usize
    }

    /// Whether this is the packet's only clone, so dropping it returns the packet to its pool.
    pub(crate) fn is_last(&self) -> bool {
        self.0.users.load(Ordering::Acquire) == 1
    }
}

impl fmt::Debug for SharedPacket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedPacket").field("seq", &self.0.seq).field("info", &self.0.info).field("len", &self.0.data.len()).finish()
    }
}

/// Packets reused from one packet to the next.
///
/// The last [`SharedPacket`] clone of a packet to be dropped puts the packet on the pool's queue
/// of returned packets. The pool sorts returned packets into free lists by buffer capacity, in
/// powers of two, and takes a packet from the list that fits, so finding a free packet costs the
/// same however many packets are in flight. A packet's buffer is at most 4 times its length (or
/// 256 bytes), so audio packets do not keep video-sized buffers.
///
/// Once the pool holds as many packets of each size as are in flight at once, sharing a packet
/// allocates nothing: [`PacketPool::share`] swaps the caller's buffer with a free packet's buffer
/// of the same size, and [`PacketPool::share_copy`] copies into a free packet's buffer. Until
/// then, sharing a packet may make one more packet (two allocations) or replace one free buffer.
/// The pool reserves room for the most packets it can hold when it is made (about 1.8 MiB), so
/// making more packets never moves its tables.
///
/// One thread, the producer, owns the pool. The packets it shares may go to any thread.
pub struct PacketPool {
    slots: Vec<Arc<Slot>>,
    /// For each free packet, the next packet in its free list.
    links: Vec<u32>,
    /// The first packet of each free list.
    heads: [u32; CLASSES],
    /// Packets whose last clone has been dropped while the dropping thread still holds the
    /// packet's reference count. They are free once it lets go.
    pending: u32,
    /// Packets whose last clone has been dropped, put there by the dropping thread.
    returned: Arc<ArrayQueue<u32>>,
    /// How many packets are in the free lists.
    free: usize,
    seq: u64,
    /// The capacity of every buffer in the pool, at most [`MAX_POOL_BYTES`].
    held: usize,
}

impl PacketPool {
    /// A pool with `slots` packets made up front, 1 to [`MAX_SLOTS`], each with a buffer of
    /// `bytes` capacity, at most [`MAX_BUFFER_BYTES`], and the buffers together at most
    /// [`MAX_POOL_BYTES`]. An allocation that fails returns [`ConfigError::OutOfMemory`]. The pool
    /// makes more packets later when more are in flight at once, up to [`MAX_SLOTS`] in all, and
    /// its buffers never hold more than [`MAX_POOL_BYTES`] together.
    pub fn new(slots: usize, bytes: usize) -> Result<Self, ConfigError> {
        if slots == 0 {
            return Err(ConfigError::NoSlots);
        }
        if slots > MAX_SLOTS {
            return Err(ConfigError::TooManySlots(slots));
        }
        if bytes > MAX_BUFFER_BYTES {
            return Err(ConfigError::BufferTooLarge(bytes));
        }
        match slots.checked_mul(bytes) {
            Some(total) if total <= MAX_POOL_BYTES => {}
            _ => return Err(ConfigError::PoolTooLarge { slots, bytes }),
        }
        let mut v = Vec::new();
        v.try_reserve_exact(MAX_SLOTS).map_err(|_| ConfigError::OutOfMemory)?;
        let mut links = Vec::new();
        links.try_reserve_exact(MAX_SLOTS).map_err(|_| ConfigError::OutOfMemory)?;
        // `ArrayQueue::new` cannot report a failed allocation, so the same amount (an index and a
        // stamp for each entry) is reserved and released first.
        let mut probe: Vec<[usize; 2]> = Vec::new();
        probe.try_reserve_exact(MAX_SLOTS).map_err(|_| ConfigError::OutOfMemory)?;
        drop(probe);
        let returned = Arc::new(ArrayQueue::new(MAX_SLOTS));
        let mut pool = Self { slots: v, links, heads: [NONE; CLASSES], pending: NONE, returned, free: 0, seq: 0, held: 0 };
        for _ in 0..slots {
            let Some(i) = pool.grow() else {
                return Err(ConfigError::TooManySlots(slots));
            };
            if let Some(slot) = pool.slot(i) {
                slot.data.try_reserve_exact(bytes).map_err(|_| ConfigError::OutOfMemory)?;
                let cap = slot.data.capacity();
                pool.held = pool.held.saturating_add(cap);
            }
            pool.push_free(i);
        }
        // The allocator may give more than asked for.
        if pool.held > MAX_POOL_BYTES {
            return Err(ConfigError::PoolTooLarge { slots, bytes });
        }
        Ok(pool)
    }

    /// Share the bytes in `data` as a packet. The buffer moves into a free packet without being
    /// copied, and `data` gets the packet's previous buffer, emptied, for the producer to encode
    /// the next packet into. The pool picks a free packet whose buffer is about as large as
    /// `data`'s, so the producer gets back a buffer of the size it uses.
    ///
    /// When `data`'s capacity is more than 4 times its length (and more than 256 bytes), the
    /// bytes are copied into a pooled buffer instead, as [`PacketPool::share_copy`] does, and
    /// `data` keeps its buffer, emptied. A packet therefore never keeps a buffer much larger than
    /// its bytes.
    ///
    /// `data`'s capacity is at most [`MAX_BUFFER_BYTES`]. On an error, `data` is left as it was.
    pub fn share(&mut self, data: &mut Vec<u8>, info: Info) -> Result<SharedPacket, ShareError> {
        let cap = data.capacity();
        if cap > MAX_BUFFER_BYTES {
            return Err(ShareError::BufferTooLarge(cap));
        }
        if cap > SLACK * data.len().max(MIN_BUFFER) {
            let p = self.share_copy(data, info)?;
            data.clear();
            return Ok(p);
        }
        self.collect();
        let want = class(cap);
        let Some(i) = self.pop_free(want).or_else(|| self.grow()).or_else(|| self.steal(want)) else {
            return Err(ShareError::PoolFull);
        };
        let old = self.slot(i).map_or(0, |s| s.data.capacity());
        if !self.make_room(old, cap) {
            self.push_free(i);
            return Err(ShareError::PoolFull);
        }
        let seq = self.seq + 1;
        let Some(slot) = self.slot(i) else {
            return Err(ShareError::PoolFull);
        };
        std::mem::swap(&mut slot.data, data);
        data.clear();
        slot.info = info;
        slot.seq = seq;
        slot.users.store(1, Ordering::Relaxed);
        self.held = self.held.saturating_sub(old).saturating_add(cap);
        self.seq = seq;
        self.packet(i)
    }

    /// Share a copy of `data` as a packet. The bytes are copied into a free packet's buffer. It
    /// allocates only when no free buffer of the right size is left: the new buffer's capacity is
    /// the next power of two at or above `data`'s length, and at least 64 bytes. `data` is at most
    /// [`MAX_BUFFER_BYTES`] long.
    pub fn share_copy(&mut self, data: &[u8], info: Info) -> Result<SharedPacket, ShareError> {
        let len = data.len();
        if len > MAX_BUFFER_BYTES {
            return Err(ShareError::TooLarge(len));
        }
        self.collect();
        let base = class(len.max(MIN_BUFFER));
        // A free buffer near the front of `len`'s own list that holds it, any in the list above,
        // which all do, and only then, before allocating, the rest of `len`'s own list.
        let fitting = self.take_fitting(base, len, SEARCH).or_else(|| self.pop_free(base + 1)).or_else(|| self.take_fitting(base, len, usize::MAX));
        let i = match fitting {
            Some(i) => i,
            None => {
                let Some(i) = self.grow().or_else(|| self.steal(base + 1)) else {
                    return Err(ShareError::PoolFull);
                };
                // Replace the packet's buffer with one of the right size.
                let target = len.max(MIN_BUFFER).next_power_of_two();
                if let Some(slot) = self.slot(i) {
                    let old = std::mem::take(&mut slot.data).capacity();
                    self.held = self.held.saturating_sub(old);
                }
                if !self.make_room(0, target) {
                    self.push_free(i);
                    return Err(ShareError::PoolFull);
                }
                let Some(slot) = self.slot(i) else {
                    return Err(ShareError::PoolFull);
                };
                if slot.data.try_reserve_exact(target).is_err() {
                    self.push_free(i);
                    return Err(ShareError::OutOfMemory);
                }
                let now = slot.data.capacity();
                // The allocator may give more than asked for.
                if !fits(self.held, 0, now) {
                    if let Some(slot) = self.slot(i) {
                        slot.data = Vec::new();
                    }
                    self.push_free(i);
                    return Err(ShareError::PoolFull);
                }
                self.held = self.held.saturating_add(now);
                i
            }
        };
        let seq = self.seq + 1;
        let Some(slot) = self.slot(i) else {
            return Err(ShareError::PoolFull);
        };
        slot.data.clear();
        slot.data.extend_from_slice(data);
        slot.info = info;
        slot.seq = seq;
        slot.users.store(1, Ordering::Relaxed);
        self.seq = seq;
        self.packet(i)
    }

    /// The bytes the pool's buffers hold: their capacity, not their length.
    pub fn held_bytes(&self) -> usize {
        self.held
    }

    /// How many packets the pool has made.
    pub fn made(&self) -> usize {
        self.slots.len()
    }

    /// How many packets are free now.
    pub fn free(&mut self) -> usize {
        self.collect();
        self.free
    }

    /// The pool's number, as [`SharedPacket::pool_id`] gives it for the pool's packets.
    pub(crate) fn id(&self) -> usize {
        Arc::as_ptr(&self.returned) as usize
    }

    /// The packet at `i`, writable: it is free, so no clone of it exists.
    fn slot(&mut self, i: u32) -> Option<&mut Slot> {
        self.slots.get_mut(i as usize).and_then(Arc::get_mut)
    }

    /// The capacity of packet `i`'s buffer.
    fn capacity(&self, i: u32) -> usize {
        self.slots.get(i as usize).map_or(0, |s| s.data.capacity())
    }

    /// The first clone of packet `i`.
    fn packet(&self, i: u32) -> Result<SharedPacket, ShareError> {
        self.slots.get(i as usize).map(|a| SharedPacket(Arc::clone(a))).ok_or(ShareError::PoolFull)
    }

    /// Moves the packets returned since the last call into the free lists.
    fn collect(&mut self) {
        let mut p = std::mem::replace(&mut self.pending, NONE);
        while p != NONE {
            let next = self.links.get(p as usize).copied().unwrap_or(NONE);
            self.settle(p);
            p = next;
        }
        while let Some(i) = self.returned.pop() {
            self.settle(i);
        }
    }

    /// Puts returned packet `i` in its free list, or back in `pending` while the thread that
    /// dropped it still holds its reference count.
    fn settle(&mut self, i: u32) {
        if self.slot(i).is_some() {
            self.push_free(i);
        } else if let Some(link) = self.links.get_mut(i as usize) {
            *link = self.pending;
            self.pending = i;
        }
    }

    fn push_free(&mut self, i: u32) {
        let c = class(self.capacity(i));
        if let (Some(link), Some(head)) = (self.links.get_mut(i as usize), self.heads.get_mut(c)) {
            *link = *head;
            *head = i;
            self.free += 1;
        }
    }

    fn pop_free(&mut self, c: usize) -> Option<u32> {
        let head = *self.heads.get(c)?;
        if head == NONE {
            return None;
        }
        let next = self.links.get(head as usize).copied().unwrap_or(NONE);
        if let Some(h) = self.heads.get_mut(c) {
            *h = next;
        }
        self.free -= 1;
        Some(head)
    }

    /// A free packet from list `c` whose buffer holds `len` bytes, among the first `limit` of the
    /// list, taken out of the list.
    fn take_fitting(&mut self, c: usize, len: usize, limit: usize) -> Option<u32> {
        let mut prev = NONE;
        let mut at = *self.heads.get(c)?;
        for _ in 0..limit {
            if at == NONE {
                return None;
            }
            let next = self.links.get(at as usize).copied().unwrap_or(NONE);
            if self.capacity(at) >= len {
                let link = if prev == NONE { self.heads.get_mut(c) } else { self.links.get_mut(prev as usize) };
                if let Some(link) = link {
                    *link = next;
                }
                self.free -= 1;
                return Some(at);
            }
            prev = at;
            at = next;
        }
        None
    }

    /// A free packet from the list nearest to `near`. Every list is searched, list 0 included, even
    /// when `near` is past the last list.
    fn steal(&mut self, near: usize) -> Option<u32> {
        let near = near.min(CLASSES - 1);
        (0..CLASSES).find_map(|d| self.pop_free(near + d).or_else(|| near.checked_sub(d).and_then(|c| self.pop_free(c))))
    }

    /// Makes one more packet, with no buffer, up to [`MAX_SLOTS`]. It is in no free list.
    fn grow(&mut self) -> Option<u32> {
        let n = self.slots.len();
        if n >= MAX_SLOTS {
            return None;
        }
        let index = u32::try_from(n).ok()?;
        // Both tables were reserved for MAX_SLOTS entries, so neither moves.
        self.slots.push(Arc::new(Slot {
            data: Vec::new(),
            info: Info::data(0),
            seq: 0,
            index,
            users: AtomicUsize::new(0),
            home: Arc::clone(&self.returned),
        }));
        self.links.push(NONE);
        Some(index)
    }

    /// Whether the pool's buffers fit within [`MAX_POOL_BYTES`] when one of `old` bytes becomes one
    /// of `new` bytes. When they would not, the largest free buffers are released until they do.
    fn make_room(&mut self, old: usize, new: usize) -> bool {
        loop {
            if fits(self.held, old, new) {
                return true;
            }
            let Some(i) = (1..CLASSES).rev().find_map(|c| self.pop_free(c)) else {
                return false;
            };
            if let Some(slot) = self.slot(i) {
                let cap = std::mem::take(&mut slot.data).capacity();
                self.held = self.held.saturating_sub(cap);
            }
            self.push_free(i);
        }
    }
}

/// Whether a pool holding `held` bytes stays within [`MAX_POOL_BYTES`] when a buffer of `old`
/// bytes becomes one of `new` bytes.
fn fits(held: usize, old: usize, new: usize) -> bool {
    held.saturating_sub(old).saturating_add(new) <= MAX_POOL_BYTES
}

impl fmt::Debug for PacketPool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PacketPool").field("made", &self.slots.len()).field("seq", &self.seq).finish()
    }
}
