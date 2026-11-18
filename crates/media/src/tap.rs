//! Taps: copies of one mixer input, or of the program mix, taken block by block for a writer on
//! another thread.
//!
//! A [`TapSlot`] has its own pool of blocks, made when the slot is made, and a bounded queue. The
//! mixer claims a block, fills it in the same pass that mixes the input, and pushes it; the reader
//! drops each block after copying it, which returns it to the pool. Claiming never allocates and
//! never waits: when every block is out, the oldest block still in the queue is taken back, so a
//! reader that stalls loses its oldest audio and the mixer and the outputs carry on. The pool is
//! the slot's own, so a stalled tap never takes blocks from an output.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::audio::{AudioBlock, AudioPool, BLOCK_SAMPLES, PooledBlock};
use crate::pool::Pool;
use crate::queue::Fifo;

/// Blocks a tap's queue holds before the oldest are dropped: 683 ms of audio.
pub const TAP_QUEUE_BLOCKS: usize = 32;
/// Blocks a tap's pool makes: the queue, one block being filled and one being read.
pub const TAP_POOL_BLOCKS: usize = TAP_QUEUE_BLOCKS + 2;

/// One input's tap: its pool and its queue.
pub struct TapSlot {
    pool: Arc<AudioPool>,
    queue: Fifo<PooledBlock>,
    /// Blocks the mixer could not claim at all (every block was being read).
    skipped: AtomicU64,
}

impl Default for TapSlot {
    fn default() -> Self {
        Self::new()
    }
}

impl TapSlot {
    /// A slot with [`TAP_POOL_BLOCKS`] blocks made now and a queue of [`TAP_QUEUE_BLOCKS`].
    pub fn new() -> TapSlot {
        TapSlot {
            pool: Pool::with_capacity(TAP_POOL_BLOCKS, || AudioBlock { samples: vec![0.0; BLOCK_SAMPLES], pts: craft_time::Tick(0) }),
            queue: Fifo::new(TAP_QUEUE_BLOCKS),
            skipped: AtomicU64::new(0),
        }
    }

    /// A full-length block to fill, without allocating: a free block, else the oldest block still
    /// in the queue (counted as dropped). `None` when every block is being read; that is counted
    /// in [`TapSlot::skipped`].
    pub fn claim(&self) -> Option<PooledBlock> {
        // The pool made all its blocks up front. A block the reader frees between the two looks
        // at the pool is still found.
        let block = self.pool.try_take().or_else(|| self.queue.reclaim()).or_else(|| self.pool.try_take());
        match block {
            Some(mut b) => {
                if b.samples.len() != BLOCK_SAMPLES {
                    b.clear();
                }
                Some(b)
            }
            None => {
                self.skipped.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    /// Queue a filled block for the reader.
    pub fn push(&self, block: PooledBlock) {
        self.queue.push(block);
    }

    /// The oldest block waiting, for the reader.
    pub fn pop(&self) -> Option<PooledBlock> {
        self.queue.pop()
    }

    /// Blocks dropped from the queue because the reader fell behind.
    pub fn dropped(&self) -> u64 {
        self.queue.dropped()
    }

    /// Blocks the mixer could not claim.
    pub fn skipped(&self) -> u64 {
        self.skipped.load(Ordering::Relaxed)
    }

    /// Blocks the slot's pool has made.
    pub fn made(&self) -> usize {
        self.pool.made()
    }

    /// Blocks waiting in the queue.
    pub fn queued(&self) -> usize {
        self.queue.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reader_that_stalls_loses_its_oldest_blocks_and_nothing_is_allocated() {
        let tap = TapSlot::new();
        for n in 0..1000 {
            let Some(mut b) = tap.claim() else { panic!("block {n}: nothing to claim") };
            b.pts = craft_time::Tick(n);
            tap.push(b);
        }
        assert_eq!(tap.made(), TAP_POOL_BLOCKS, "every block was made with the slot");
        assert_eq!(tap.queued(), TAP_QUEUE_BLOCKS);
        assert_eq!(tap.skipped(), 0);
        assert_eq!(tap.dropped(), 1000 - TAP_QUEUE_BLOCKS as u64);
        assert_eq!(tap.pop().map(|b| b.pts), Some(craft_time::Tick(1000 - TAP_QUEUE_BLOCKS as i64)), "the newest blocks stay");
    }

    #[test]
    fn a_reader_holding_every_block_makes_the_mixer_skip_without_allocating() {
        let tap = TapSlot::new();
        let held: Vec<PooledBlock> = (0..TAP_POOL_BLOCKS).filter_map(|_| tap.claim()).collect();
        assert_eq!(held.len(), TAP_POOL_BLOCKS);
        assert!(tap.claim().is_none());
        assert_eq!((tap.skipped(), tap.made()), (1, TAP_POOL_BLOCKS));
        drop(held);
        assert!(tap.claim().is_some(), "blocks the reader drops come back");
    }
}
