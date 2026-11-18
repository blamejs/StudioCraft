//! One producer's packets offered to every subscriber.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use arc_swap::ArcSwap;

use crate::packet::SharedPacket;
use crate::ring::{ConfigError, MAX_SUBSCRIBERS, Monitor, Receiver, Ring, RingConfig, SubscriberId};

/// The subscriber list and the end flag, shared by the [`Fanout`], its [`Control`] handles and its
/// receivers.
pub(crate) struct Shared {
    subs: ArcSwap<Vec<Arc<Ring>>>,
    next_id: AtomicU64,
    ended: AtomicBool,
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
        self.subs.rcu(|cur| {
            // Another thread may have added subscribers since the check above.
            full = cur.len() >= MAX_SUBSCRIBERS;
            let mut v = Vec::with_capacity(cur.len() + 1);
            v.extend(cur.iter().cloned());
            if !full {
                v.push(Arc::clone(&ring));
            }
            v
        });
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
        self.subs.rcu(|cur| {
            found = cur.iter().any(|r| r.id() == id);
            cur.iter().filter(|r| r.id() != id).cloned().collect::<Vec<_>>()
        });
        found
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
}

impl Default for Fanout {
    fn default() -> Self {
        Self::new()
    }
}

impl Fanout {
    /// A fan-out with no subscribers.
    pub fn new() -> Self {
        Self { shared: Arc::new(Shared { subs: ArcSwap::from_pointee(Vec::new()), next_id: AtomicU64::new(0), ended: AtomicBool::new(false) }) }
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
        let subs = self.shared.subs.load();
        let mut took = 0;
        for r in subs.iter() {
            if r.offer(p) {
                took += 1;
            }
        }
        took
    }

    /// How many subscribers there are.
    pub fn subscribers(&self) -> usize {
        self.shared.subs.load().len()
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
/// list at its next packet.
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
    /// the ring after that and is not delivered; the producer does not wait for this thread.
    /// Returns whether `id` was subscribed.
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
