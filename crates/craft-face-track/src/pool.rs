//! The threads a tracker runs its models on.
//!
//! A [`Pool`] holds a fixed number of worker threads. Every tracker that shares one pool runs on
//! those threads, so two trackers on two cameras share the same threads instead of starting a
//! pool each: the pool's size is the cap for all of them. A pool of one thread starts no worker
//! and runs every model on the caller's thread.

use std::sync::Arc;

use rayon::prelude::*;

use crate::{Error, Result};

/// The threads a pool starts by default.
pub const DEFAULT_THREADS: usize = 2;
/// The most threads a pool may start.
pub const MAX_THREADS: usize = 8;

/// Values a parallel task should at least cover. Smaller pieces of work run on one thread, where
/// handing them to a worker would cost more than the work.
const MIN_TASK_WORK: usize = 1 << 15;

/// Worker threads shared by trackers. Cloning a pool shares its threads.
#[derive(Clone)]
pub struct Pool {
    workers: Option<Arc<rayon::ThreadPool>>,
    threads: usize,
}

impl std::fmt::Debug for Pool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pool").field("threads", &self.threads).finish()
    }
}

impl Pool {
    /// A pool of `threads` worker threads, 1 to [`MAX_THREADS`]. One thread runs the models on
    /// the caller's thread and starts no worker.
    pub fn new(threads: usize) -> Result<Pool> {
        if !(1..=MAX_THREADS).contains(&threads) {
            return Err(Error::Threads(format!("a tracker pool has 1 to {MAX_THREADS} threads, not {threads}")));
        }
        if threads == 1 {
            return Ok(Pool { workers: None, threads });
        }
        let workers = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|i| format!("face-track-{i}"))
            .build()
            .map_err(|e| Error::Threads(format!("the tracker's threads could not start: {e}")))?;
        Ok(Pool { workers: Some(Arc::new(workers)), threads })
    }

    /// A pool that runs everything on the caller's thread.
    pub fn single() -> Pool {
        Pool { workers: None, threads: 1 }
    }

    /// How many threads the pool runs work on.
    pub fn threads(&self) -> usize {
        self.threads
    }

    /// Run `f` on the pool. [`Pool::chunks`] calls made inside `f` share the pool's workers.
    pub(crate) fn install<R: Send>(&self, f: impl FnOnce() -> R + Send) -> R {
        match &self.workers {
            Some(w) => w.install(f),
            None => f(),
        }
    }

    /// Call `f(i, chunk)` for every `chunk`-long piece `i` of `y` (the last may be shorter),
    /// across the workers when `work` (about the values touched) is large enough. The pieces run on
    /// this pool's workers also when the caller is not inside [`Pool::install`]; small work runs on
    /// the caller's thread.
    pub(crate) fn chunks(&self, y: &mut [f32], chunk: usize, work: usize, f: impl Fn(usize, &mut [f32]) + Send + Sync) {
        if chunk == 0 || y.is_empty() {
            return;
        }
        let pieces = y.len().div_ceil(chunk);
        let Some(workers) = self.workers.as_ref().filter(|_| pieces >= 2 && work >= 2 * MIN_TASK_WORK) else {
            for (i, c) in y.chunks_mut(chunk).enumerate() {
                f(i, c);
            }
            return;
        };
        // Pieces per task, so each task covers at least `MIN_TASK_WORK`.
        let per_piece = (work / pieces).max(1);
        let min_len = MIN_TASK_WORK.div_ceil(per_piece).max(1);
        // Inside `install` this runs at once on the current worker.
        workers.install(|| y.par_chunks_mut(chunk).with_min_len(min_len).enumerate().for_each(|(i, c)| f(i, c)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_counts_outside_one_to_eight_are_errors() {
        assert!(Pool::new(0).is_err());
        assert!(Pool::new(MAX_THREADS + 1).is_err());
        assert_eq!(Pool::new(1).unwrap().threads(), 1);
        assert_eq!(Pool::new(3).unwrap().threads(), 3);
    }

    #[test]
    fn chunks_cover_every_value_once_on_any_pool() {
        for pool in [Pool::single(), Pool::new(2).unwrap(), Pool::new(4).unwrap()] {
            let mut y = vec![0.0f32; 100_003];
            pool.install(|| {
                pool.chunks(&mut y, 1000, 1 << 24, |i, c| {
                    for (k, v) in c.iter_mut().enumerate() {
                        *v += (i * 1000 + k) as f32;
                    }
                })
            });
            assert!(y.iter().enumerate().all(|(k, v)| *v == k as f32), "{pool:?}");
        }
    }

    /// A kernel called outside `install` still runs its pieces on the pool's workers, not on
    /// rayon's global pool.
    #[test]
    fn chunks_run_on_the_pool_without_install() {
        let pool = Pool::new(2).unwrap();
        let seen = std::sync::Mutex::new(std::collections::HashSet::new());
        let mut y = vec![0.0f32; 1 << 18];
        pool.chunks(&mut y, 1 << 12, 1 << 24, |_, c| {
            seen.lock().unwrap().insert(std::thread::current().name().map(str::to_owned));
            c.fill(1.0);
        });
        assert!(y.iter().all(|v| *v == 1.0));
        let seen = seen.into_inner().unwrap();
        assert!(seen.iter().all(|n| n.as_deref().is_some_and(|n| n.starts_with("face-track-"))), "{seen:?}");
    }

    /// Two trackers on two threads share one pool's workers: both finish, and no more threads
    /// than the pool's run their pieces.
    #[test]
    fn two_callers_share_one_pool() {
        let pool = Pool::new(2).unwrap();
        let seen = std::sync::Mutex::new(std::collections::HashSet::new());
        std::thread::scope(|s| {
            for _ in 0..2 {
                s.spawn(|| {
                    let mut y = vec![0.0f32; 1 << 18];
                    pool.install(|| {
                        pool.chunks(&mut y, 1 << 12, 1 << 24, |_, c| {
                            seen.lock().unwrap().insert(std::thread::current().name().map(str::to_owned));
                            c.fill(1.0);
                        })
                    });
                    assert!(y.iter().all(|v| *v == 1.0));
                });
            }
        });
        let seen = seen.into_inner().unwrap();
        assert!(seen.iter().all(|n| n.as_deref().is_some_and(|n| n.starts_with("face-track-"))), "{seen:?}");
        assert!(seen.len() <= 2, "{seen:?}");
    }
}
