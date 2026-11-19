//! One producer's packets offered to every subscriber.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use arc_swap::ArcSwap;
use crossbeam_utils::CachePadded;

use crate::packet::{Info, PacketPool, ShareError, SharedPacket};
use crate::ring::{ConfigError, MAX_SUBSCRIBERS, Monitor, Receiver, Ring, RingConfig, SubscriberId};

type List = Arc<Vec<Arc<Ring>>>;

/// The subscriber list and the end flag, shared by the [`Fanout`], its [`Control`] handles and its
/// receivers.
pub(crate) struct Shared {
    subs: ArcSwap<Vec<Arc<Ring>>>,
    /// The `dts` of the newest packet published, read by every receiver's stale check.
    newest: CachePadded<AtomicI64>,
    next_id: AtomicU64,
    ended: AtomicBool,
    /// Subscriber lists replaced while the producer may still have been reading them. Each is
    /// freed on a thread that changes the list, once nothing else holds it, so the producer never
    /// frees a list or a ring.
    retired: Mutex<Vec<List>>,
}

impl Shared {
    fn subscribe(self: &Arc<Self>, cfg: RingConfig) -> Result<Receiver, ConfigError> {
        cfg.check()?;
        if self.subs.load().len() >= MAX_SUBSCRIBERS {
            return Err(ConfigError::TooManySubscribers);
        }
        let id = SubscriberId(self.next_id.fetch_add(1, Ordering::Relaxed) + 1);
        let ring = Arc::new(Ring::new(id, cfg)?);
        let mut full = false;
        let old = self.subs.rcu(|cur| {
            // Another thread may have added subscribers since the check above.
            full = cur.len() >= MAX_SUBSCRIBERS;
            let mut v = Vec::with_capacity(cur.len() + 1);
            v.extend(cur.iter().cloned());
            if !full {
                v.push(Arc::clone(&ring));
            }
            v
        });
        self.retire(old);
        if full {
            return Err(ConfigError::TooManySubscribers);
        }
        // Pairs with the store in `Fanout::drop`: a ring added after the fan-out read the list to
        // close it sees `ended` here instead.
        if self.ended.load(Ordering::SeqCst) {
            ring.close();
        }
        Ok(Receiver::new(ring, Arc::clone(self)))
    }

    pub(crate) fn remove(&self, id: SubscriberId) -> bool {
        let mut found = false;
        let old = self.subs.rcu(|cur| {
            found = cur.iter().any(|r| r.id() == id);
            cur.iter().filter(|r| r.id() != id).cloned().collect::<Vec<_>>()
        });
        self.retire(old);
        found
    }

    /// Keeps `old`, a list just replaced, until nothing else holds it, and frees the lists retired
    /// earlier that nothing holds any more. A list still held when the fan-out ends is freed with
    /// the last of the fan-out, its controls and its receivers.
    fn retire(&self, old: List) {
        let mut retired = self.retired.lock().unwrap_or_else(PoisonError::into_inner);
        // A list is no longer the current one once it is here, so no thread can take a new
        // reference to it; one held only here is free to drop.
        retired.retain(|l| Arc::strong_count(l) > 1);
        if Arc::strong_count(&old) > 1 {
            retired.push(old);
        }
    }

    /// The `dts` of the newest packet published.
    pub(crate) fn newest(&self) -> i64 {
        self.newest.load(Ordering::Relaxed)
    }
}

/// Sends each packet a producer shares to every subscriber's ring.
///
/// The producer's thread owns the `Fanout` and calls [`Fanout::publish`] for each packet. Other
/// threads add and remove subscribers through a [`Control`]. Publishing reads the subscriber list
/// without a lock, then, for each subscriber, adds one to the packet's reference count and pushes
/// it into the subscriber's ring without blocking. A subscriber whose ring is full loses packets;
/// the producer and the other subscribers never wait for it.
///
/// Dropping the `Fanout` ends every subscription: receivers deliver what their rings hold and then
/// report [`crate::RecvError::Closed`].
pub struct Fanout {
    shared: Arc<Shared>,
    /// The pool of the packets published, 0 before the first packet.
    pool: usize,
    /// Whether packets from more than one pool have been published.
    mixed: bool,
}

impl Default for Fanout {
    fn default() -> Self {
        Self::new()
    }
}

impl Fanout {
    /// A fan-out with no subscribers.
    pub fn new() -> Self {
        Self {
            shared: Arc::new(Shared {
                subs: ArcSwap::from_pointee(Vec::new()),
                newest: CachePadded::new(AtomicI64::new(i64::MIN)),
                next_id: AtomicU64::new(0),
                ended: AtomicBool::new(false),
                retired: Mutex::new(Vec::new()),
            }),
            pool: 0,
            mixed: false,
        }
    }

    /// A handle for adding and removing subscribers from other threads.
    pub fn control(&self) -> Control {
        Control { shared: Arc::clone(&self.shared) }
    }

    /// Adds a subscriber with a ring of `cfg`'s size. Its first packet is the next run start
    /// published: a video keyframe, or in an audio-only stream an audio packet. A fan-out has at
    /// most [`MAX_SUBSCRIBERS`] subscribers.
    pub fn subscribe(&self, cfg: RingConfig) -> Result<Receiver, ConfigError> {
        self.shared.subscribe(cfg)
    }

    /// Offers `p` to every subscriber and returns how many took it.
    ///
    /// When a ring is full, the producer drops the packet for that subscriber and every packet
    /// after it, audio and data included, up to the next run start. What the ring holds is then an
    /// unbroken run, so audio and video stay aligned and the next packet the sender sees after the
    /// gap is a keyframe.
    pub fn publish(&mut self, p: &SharedPacket) -> usize {
        let pool = p.pool_id();
        if pool != self.pool {
            self.mixed |= self.pool != 0;
            self.pool = pool;
        }
        self.shared.newest.store(p.dts(), Ordering::Relaxed);
        let subs = self.shared.subs.load();
        let mut took = 0;
        for r in subs.iter() {
            if r.offer(p) {
                took += 1;
            }
        }
        took
    }

    /// [`PacketPool::share`] with `pool`, freeing pool space when the pool is full.
    ///
    /// Subscribers' rings keep pool packets until their receivers take them. When the pool is full,
    /// this empties a ring, counting its packets as discarded, makes that subscriber drop packets
    /// up to the next run start, as after an overflow, and tries again, one ring at a time. It
    /// empties the rings of subscribers that have fallen behind first: a ring that is full by
    /// packets, or whose producer side is dropping packets up to a run start. Among those, and then
    /// among the others, it takes the ring that holds the most bytes. A stalled subscriber
    /// therefore loses its own packets and never keeps the others from getting theirs. Packets a
    /// receiver has already taken out of its ring stay with it.
    ///
    /// Rings are emptied only while every packet published so far came from `pool`. Once packets
    /// from another pool have been published, a full pool returns [`ShareError::PoolFull`] as
    /// [`PacketPool::share`] does, because emptying a ring might free nothing in `pool`. When an
    /// emptied ring of a subscriber that keeps up returns no packet to the pool, because the caller
    /// still holds clones of its packets, no further ring is emptied: the pool is asked once more,
    /// in case another thread dropped those clones meanwhile, and the call returns
    /// [`ShareError::PoolFull`] if it is still full.
    pub fn share(&mut self, pool: &mut PacketPool, data: &mut Vec<u8>, info: Info) -> Result<SharedPacket, ShareError> {
        self.relieved(pool, |pool| pool.share(data, info))
    }

    /// [`PacketPool::share_copy`] with `pool`, freeing pool space when the pool is full, as
    /// [`Fanout::share`] does.
    pub fn share_copy(&mut self, pool: &mut PacketPool, data: &[u8], info: Info) -> Result<SharedPacket, ShareError> {
        self.relieved(pool, |pool| pool.share_copy(data, info))
    }

    /// Runs `share` on `pool`, emptying rings while it reports the pool full, as [`Fanout::share`]
    /// describes.
    fn relieved(
        &mut self,
        pool: &mut PacketPool,
        mut share: impl FnMut(&mut PacketPool) -> Result<SharedPacket, ShareError>,
    ) -> Result<SharedPacket, ShareError> {
        let mut last_try = false;
        let mut emptied = false;
        loop {
            match share(pool) {
                Err(ShareError::PoolFull) if !last_try => match self.relieve(pool.id()) {
                    // Another stalled ring may hold the last clones of this one's packets, so a
                    // stalled ring that returned nothing does not end the search.
                    Some((returned, behind)) => {
                        emptied = true;
                        last_try = returned == 0 && !behind;
                    }
                    // Every ring is empty. Ask once more, in case another thread dropped clones.
                    None if emptied => last_try = true,
                    None => return Err(ShareError::PoolFull),
                },
                other => return other,
            }
        }
    }

    /// Empties one ring, when every packet published came from pool `pool`: of the rings that
    /// hold packets, the one holding the most bytes among those whose subscriber has fallen
    /// behind, or else among all. Returns how many of its packets went back to the pool and
    /// whether its subscriber had fallen behind, or `None` when no ring was emptied.
    fn relieve(&mut self, pool: usize) -> Option<(usize, bool)> {
        if self.mixed || self.pool != pool {
            return None;
        }
        let subs = self.shared.subs.load();
        // Only the producer adds packets, so a ring emptied here is not picked again, and the
        // search ends.
        let holding = || subs.iter().filter(|r| r.has_packets());
        let behind = holding().filter(|r| r.is_backed_up()).max_by_key(|r| r.queued_bytes());
        match behind {
            Some(r) => Some((r.flush(), true)),
            None => holding().max_by_key(|r| r.queued_bytes()).map(|r| (r.flush(), false)),
        }
    }

    /// How many subscribers there are.
    pub fn subscribers(&self) -> usize {
        self.shared.subs.load().len()
    }

    /// The current subscriber list, held the way `publish` holds it.
    #[cfg(test)]
    pub(crate) fn hold_list(&self) -> arc_swap::Guard<Arc<Vec<Arc<Ring>>>> {
        self.shared.subs.load()
    }
}

impl Drop for Fanout {
    fn drop(&mut self) {
        self.shared.ended.store(true, Ordering::SeqCst);
        for r in self.shared.subs.load().iter() {
            r.close();
        }
    }
}

impl fmt::Debug for Fanout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Fanout").field("subscribers", &self.subscribers()).finish()
    }
}

/// Adds and removes a [`Fanout`]'s subscribers from any thread.
///
/// Each change copies the subscriber list once, off the producer's path; the producer sees the new
/// list at its next packet. The replaced list, and a ring whose receiver is gone, are freed on a
/// thread that changes the list, never on the producer's.
#[derive(Clone)]
pub struct Control {
    shared: Arc<Shared>,
}

impl Control {
    /// Adds a subscriber; see [`Fanout::subscribe`]. After the fan-out is dropped, the new
    /// receiver reports [`crate::RecvError::Closed`].
    pub fn subscribe(&self, cfg: RingConfig) -> Result<Receiver, ConfigError> {
        self.shared.subscribe(cfg)
    }

    /// Removes subscriber `id`. Its receiver delivers what its ring holds and then reports
    /// [`crate::RecvError::Closed`]. A packet the producer is offering at that moment may reach
    /// the ring after that; the receiver's next call releases it without delivering it. The
    /// producer does not wait for this thread. Returns whether `id` was subscribed.
    pub fn unsubscribe(&self, id: SubscriberId) -> bool {
        let list = self.shared.subs.load();
        let ring = list.iter().find(|r| r.id() == id).cloned();
        drop(list);
        let found = self.shared.remove(id);
        if let Some(r) = ring {
            r.close();
        }
        found
    }

    /// How many subscribers there are.
    pub fn subscribers(&self) -> usize {
        self.shared.subs.load().len()
    }

    /// Counter handles for every subscriber, in the order they subscribed.
    pub fn monitors(&self) -> Vec<Monitor> {
        self.shared.subs.load().iter().map(crate::ring::monitor).collect()
    }

    /// Whether the fan-out has been dropped.
    pub fn is_ended(&self) -> bool {
        self.shared.ended.load(Ordering::Acquire)
    }
}

impl fmt::Debug for Control {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Control").field("subscribers", &self.subscribers()).finish()
    }
}
