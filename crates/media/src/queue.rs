//! Bounded lock-free queues between pipeline threads. Neither side ever blocks: a producer that
//! finds the queue full pushes the oldest item out and the drop is counted, so a stalled consumer
//! costs frames, not a frozen capture thread.

use std::sync::atomic::{AtomicU64, Ordering};

use crossbeam_queue::ArrayQueue;

/// A first-in first-out queue that drops its oldest item when full.
pub struct Fifo<T> {
    q: ArrayQueue<T>,
    dropped: AtomicU64,
}

impl<T> Fifo<T> {
    /// A queue of `capacity` items (at least 1).
    pub fn new(capacity: usize) -> Self {
        Self { q: ArrayQueue::new(capacity.max(1)), dropped: AtomicU64::new(0) }
    }

    /// Add `item`; when full, the oldest item is dropped and counted.
    pub fn push(&self, item: T) {
        if self.q.force_push(item).is_some() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn pop(&self) -> Option<T> {
        self.q.pop()
    }

    /// Take back the oldest item unread, counted as dropped, for the writer to reuse.
    pub fn reclaim(&self) -> Option<T> {
        let item = self.q.pop();
        if item.is_some() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        item
    }

    pub fn len(&self) -> usize {
        self.q.len()
    }

    pub fn is_empty(&self) -> bool {
        self.q.is_empty()
    }

    pub fn capacity(&self) -> usize {
        self.q.capacity()
    }

    /// Items dropped because the queue was full.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// A mailbox holding the newest item: the reader takes the latest and older ones are dropped.
/// Video sources publish frames into one; the compositor takes the newest each frame.
pub struct Latest<T> {
    q: ArrayQueue<T>,
    replaced: AtomicU64,
}

impl<T> Default for Latest<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Latest<T> {
    pub fn new() -> Self {
        Self { q: ArrayQueue::new(2), replaced: AtomicU64::new(0) }
    }

    /// Publish `item`; an item the reader has not taken yet may be dropped.
    pub fn publish(&self, item: T) {
        if self.q.force_push(item).is_some() {
            self.replaced.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The newest item published since the last call, if any.
    pub fn take(&self) -> Option<T> {
        let mut last = None;
        while let Some(item) = self.q.pop() {
            if last.replace(item).is_some() {
                self.replaced.fetch_add(1, Ordering::Relaxed);
            }
        }
        last
    }

    /// Items published but never taken.
    pub fn replaced(&self) -> u64 {
        self.replaced.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn fifo_drops_the_oldest_when_full() {
        let q = Fifo::new(2);
        q.push(1);
        q.push(2);
        q.push(3);
        assert_eq!(q.dropped(), 1);
        assert_eq!(q.pop(), Some(2));
        assert_eq!(q.pop(), Some(3));
        assert_eq!(q.pop(), None);
    }

    #[test]
    fn latest_keeps_only_the_newest() {
        let m = Latest::new();
        assert_eq!(m.take(), None::<u32>);
        for i in 0..10 {
            m.publish(i);
        }
        assert_eq!(m.take(), Some(9));
        assert_eq!(m.take(), None);
        assert_eq!(m.replaced(), 9);
    }

    #[test]
    fn threads_exchange_without_locks() {
        let q = Arc::new(Fifo::new(64));
        let producer = {
            let q = Arc::clone(&q);
            std::thread::spawn(move || {
                for i in 0..10_000u32 {
                    q.push(i);
                }
            })
        };
        let mut seen = 0u64;
        let mut last = None;
        while !producer.is_finished() || !q.is_empty() {
            if let Some(v) = q.pop() {
                assert!(last.is_none_or(|l| v > l), "order kept");
                last = Some(v);
                seen += 1;
            }
        }
        let _ = producer.join();
        assert_eq!(seen + q.dropped(), 10_000);
    }
}
