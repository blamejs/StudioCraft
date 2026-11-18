//! One subscriber's bounded ring, the producer's drop rule and the sender's stale-packet rule.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicU64, Ordering, fence};
use std::thread::{self, Thread, ThreadId};
use std::time::{Duration, Instant};

use arc_swap::ArcSwapOption;
use crossbeam_queue::ArrayQueue;
use crossbeam_utils::CachePadded;

use crate::fanout::Shared;
use crate::packet::{Kind, SharedPacket};

/// The size of one subscriber's ring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RingConfig {
    /// How many packets the ring holds, at most [`MAX_RING_SLOTS`]. The ring's storage is
    /// allocated once, when it is made.
    pub slots: usize,
    /// How many encoded bytes the ring holds, at most [`MAX_RING_BYTES`]. A packet counts until the
    /// receiver has finished taking it out of the ring. A packet larger than this is still taken
    /// when the ring is empty.
    pub bytes: usize,
    /// How far, in the stream's time base, a packet may fall behind the newest packet published
    /// before the receiver skips it as stale.
    pub span: i64,
    /// Whether the stream has video. With video, a run of packets starts at a video keyframe; an
    /// audio-only stream starts a run at any audio packet.
    pub video: bool,
    /// How many packets wait in the ring before the producer wakes a parked receiver, 1 to
    /// `slots`. Each wake is a system call on the producer's thread; a larger value wakes the
    /// receiver less often and delivers packets in batches, later by up to that many packets. The
    /// producer also wakes a parked receiver when the ring is full and when the subscription ends.
    /// Above 1, a parked receiver also looks at its ring every [`BATCH_WAIT`] (33 ms), so packets
    /// that arrive before the stream pauses wait at most that long.
    pub wake_every: usize,
}

/// The rates a ring is sized from with [`RingConfig::for_stream`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamRates {
    /// Video packets per second; 0 for an audio-only stream.
    pub video_packets_per_second: u32,
    /// Audio packets per second.
    pub audio_packets_per_second: u32,
    /// Video and audio bits per second together.
    pub bits_per_second: u64,
    /// Units of the stream's time base per second.
    pub ticks_per_second: i64,
}

impl RingConfig {
    /// A ring holding `seconds` of a stream at `rates`: room for that many seconds of packets, and
    /// one and a half times that many seconds of bytes at the stated bit rate. The producer wakes a
    /// parked receiver about 30 times a second: `wake_every` is the packets per second divided by
    /// 30, at least 1 and at most `slots`, so a packet waits at most about 33 ms for its wake.
    ///
    /// The arithmetic saturates. Rates or times too large for a ring give sizes that
    /// [`RingConfig::check`] refuses.
    pub fn for_stream(rates: &StreamRates, seconds: u32) -> Self {
        let s = u64::from(seconds);
        let per_second = u64::from(rates.video_packets_per_second) + u64::from(rates.audio_packets_per_second);
        let packets = s.saturating_mul(per_second);
        let slots = usize::try_from(packets).unwrap_or(usize::MAX);
        let wake_every = usize::try_from(per_second / 30).unwrap_or(usize::MAX).clamp(1, slots.max(1));
        // Bits to bytes and the factor of 1.5 in one step, rounded up: s × bit rate × 3 / 16.
        let bytes = (u128::from(s) * u128::from(rates.bits_per_second) * 3).div_ceil(16);
        Self {
            slots,
            bytes: usize::try_from(bytes).unwrap_or(usize::MAX),
            span: rates.ticks_per_second.saturating_mul(i64::from(seconds)),
            video: rates.video_packets_per_second > 0,
            wake_every,
        }
    }

    /// Checks the sizes: every field must be above 0, `slots` at most [`MAX_RING_SLOTS`], `bytes`
    /// at most [`MAX_RING_BYTES`], and `wake_every` at most `slots`.
    pub fn check(&self) -> Result<(), ConfigError> {
        if self.slots == 0 {
            return Err(ConfigError::NoSlots);
        }
        if self.slots > MAX_RING_SLOTS {
            return Err(ConfigError::TooManyRingSlots(self.slots));
        }
        if self.bytes == 0 {
            return Err(ConfigError::NoBytes);
        }
        if self.bytes > MAX_RING_BYTES {
            return Err(ConfigError::TooManyRingBytes(self.bytes));
        }
        if self.span <= 0 {
            return Err(ConfigError::NoSpan(self.span));
        }
        if self.wake_every == 0 || self.wake_every > self.slots {
            return Err(ConfigError::BadWake(self.wake_every));
        }
        Ok(())
    }
}

/// The most packets a [`crate::PacketPool`] holds, and the most it makes up front: 65 536.
pub const MAX_SLOTS: usize = 65_536;

/// The largest buffer a [`crate::PacketPool`] makes up front for each packet: 16 MiB.
pub const MAX_BUFFER_BYTES: usize = 16 << 20;

/// The most bytes a [`crate::PacketPool`] holds in all its buffers: 256 MiB.
pub const MAX_POOL_BYTES: usize = 256 << 20;

/// The most packets one ring holds: half of [`MAX_SLOTS`], 32 768, about five minutes of a 60 fps
/// stream with 47 audio packets a second. One ring can then never hold every packet of the pool.
pub const MAX_RING_SLOTS: usize = MAX_SLOTS / 2;

/// The most encoded bytes one ring holds: half of [`MAX_POOL_BYTES`], 128 MiB.
pub const MAX_RING_BYTES: usize = MAX_POOL_BYTES / 2;

/// The most subscribers a [`crate::Fanout`] has at once: 64.
pub const MAX_SUBSCRIBERS: usize = 64;

/// How long a receiver whose [`RingConfig::wake_every`] is above 1 stays parked before it looks at
/// its ring again: 33 ms, about one batch at the `wake_every` that [`RingConfig::for_stream`] sets.
pub const BATCH_WAIT: Duration = Duration::from_millis(33);

/// A ring or pool size that is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigError {
    /// The number of packets is 0.
    NoSlots,
    /// A pool's number of packets is above [`MAX_SLOTS`].
    TooManySlots(usize),
    /// A ring's number of packets is above [`MAX_RING_SLOTS`].
    TooManyRingSlots(usize),
    /// A ring's `bytes` is 0.
    NoBytes,
    /// A ring's `bytes` is above [`MAX_RING_BYTES`].
    TooManyRingBytes(usize),
    /// A ring's `span` is 0 or negative.
    NoSpan(i64),
    /// A ring's `wake_every` is 0 or above its `slots`.
    BadWake(usize),
    /// A pool's buffer size is above [`MAX_BUFFER_BYTES`].
    BufferTooLarge(usize),
    /// A pool's buffers together are above [`MAX_POOL_BYTES`].
    PoolTooLarge {
        /// The packets asked for.
        slots: usize,
        /// The buffer size asked for.
        bytes: usize,
    },
    /// The system could not allocate the pool or the ring.
    OutOfMemory,
    /// The fan-out already has [`MAX_SUBSCRIBERS`] subscribers.
    TooManySubscribers,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::NoSlots => f.write_str("room for at least one packet is needed"),
            ConfigError::TooManySlots(n) => write!(f, "{n} packets is more than the limit of {MAX_SLOTS}"),
            ConfigError::TooManyRingSlots(n) => write!(f, "a ring of {n} packets is more than the limit of {MAX_RING_SLOTS}"),
            ConfigError::NoBytes => f.write_str("a ring needs room for at least one byte"),
            ConfigError::TooManyRingBytes(n) => write!(f, "a ring of {n} bytes is more than the limit of {MAX_RING_BYTES}"),
            ConfigError::NoSpan(s) => write!(f, "a ring's span must be above 0, not {s}"),
            ConfigError::BadWake(n) => write!(f, "a ring's wake_every must be from 1 to its number of packets, not {n}"),
            ConfigError::BufferTooLarge(n) => write!(f, "a packet buffer of {n} bytes is more than the limit of {MAX_BUFFER_BYTES}"),
            ConfigError::PoolTooLarge { slots, bytes } => {
                write!(f, "{slots} buffers of {bytes} bytes are more than the pool limit of {MAX_POOL_BYTES} bytes")
            }
            ConfigError::OutOfMemory => f.write_str("there is not enough memory for the packet pool or ring"),
            ConfigError::TooManySubscribers => write!(f, "a fan-out has at most {MAX_SUBSCRIBERS} subscribers"),
        }
    }
}

impl std::error::Error for ConfigError {}

/// Why [`Receiver::try_recv`] or [`Receiver::recv_timeout`] returned no packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecvError {
    /// The ring is empty now.
    Empty,
    /// No packet arrived before the timeout.
    Timeout,
    /// The ring is empty and no packet will come: the fan-out was dropped or the subscriber was
    /// removed.
    Closed,
}

impl fmt::Display for RecvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            RecvError::Empty => "no packet is waiting",
            RecvError::Timeout => "no packet arrived in time",
            RecvError::Closed => "the packet stream has ended",
        })
    }
}

impl std::error::Error for RecvError {}

/// A subscriber's number, unique within its fan-out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SubscriberId(pub u64);

/// One subscriber's counters, read without a lock.
///
/// Every packet offered to a subscriber after it starts is counted once as accepted or dropped, and
/// every accepted packet is later delivered, skipped, discarded or still queued.
///
/// The counters are read one at a time while the producer and the receiver keep running, so they
/// are not one snapshot. They are read in an order that keeps `delivered + skipped + discarded` at
/// most `accepted`, for packets and for bytes, so `accepted - delivered - skipped - discarded` never
/// goes below 0. `queued_packets` and `queued_bytes` may differ from that difference for a moment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Packets and bytes the ring took.
    pub accepted_packets: u64,
    /// See [`Stats::accepted_packets`].
    pub accepted_bytes: u64,
    /// Packets and bytes the producer dropped: the one that found the ring full, and every packet
    /// after it up to the next run start.
    pub dropped_packets: u64,
    /// See [`Stats::dropped_packets`].
    pub dropped_bytes: u64,
    /// Packets and bytes the receiver skipped as stale.
    pub skipped_packets: u64,
    /// See [`Stats::skipped_packets`].
    pub skipped_bytes: u64,
    /// Packets and bytes the receiver handed out.
    pub delivered_packets: u64,
    /// See [`Stats::delivered_packets`].
    pub delivered_bytes: u64,
    /// Packets and bytes released without delivery: those in the ring when the receiver was
    /// dropped or after the subscription ended, and those the producer took out of a full ring to
    /// free pool space ([`crate::Fanout::share`]).
    pub discarded_packets: u64,
    /// See [`Stats::discarded_packets`].
    pub discarded_bytes: u64,
    /// Packets and bytes in the ring now.
    pub queued_packets: usize,
    /// See [`Stats::queued_packets`].
    pub queued_bytes: usize,
    /// Whether the producer is dropping packets until the next run start.
    pub resyncing: bool,
    /// How many times the producer woke the parked receiver.
    pub wakes: u64,
}

/// The ring is taking packets.
const FLOWING: u8 = 0;
/// The ring overflowed; packets are dropped and counted until the next run start.
const RESYNC: u8 = 1;
/// The subscriber has not started yet; packets before its first run start are not counted.
const JOINING: u8 = 2;

/// One packet in [`Ring::held`]: the packet count sits above the low 47 bits, which hold the bytes.
/// A ring holds at most [`MAX_SLOTS`] packets and a packet at most [`MAX_BUFFER_BYTES`] bytes, so
/// neither part overflows.
const ONE_PACKET: u64 = 1 << 47;

/// The packet count and the bytes of `held`.
fn split(held: u64) -> (u64, u64) {
    (held >> 47, held & (ONE_PACKET - 1))
}

/// The counters only the producer writes, written for each packet.
struct ProducerSide {
    state: AtomicU8,
    /// The `dts` of the newest packet offered to the ring. After the ring closes, the receiver
    /// checks its last packets for staleness against this, since the fan-out goes on publishing
    /// to the other rings.
    offered_dts: AtomicI64,
    accepted_packets: AtomicU64,
    accepted_bytes: AtomicU64,
    dropped_packets: AtomicU64,
    dropped_bytes: AtomicU64,
    wakes: AtomicU64,
}

/// The counters the receiver writes for each packet. The producer writes the discarded counters
/// too, but only after the receiver is dropped or when it frees pool space.
#[derive(Default)]
struct ReceiverSide {
    delivered_packets: AtomicU64,
    delivered_bytes: AtomicU64,
    skipped_packets: AtomicU64,
    skipped_bytes: AtomicU64,
    discarded_packets: AtomicU64,
    discarded_bytes: AtomicU64,
}

/// The flags that change when the receiver parks or the subscription ends.
#[derive(Default)]
struct Flags {
    closed: AtomicBool,
    /// Set when the receiver is dropped. From then on, whoever puts a packet in the ring or finds
    /// one there releases it, so a [`Monitor`] that outlives the receiver holds no packets.
    abandoned: AtomicBool,
    waiting: AtomicBool,
    waiter: ArcSwapOption<Thread>,
}

/// One subscriber's ring and counters, shared by the producer and the receiver.
///
/// The fields the producer writes, the fields the receiver writes and the word both change sit on
/// separate cache lines, so a packet moves each line between the two threads' cores only when both
/// threads write it.
pub(crate) struct Ring {
    id: SubscriberId,
    cfg: RingConfig,
    q: ArrayQueue<SharedPacket>,
    /// The packets and bytes the ring holds, in one word so that both change together. A packet
    /// counts from before the producer pushes it until after the receiver has taken it out.
    held: CachePadded<AtomicU64>,
    tx: CachePadded<ProducerSide>,
    rx: CachePadded<ReceiverSide>,
    flags: CachePadded<Flags>,
}

impl Ring {
    /// A ring of `cfg`'s size. `cfg` has passed [`RingConfig::check`], so the queue is at most
    /// [`MAX_RING_SLOTS`] entries of two words each, 512 KiB on a 64-bit system. The same amount
    /// is reserved and released first, and a reservation that fails returns
    /// [`ConfigError::OutOfMemory`]. `ArrayQueue::new` itself cannot report a failed allocation.
    pub(crate) fn new(id: SubscriberId, cfg: RingConfig) -> Result<Self, ConfigError> {
        let mut probe: Vec<[usize; 2]> = Vec::new();
        probe.try_reserve_exact(cfg.slots).map_err(|_| ConfigError::OutOfMemory)?;
        drop(probe);
        Ok(Self {
            id,
            cfg,
            q: ArrayQueue::new(cfg.slots),
            held: CachePadded::new(AtomicU64::new(0)),
            tx: CachePadded::new(ProducerSide {
                state: AtomicU8::new(JOINING),
                offered_dts: AtomicI64::new(i64::MIN),
                accepted_packets: AtomicU64::new(0),
                accepted_bytes: AtomicU64::new(0),
                dropped_packets: AtomicU64::new(0),
                dropped_bytes: AtomicU64::new(0),
                wakes: AtomicU64::new(0),
            }),
            rx: CachePadded::new(ReceiverSide::default()),
            flags: CachePadded::new(Flags::default()),
        })
    }

    pub(crate) fn id(&self) -> SubscriberId {
        self.id
    }

    /// Whether `p` can start a run: a video keyframe, or in an audio-only stream any audio packet.
    fn starts_run(&self, p: &SharedPacket) -> bool {
        if self.cfg.video { p.kind() == Kind::Video && p.keyframe() } else { p.kind() == Kind::Audio }
    }

    /// Offers `p` to the ring without blocking; called by the producer only. Returns whether the
    /// ring took it.
    pub(crate) fn offer(&self, p: &SharedPacket) -> bool {
        if self.flags.closed.load(Ordering::Relaxed) {
            return false;
        }
        // A store to the cache line of the fields that only the producer writes.
        self.tx.offered_dts.store(p.dts(), Ordering::Release);
        let len = p.len();
        match self.tx.state.load(Ordering::Relaxed) {
            FLOWING => {}
            JOINING if !self.starts_run(p) => return false,
            _ if !self.starts_run(p) => {
                self.count_drop(len);
                return false;
            }
            _ => {}
        }
        // The receiver only takes packets and bytes away, so a check that passes here stays true.
        // A packet the receiver is taking out still counts until it has been taken.
        let (packets, used) = split(self.held.load(Ordering::Acquire));
        let add = len as u64;
        if add >= ONE_PACKET || (packets > 0 && used.saturating_add(add) > self.cfg.bytes as u64) {
            return self.overflow(len);
        }
        // Count the packet before the push, so the receiver never takes away one not yet added,
        // and never counts a delivery before the producer has counted the packet as accepted.
        self.held.fetch_add(ONE_PACKET + add, Ordering::AcqRel);
        self.tx.accepted_packets.fetch_add(1, Ordering::Relaxed);
        self.tx.accepted_bytes.fetch_add(add, Ordering::Relaxed);
        if let Err(back) = self.q.push(p.clone()) {
            drop(back);
            self.held.fetch_sub(ONE_PACKET + add, Ordering::AcqRel);
            self.tx.accepted_packets.fetch_sub(1, Ordering::Relaxed);
            self.tx.accepted_bytes.fetch_sub(add, Ordering::Relaxed);
            return self.overflow(len);
        }
        self.tx.state.store(FLOWING, Ordering::Relaxed);
        self.after_push(packets + 1);
        true
    }

    /// Wakes a waiting receiver after a push once `queued` packets reach `wake_every`, or releases
    /// the ring's packets when the receiver has been dropped.
    fn after_push(&self, queued: u64) {
        // Pairs with the fences in `Receiver::recv_timeout` and `Receiver::drop`: either the
        // receiver sees this packet before it parks or releases the ring, or this sees `waiting`
        // or `abandoned`.
        fence(Ordering::SeqCst);
        if self.flags.abandoned.load(Ordering::SeqCst) {
            // The dropped receiver's thread empties the ring until it finds it empty. If it found
            // it empty before this push, the ring holds this one packet, so taking one packet out
            // here leaves it empty, and the producer never drains a whole ring.
            self.discard_one();
            return;
        }
        if queued >= self.cfg.wake_every as u64 {
            self.wake_waiting();
        }
    }

    /// Wakes the receiver if it is parked or about to park.
    fn wake_waiting(&self) {
        if self.flags.waiting.load(Ordering::SeqCst) && self.flags.waiting.swap(false, Ordering::SeqCst) {
            self.tx.wakes.fetch_add(1, Ordering::Relaxed);
            self.wake();
        }
    }

    /// The packets the ring holds now.
    pub(crate) fn queued_packets(&self) -> u64 {
        split(self.held.load(Ordering::Relaxed)).0
    }

    /// The bytes the ring holds now.
    pub(crate) fn queued_bytes(&self) -> u64 {
        split(self.held.load(Ordering::Relaxed)).1
    }

    /// Whether the receiver has fallen behind: the ring is full by packets, or the producer is
    /// dropping packets until the next run start.
    pub(crate) fn is_backed_up(&self) -> bool {
        self.tx.state.load(Ordering::Relaxed) == RESYNC || self.queued_packets() >= self.cfg.slots as u64
    }

    /// Takes every packet out of the ring and releases it, counting it as discarded, and makes the
    /// producer drop packets up to the next run start, as after an overflow. Called by the
    /// producer to free pool space; returns how many of the packets went back to their pool,
    /// which leaves out those that another ring or the caller still holds.
    pub(crate) fn flush(&self) -> usize {
        self.tx.state.store(RESYNC, Ordering::Relaxed);
        let mut n = 0;
        while let Some(last) = self.discard_one() {
            n += usize::from(last);
        }
        n
    }

    /// Takes every packet out of the ring and releases it, counting it as discarded.
    fn discard(&self) {
        while self.discard_one().is_some() {}
    }

    /// Takes the next packet out of the ring, if there is one, and releases it, counting it as
    /// discarded. Returns whether the ring's clone was the packet's last, or `None` when the ring
    /// is empty. The producer and the dropped receiver's thread may both run this at once; each
    /// packet is taken once.
    fn discard_one(&self) -> Option<bool> {
        let p = self.q.pop()?;
        let len = p.len() as u64;
        let last = p.is_last();
        drop(p);
        self.held.fetch_sub(ONE_PACKET + len, Ordering::AcqRel);
        self.rx.discarded_packets.fetch_add(1, Ordering::Release);
        self.rx.discarded_bytes.fetch_add(len, Ordering::Release);
        Some(last)
    }

    fn overflow(&self, len: usize) -> bool {
        self.tx.state.store(RESYNC, Ordering::Relaxed);
        self.count_drop(len);
        // A ring that is full by bytes may hold fewer than `wake_every` packets.
        self.wake_waiting();
        false
    }

    fn count_drop(&self, len: usize) {
        self.tx.dropped_packets.fetch_add(1, Ordering::Relaxed);
        self.tx.dropped_bytes.fetch_add(len as u64, Ordering::Relaxed);
    }

    fn wake(&self) {
        if let Some(t) = self.flags.waiter.load().as_ref() {
            t.unpark();
        }
    }

    /// Stops the ring: the producer offers it nothing more, and a waiting receiver wakes.
    pub(crate) fn close(&self) {
        self.flags.closed.store(true, Ordering::SeqCst);
        self.flags.waiting.store(false, Ordering::SeqCst);
        self.wake();
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.flags.closed.load(Ordering::Acquire)
    }

    pub(crate) fn stats(&self) -> Stats {
        // The receiver's counters first. Each packet they count was counted as accepted before it
        // was pushed, and the acquire loads make those counts visible, so `accepted`, read after
        // them, is at least their sum.
        let delivered_packets = self.rx.delivered_packets.load(Ordering::Acquire);
        let delivered_bytes = self.rx.delivered_bytes.load(Ordering::Acquire);
        let skipped_packets = self.rx.skipped_packets.load(Ordering::Acquire);
        let skipped_bytes = self.rx.skipped_bytes.load(Ordering::Acquire);
        let discarded_packets = self.rx.discarded_packets.load(Ordering::Acquire);
        let discarded_bytes = self.rx.discarded_bytes.load(Ordering::Acquire);
        let (packets, bytes) = split(self.held.load(Ordering::Acquire));
        Stats {
            accepted_packets: self.tx.accepted_packets.load(Ordering::Acquire),
            accepted_bytes: self.tx.accepted_bytes.load(Ordering::Acquire),
            dropped_packets: self.tx.dropped_packets.load(Ordering::Relaxed),
            dropped_bytes: self.tx.dropped_bytes.load(Ordering::Relaxed),
            skipped_packets,
            skipped_bytes,
            delivered_packets,
            delivered_bytes,
            discarded_packets,
            discarded_bytes,
            queued_packets: packets as usize,
            queued_bytes: bytes as usize,
            resyncing: self.tx.state.load(Ordering::Relaxed) == RESYNC,
            wakes: self.tx.wakes.load(Ordering::Relaxed),
        }
    }
}

/// The receiving end of one subscription, used by one thread at a time, such as a destination's
/// sender thread.
///
/// Dropping it removes the subscription from its fan-out and releases the packets left in its ring,
/// including one the producer was offering at that moment.
pub struct Receiver {
    ring: Arc<Ring>,
    shared: Arc<Shared>,
    skipping: bool,
    ended: bool,
    bound: Option<ThreadId>,
}

impl Receiver {
    pub(crate) fn new(ring: Arc<Ring>, shared: Arc<Shared>) -> Self {
        Self { ring, shared, skipping: false, ended: false, bound: None }
    }

    /// The subscription's number.
    pub fn id(&self) -> SubscriberId {
        self.ring.id
    }

    /// The ring's size.
    pub fn config(&self) -> RingConfig {
        self.ring.cfg
    }

    /// The next packet, without waiting.
    ///
    /// A packet that has fallen more than the ring's span behind the newest packet published is
    /// stale. From a stale packet on, the receiver skips packets, counting them, until a run start
    /// that is not stale.
    ///
    /// Once it has returned [`RecvError::Closed`], it returns that from then on. A packet the
    /// producer was offering when the subscription was removed may reach the ring after that; the
    /// next call releases it, counting it as discarded, so it does not keep a pool packet.
    pub fn try_recv(&mut self) -> Result<SharedPacket, RecvError> {
        if self.ended {
            self.ring.discard();
            return Err(RecvError::Closed);
        }
        loop {
            let p = match self.ring.q.pop() {
                Some(p) => p,
                // The producer pushes before it closes, so a second look after seeing the ring
                // closed finds anything pushed before that.
                None if self.ring.is_closed() => match self.ring.q.pop() {
                    Some(p) => p,
                    None => {
                        self.ended = true;
                        return Err(RecvError::Closed);
                    }
                },
                None => return Err(RecvError::Empty),
            };
            let len = p.len();
            self.ring.held.fetch_sub(ONE_PACKET + len as u64, Ordering::AcqRel);
            // Once the ring is closed, the fan-out's newest packet may be one this ring never got,
            // so the newest packet offered to this ring is used instead.
            let newest = if self.ring.is_closed() { self.ring.tx.offered_dts.load(Ordering::Acquire) } else { self.shared.newest() };
            let stale = newest.saturating_sub(p.dts()) > self.ring.cfg.span;
            if stale {
                self.skipping = true;
            }
            if self.skipping {
                if !stale && self.ring.starts_run(&p) {
                    self.skipping = false;
                } else {
                    self.ring.rx.skipped_packets.fetch_add(1, Ordering::Release);
                    self.ring.rx.skipped_bytes.fetch_add(len as u64, Ordering::Release);
                    continue;
                }
            }
            self.ring.rx.delivered_packets.fetch_add(1, Ordering::Release);
            self.ring.rx.delivered_bytes.fetch_add(len as u64, Ordering::Release);
            return Ok(p);
        }
    }

    /// The next packet, parking the thread until one arrives, the subscription ends or `timeout`
    /// passes. The producer wakes the thread only when it finds it waiting.
    pub fn recv_timeout(&mut self, timeout: Duration) -> Result<SharedPacket, RecvError> {
        let deadline = Instant::now().checked_add(timeout);
        loop {
            match self.try_recv() {
                Err(RecvError::Empty) => {}
                other => return other,
            }
            let now = Instant::now();
            let left = match deadline {
                Some(d) if d <= now => return Err(RecvError::Timeout),
                Some(d) => d - now,
                None => Duration::MAX,
            };
            self.bind();
            self.ring.flags.waiting.store(true, Ordering::SeqCst);
            // Pairs with the fence in `Ring::after_push`.
            fence(Ordering::SeqCst);
            if !self.ring.q.is_empty() || self.ring.is_closed() {
                self.ring.flags.waiting.store(false, Ordering::SeqCst);
                continue;
            }
            thread::park_timeout(park_for(&self.ring.cfg, left));
            self.ring.flags.waiting.store(false, Ordering::SeqCst);
        }
    }

    /// Records this thread as the one to wake. It allocates only when the receiver moves to
    /// another thread.
    fn bind(&mut self) {
        let me = thread::current();
        if self.bound != Some(me.id()) {
            self.bound = Some(me.id());
            self.ring.flags.waiter.store(Some(Arc::new(me)));
        }
    }

    /// The subscription's counters.
    pub fn stats(&self) -> Stats {
        self.ring.stats()
    }

    /// Puts `p` in the ring the way an offer that passed the closed check before the subscription
    /// was removed does.
    #[cfg(test)]
    pub(crate) fn push_late(&self, p: SharedPacket) {
        let add = ONE_PACKET + p.len() as u64;
        if self.ring.q.push(p).is_ok() {
            self.ring.held.fetch_add(add, Ordering::AcqRel);
        }
    }

    /// Offers `p` to this ring alone, as `publish` offers it to each ring.
    #[cfg(test)]
    pub(crate) fn offer_only_here(&self, p: &SharedPacket) -> bool {
        self.ring.offer(p)
    }

    /// A weak reference to the ring, to see when it is freed.
    #[cfg(test)]
    pub(crate) fn ring_weak(&self) -> std::sync::Weak<Ring> {
        Arc::downgrade(&self.ring)
    }

    /// A handle that reads the counters from another thread, such as the user interface's.
    pub fn monitor(&self) -> Monitor {
        Monitor(Arc::clone(&self.ring))
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.shared.remove(self.ring.id);
        self.ring.close();
        self.ring.flags.abandoned.store(true, Ordering::SeqCst);
        // Pairs with the fence in `Ring::after_push`: a packet pushed after this sees `abandoned`.
        fence(Ordering::SeqCst);
        self.ring.discard();
    }
}

impl fmt::Debug for Receiver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Receiver").field("id", &self.ring.id).field("config", &self.ring.cfg).finish()
    }
}

/// How long a receiver of a ring of `cfg`'s size parks with `left` of its timeout to go. The
/// producer wakes the receiver only once `wake_every` packets wait, so a receiver that batches
/// looks at its ring again after [`BATCH_WAIT`] in case the stream paused.
pub(crate) fn park_for(cfg: &RingConfig, left: Duration) -> Duration {
    if cfg.wake_every > 1 { left.min(BATCH_WAIT) } else { left }
}

pub(crate) fn monitor(ring: &Arc<Ring>) -> Monitor {
    Monitor(Arc::clone(ring))
}

/// Reads one subscription's counters without a lock.
#[derive(Clone)]
pub struct Monitor(Arc<Ring>);

impl Monitor {
    /// The subscription's number.
    pub fn id(&self) -> SubscriberId {
        self.0.id
    }

    /// The subscription's counters.
    pub fn stats(&self) -> Stats {
        self.0.stats()
    }

    /// Whether the subscription has ended.
    pub fn is_closed(&self) -> bool {
        self.0.is_closed()
    }

    /// Whether the receiver is parked, or about to park, waiting for the producer to wake it.
    pub fn is_waiting(&self) -> bool {
        self.0.flags.waiting.load(Ordering::SeqCst)
    }

    /// Marks the receiver as waiting, as `recv_timeout` does before it parks.
    #[cfg(test)]
    pub(crate) fn set_waiting(&self) {
        self.0.flags.waiting.store(true, Ordering::SeqCst);
    }

    /// Puts `p` in the ring the way an offer that passed the closed check before the receiver was
    /// dropped does.
    #[cfg(test)]
    pub(crate) fn push_in_flight(&self, p: SharedPacket) {
        let add = ONE_PACKET + p.len() as u64;
        self.0.held.fetch_add(add, Ordering::AcqRel);
        if self.0.q.push(p).is_err() {
            self.0.held.fetch_sub(add, Ordering::AcqRel);
            return;
        }
        self.0.tx.accepted_packets.fetch_add(1, Ordering::Relaxed);
        self.0.after_push(1);
    }
}

impl fmt::Debug for Monitor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Monitor").field("id", &self.0.id).finish()
    }
}
