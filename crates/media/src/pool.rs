//! Buffers reused from frame to frame. A [`Pool`] hands out items and takes them back when the
//! [`Pooled`] handle is dropped, through a lock-free queue. After the pool has warmed up (as many
//! items made as are in flight at once), taking an item does not allocate.
//!
//! Items are `Default` so a dropped handle can move its item out cheaply: the default of the
//! buffer types used here (`Vec`) does not allocate.

use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crossbeam_queue::ArrayQueue;

/// A pool of reusable `T`.
pub struct Pool<T> {
    free: ArrayQueue<T>,
    made: AtomicUsize,
    make: fn() -> T,
}

impl<T: Default> Pool<T> {
    /// A pool keeping up to `capacity` free items (at least 1), making new ones with `make`.
    pub fn new(capacity: usize, make: fn() -> T) -> Arc<Self> {
        Arc::new(Self { free: ArrayQueue::new(capacity.max(1)), made: AtomicUsize::new(0), make })
    }

    /// A free item, or a new one when none is free.
    pub fn take(self: &Arc<Self>) -> Pooled<T> {
        let item = self.free.pop().unwrap_or_else(|| {
            self.made.fetch_add(1, Ordering::Relaxed);
            (self.make)()
        });
        Pooled { item, pool: Arc::clone(self) }
    }

    /// How many items the pool has made so far.
    pub fn made(&self) -> usize {
        self.made.load(Ordering::Relaxed)
    }

    /// How many items are free now.
    pub fn free(&self) -> usize {
        self.free.len()
    }

    /// How many free items the pool keeps.
    pub fn capacity(&self) -> usize {
        self.free.capacity()
    }

    /// Whether [`Pool::take`] would allocate: every item the pool keeps has been made and none is
    /// free.
    pub fn exhausted(&self) -> bool {
        self.free() == 0 && self.made() >= self.capacity()
    }
}

/// An item on loan from a [`Pool`]; returned when dropped.
pub struct Pooled<T: Default> {
    item: T,
    pool: Arc<Pool<T>>,
}

impl<T: Default> Deref for Pooled<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.item
    }
}

impl<T: Default> DerefMut for Pooled<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.item
    }
}

impl<T: Default> Drop for Pooled<T> {
    fn drop(&mut self) {
        // A full free list means the pool already holds enough; the item is freed instead.
        let _ = self.pool.free.push(std::mem::take(&mut self.item));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_are_reused_after_warm_up() {
        let pool: Arc<Pool<Vec<u8>>> = Pool::new(4, || Vec::with_capacity(1024));
        for _ in 0..1000 {
            let a = pool.take();
            let b = pool.take();
            drop((a, b));
        }
        assert_eq!(pool.made(), 2, "two in flight at once: two made, then reused");
    }

    #[test]
    fn buffers_keep_their_capacity() {
        let pool: Arc<Pool<Vec<u8>>> = Pool::new(1, Vec::new);
        {
            let mut a = pool.take();
            a.resize(4096, 0);
        }
        let b = pool.take();
        assert!(b.capacity() >= 4096);
    }

    #[test]
    fn overflow_is_freed_not_kept() {
        let pool: Arc<Pool<u32>> = Pool::new(1, || 0);
        let items: Vec<_> = (0..3).map(|_| pool.take()).collect();
        drop(items);
        assert_eq!(pool.free(), 1);
    }
}
