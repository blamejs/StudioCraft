//! One subscriber's bounded ring, the producer's drop rule and the sender's stale-packet rule.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicU64, Ordering, fence};
use std::thread::{self, Thread, ThreadId};
use std::time::{Duration, Instant};

use arc_swap::ArcSwapOption;
use crossbeam_queue::ArrayQueue;

use crate::fanout::Shared;
use crate::packet::{Kind, SharedPacket};

/// The size of one subscriber's ring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RingConfig {
    /// How many packets the ring holds. The ring's storage is allocated once, when it is made.
    pub slots: usize,
    /// How many encoded bytes the ring holds. A packet counts until the receiver has finished
    /// taking it out of the ring. A packet larger than this is still taken when the ring is empty.
    pub bytes: usize,
    /// How far, in the stream's time base, a packet may fall behind the newest packet published
    /// before the receiver skips it as stale.
    pub span: i64,
    /// Whether the stream has video. With video, a run of packets starts at a video keyframe; an
    /// audio-only stream starts a run at any audio packet.
    pub video: bool,
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
    /// one and a half times that many seconds of bytes at the stated bit rate.
    ///
    /// The arithmetic saturates. Rates too large for a ring give a packet count that
    /// [`RingConfig::check`] refuses.
    pub fn for_stream(rates: &StreamRates, seconds: u32) -> Self {
        let s = u64::from(seconds);
        let packets = s.saturating_mul(u64::from(rates.video_packets_per_second) + u64::from(rates.audio_packets_per_second));
        // Bits to bytes and the factor of 1.5 in one step, rounded up: s × bit rate × 3 / 16.
        let bytes = (u128::from(s) * u128::from(rates.bits_per_second) * 3).div_ceil(16);
        Self {
            slots: usize::try_from(packets).unwrap_or(usize::MAX),
            bytes: usize::try_from(bytes).unwrap_or(usize::MAX),
            span: rates.ticks_per_second.saturating_mul(i64::from(seconds)),
            video: rates.video_packets_per_second > 0,
        }
    }

    /// Checks the sizes: every field must be above 0, and `slots` at most [`MAX_SLOTS`].
    pub fn check(&self) -> Result<(), ConfigError> {
        if self.slots == 0 {
            return Err(ConfigError::NoSlots);
        }
        if self.slots > MAX_SLOTS {
            return Err(ConfigError::TooManySlots(self.slots));
        }
        if self.bytes == 0 {
            return Err(ConfigError::NoBytes);
        }
        if self.span <= 0 {
            return Err(ConfigError::NoSpan(self.span));
        }
        Ok(())
    }
}

/// The most packets a ring holds, and the most a [`crate::PacketPool`] makes up front: 65 536,
/// about ten minutes of a 60 fps stream with 47 audio packets a second.
pub const MAX_SLOTS: usize = 65_536;

/// The largest buffer a [`crate::PacketPool`] makes up front for each packet: 16 MiB.
pub const MAX_BUFFER_BYTES: usize = 16 << 20;

/// The most bytes a [`crate::PacketPool`] allocates up front for all its buffers: 256 MiB.
pub const MAX_POOL_BYTES: usize = 256 << 20;

/// The most subscribers a [`crate::Fanout`] has at once: 64.
pub const MAX_SUBSCRIBERS: usize = 64;

/// A ring or pool size that is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigError {
    /// The number of packets is 0.
    NoSlots,
    /// The number of packets is above [`MAX_SLOTS`].
    TooManySlots(usize),
    /// A ring's `bytes` is 0.
    NoBytes,
    /// A ring's `span` is 0 or negative.
    NoSpan(i64),
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
            ConfigError::NoBytes => f.write_str("a ring needs room for at least one byte"),
            ConfigError::NoSpan(s) => write!(f, "a ring's span must be above 0, not {s}"),
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
/// every accepted packet is later delivered, skipped or still queued.
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
    /// Packets and bytes in the ring now.
    pub queued_packets: usize,
    /// See [`Stats::queued_packets`].
    pub queued_bytes: usize,
    /// Whether the producer is dropping packets until the next run start.
    pub resyncing: bool,
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

/// One subscriber's ring and counters, shared by the producer and the receiver.
pub(crate) struct Ring {
    id: SubscriberId,
    cfg: RingConfig,
    q: ArrayQueue<SharedPacket>,
    /// The packets and bytes the ring holds, in one word so that both change together. A packet
    /// counts from before the producer pushes it until after the receiver has taken it out.
    held: AtomicU64,
    state: AtomicU8,
    newest: AtomicI64,
    closed: AtomicBool,
    waiting: AtomicBool,
    waiter: ArcSwapOption<Thread>,
    accepted_packets: AtomicU64,
    accepted_bytes: AtomicU64,
    dropped_packets: AtomicU64,
    dropped_bytes: AtomicU64,
    skipped_packets: AtomicU64,
    skipped_bytes: AtomicU64,
    delivered_packets: AtomicU64,
    delivered_bytes: AtomicU64,
}

impl Ring {
    /// A ring of `cfg`'s size. `cfg` has passed [`RingConfig::check`], so the queue is at most
    /// [`MAX_SLOTS`] entries of two words each, 1 MiB on a 64-bit system. The same amount is
    /// reserved and released first, and a reservation that fails returns
    /// [`ConfigError::OutOfMemory`]. `ArrayQueue::new` itself cannot report a failed allocation.
    pub(crate) fn new(id: SubscriberId, cfg: RingConfig) -> Result<Self, ConfigError> {
        let mut probe: Vec<[usize; 2]> = Vec::new();
        probe.try_reserve_exact(cfg.slots).map_err(|_| ConfigError::OutOfMemory)?;
        drop(probe);
        Ok(Self {
            id,
            cfg,
            q: ArrayQueue::new(cfg.slots),
            held: AtomicU64::new(0),
            state: AtomicU8::new(JOINING),
            newest: AtomicI64::new(i64::MIN),
            closed: AtomicBool::new(false),
            waiting: AtomicBool::new(false),
            waiter: ArcSwapOption::empty(),
            accepted_packets: AtomicU64::new(0),
            accepted_bytes: AtomicU64::new(0),
            dropped_packets: AtomicU64::new(0),
            dropped_bytes: AtomicU64::new(0),
            skipped_packets: AtomicU64::new(0),
            skipped_bytes: AtomicU64::new(0),
            delivered_packets: AtomicU64::new(0),
            delivered_bytes: AtomicU64::new(0),
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
        if self.closed.load(Ordering::Relaxed) {
            return false;
        }
        self.newest.store(p.dts(), Ordering::Relaxed);
        let len = p.len();
        match self.state.load(Ordering::Relaxed) {
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
        // Count the packet before the push, so the receiver never takes away one not yet added.
        self.held.fetch_add(ONE_PACKET + add, Ordering::AcqRel);
        if let Err(back) = self.q.push(p.clone()) {
            drop(back);
            self.held.fetch_sub(ONE_PACKET + add, Ordering::AcqRel);
            return self.overflow(len);
        }
        self.state.store(FLOWING, Ordering::Relaxed);
        self.accepted_packets.fetch_add(1, Ordering::Relaxed);
        self.accepted_bytes.fetch_add(len as u64, Ordering::Relaxed);
        // Pairs with the fence in `Receiver::recv_timeout`: either the receiver sees this packet
        // before it parks, or this sees `waiting` and wakes it.
        fence(Ordering::SeqCst);
        if self.waiting.load(Ordering::SeqCst) && self.waiting.swap(false, Ordering::SeqCst) {
            self.wake();
        }
        true
    }

    fn overflow(&self, len: usize) -> bool {
        self.state.store(RESYNC, Ordering::Relaxed);
        self.count_drop(len);
        false
    }

    fn count_drop(&self, len: usize) {
        self.dropped_packets.fetch_add(1, Ordering::Relaxed);
        self.dropped_bytes.fetch_add(len as u64, Ordering::Relaxed);
    }

    fn wake(&self) {
        if let Some(t) = self.waiter.load().as_ref() {
            t.unpark();
        }
    }

    /// Stops the ring: the producer offers it nothing more, and a waiting receiver wakes.
    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.waiting.store(false, Ordering::SeqCst);
        self.wake();
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    pub(crate) fn stats(&self) -> Stats {
        let (packets, bytes) = split(self.held.load(Ordering::Relaxed));
        Stats {
            accepted_packets: self.accepted_packets.load(Ordering::Relaxed),
            accepted_bytes: self.accepted_bytes.load(Ordering::Relaxed),
            dropped_packets: self.dropped_packets.load(Ordering::Relaxed),
            dropped_bytes: self.dropped_bytes.load(Ordering::Relaxed),
            skipped_packets: self.skipped_packets.load(Ordering::Relaxed),
            skipped_bytes: self.skipped_bytes.load(Ordering::Relaxed),
            delivered_packets: self.delivered_packets.load(Ordering::Relaxed),
            delivered_bytes: self.delivered_bytes.load(Ordering::Relaxed),
            queued_packets: packets as usize,
            queued_bytes: bytes as usize,
            resyncing: self.state.load(Ordering::Relaxed) == RESYNC,
        }
    }
}

/// The receiving end of one subscription, used by one thread at a time, such as a destination's
/// sender thread.
///
/// Dropping it removes the subscription from its fan-out.
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
    /// Once it has returned [`RecvError::Closed`], it returns that from then on, even if a
    /// packet the producer was offering when the subscription was removed reaches the ring.
    pub fn try_recv(&mut self) -> Result<SharedPacket, RecvError> {
        if self.ended {
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
            let newest = self.ring.newest.load(Ordering::Relaxed);
            let stale = newest.saturating_sub(p.dts()) > self.ring.cfg.span;
            if stale {
                self.skipping = true;
            }
            if self.skipping {
                if !stale && self.ring.starts_run(&p) {
                    self.skipping = false;
                } else {
                    self.ring.skipped_packets.fetch_add(1, Ordering::Relaxed);
                    self.ring.skipped_bytes.fetch_add(len as u64, Ordering::Relaxed);
                    continue;
                }
            }
            self.ring.delivered_packets.fetch_add(1, Ordering::Relaxed);
            self.ring.delivered_bytes.fetch_add(len as u64, Ordering::Relaxed);
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
            self.ring.waiting.store(true, Ordering::SeqCst);
            // Pairs with the fence in `Ring::offer`.
            fence(Ordering::SeqCst);
            if !self.ring.q.is_empty() || self.ring.is_closed() {
                self.ring.waiting.store(false, Ordering::SeqCst);
                continue;
            }
            thread::park_timeout(left);
            self.ring.waiting.store(false, Ordering::SeqCst);
        }
    }

    /// Records this thread as the one to wake. It allocates only when the receiver moves to
    /// another thread.
    fn bind(&mut self) {
        let me = thread::current();
        if self.bound != Some(me.id()) {
            self.bound = Some(me.id());
            self.ring.waiter.store(Some(Arc::new(me)));
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

    /// A handle that reads the counters from another thread, such as the user interface's.
    pub fn monitor(&self) -> Monitor {
        Monitor(Arc::clone(&self.ring))
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.shared.remove(self.ring.id);
        self.ring.close();
    }
}

impl fmt::Debug for Receiver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Receiver").field("id", &self.ring.id).field("config", &self.ring.cfg).finish()
    }
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

    /// Whether the receiver is about to park or parked, waiting for the producer to wake it.
    #[cfg(test)]
    pub(crate) fn is_waiting(&self) -> bool {
        self.0.waiting.load(Ordering::SeqCst)
    }
}

impl fmt::Debug for Monitor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Monitor").field("id", &self.0.id).finish()
    }
}
