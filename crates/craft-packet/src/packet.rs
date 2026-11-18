//! Encoded packets shared by reference, and the pool that recycles them.

use std::fmt;
use std::sync::Arc;

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

/// One slot of a [`PacketPool`]: the bytes and what they are.
struct Slot {
    data: Vec<u8>,
    info: Info,
    seq: u64,
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
/// Cloning it adds one to a reference count; the bytes are never copied. The packet goes back to
/// its [`PacketPool`] when the last clone is dropped.
#[derive(Clone)]
pub struct SharedPacket(Arc<Slot>);

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
}

impl fmt::Debug for SharedPacket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedPacket").field("seq", &self.0.seq).field("info", &self.0.info).field("len", &self.0.data.len()).finish()
    }
}

/// Packets reused from one packet to the next.
///
/// The pool keeps a reference to every packet it has made. A packet is free again once every
/// [`SharedPacket`] clone of it has been dropped, which the pool sees from the reference count, so
/// dropping a clone costs one atomic decrement and nothing else. Once the pool has made as many
/// packets as are in flight at once, sharing a packet allocates nothing: [`PacketPool::share`]
/// swaps the caller's buffer with a free packet's buffer, and
/// [`PacketPool::share_copy`] copies into a free packet's buffer, which keeps its capacity.
///
/// One thread, the producer, owns the pool. The packets it shares may go to any thread.
pub struct PacketPool {
    slots: Vec<Arc<Slot>>,
    next: usize,
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
        v.try_reserve_exact(slots).map_err(|_| ConfigError::OutOfMemory)?;
        let mut held = 0usize;
        for _ in 0..slots {
            let mut data = Vec::new();
            data.try_reserve_exact(bytes).map_err(|_| ConfigError::OutOfMemory)?;
            held = held.saturating_add(data.capacity());
            v.push(Arc::new(Slot { data, info: Info::data(0), seq: 0 }));
        }
        // The allocator may give more than asked for.
        if held > MAX_POOL_BYTES {
            return Err(ConfigError::PoolTooLarge { slots, bytes });
        }
        Ok(Self { slots: v, next: 0, seq: 0, held })
    }

    /// Share the bytes in `data` as a packet. The buffer moves into a free packet without being
    /// copied, and `data` gets the packet's previous buffer, emptied, for the producer to encode
    /// the next packet into. `data`'s capacity is at most [`MAX_BUFFER_BYTES`]. On an error,
    /// `data` is left as it was.
    pub fn share(&mut self, data: &mut Vec<u8>, info: Info) -> Result<SharedPacket, ShareError> {
        let incoming = data.capacity();
        if incoming > MAX_BUFFER_BYTES {
            return Err(ShareError::BufferTooLarge(incoming));
        }
        self.fill(
            |_| incoming,
            |slot| {
                std::mem::swap(&mut slot.data, data);
                data.clear();
                slot.info = info;
                Ok(())
            },
        )
    }

    /// Share a copy of `data` as a packet. The bytes are copied into a free packet's buffer, which
    /// allocates only when the buffer is smaller than `data`. `data` is at most
    /// [`MAX_BUFFER_BYTES`] long.
    pub fn share_copy(&mut self, data: &[u8], info: Info) -> Result<SharedPacket, ShareError> {
        if data.len() > MAX_BUFFER_BYTES {
            return Err(ShareError::TooLarge(data.len()));
        }
        self.fill(
            |old| old.max(data.len()),
            |slot| {
                slot.data.clear();
                slot.data.try_reserve_exact(data.len()).map_err(|_| ShareError::OutOfMemory)?;
                slot.data.extend_from_slice(data);
                slot.info = info;
                Ok(())
            },
        )
    }

    /// The bytes the pool's buffers hold: their capacity, not their length.
    pub fn held_bytes(&self) -> usize {
        self.held
    }

    /// How many packets the pool has made.
    pub fn made(&self) -> usize {
        self.slots.len()
    }

    /// How many packets are free now. This looks at every packet, so it is for statistics, not
    /// for each packet.
    pub fn free(&mut self) -> usize {
        self.slots.iter_mut().filter_map(|s| Arc::get_mut(s)).count()
    }

    /// Fills the first free packet after the last one used, or a new one when none is free, and
    /// returns a clone of it. `need` gives the buffer capacity `f` leaves, from the capacity
    /// before; the pool refuses when that would take its buffers past [`MAX_POOL_BYTES`].
    ///
    /// `f` runs again on the next free packet only when the allocator gave a buffer larger than
    /// `need`, which `share` never meets, since its buffer's capacity is `need`.
    fn fill(&mut self, need: impl Fn(usize) -> usize, mut f: impl FnMut(&mut Slot) -> Result<(), ShareError>) -> Result<SharedPacket, ShareError> {
        let seq = self.seq + 1;
        let mut held = self.held;
        let n = self.slots.len();
        for i in 0..n {
            let at = (self.next + i) % n;
            let Some(arc) = self.slots.get_mut(at) else { break };
            if let Some(slot) = Arc::get_mut(arc) {
                let old = slot.data.capacity();
                if !fits(held, old, need(old)) {
                    // Another free packet may hold a buffer that is large enough already.
                    continue;
                }
                if let Err(e) = f(slot) {
                    self.held = held;
                    return Err(e);
                }
                slot.seq = seq;
                let now = slot.data.capacity();
                if !fits(held, old, now) {
                    // An allocation larger than asked for: drop it and try the next free packet.
                    slot.data = Vec::new();
                    held = held.saturating_sub(old);
                    continue;
                }
                let p = SharedPacket(Arc::clone(arc));
                self.held = held.saturating_sub(old).saturating_add(now);
                self.seq = seq;
                self.next = (at + 1) % n;
                return Ok(p);
            }
        }
        // Every packet is in flight, or no free one fits: make one more, up to the limits. A new
        // packet never fits where a free one did not, since it starts with no buffer to reuse.
        self.held = held;
        if n >= MAX_SLOTS || !fits(held, 0, need(0)) {
            return Err(ShareError::PoolFull);
        }
        self.slots.try_reserve(1).map_err(|_| ShareError::OutOfMemory)?;
        let mut slot = Slot { data: Vec::new(), info: Info::data(0), seq };
        f(&mut slot)?;
        if !fits(held, 0, slot.data.capacity()) {
            // An allocation larger than asked for, which only `share_copy` can meet.
            return Err(ShareError::PoolFull);
        }
        self.held = held.saturating_add(slot.data.capacity());
        let arc = Arc::new(slot);
        self.slots.push(Arc::clone(&arc));
        self.seq = seq;
        self.next = 0;
        Ok(SharedPacket(arc))
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
