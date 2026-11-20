//! Markers set while a take runs. Any thread adds one without waiting: it goes into a bounded
//! queue, the disk thread writes it to the take's journal with the next sync, and the take's
//! session lists it as a marker at its place on the timeline.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use crossbeam_queue::ArrayQueue;
use studiocraft_media::Tick;

use crate::Status;
use crate::collect::FRAME_TICKS;

/// Markers waiting for the disk thread at most. A marker added while the queue is full is not
/// kept, and [`TakeMarkers::dropped`] counts it.
pub const QUEUED: usize = 1024;

/// Characters kept of a marker's name.
pub const MAX_NAME_CHARS: usize = 256;

/// A marker: its take frame and its name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Marker {
    pub at: u64,
    pub name: String,
}

/// The queue the disk thread reads markers from.
pub(crate) type MarkerQueue = Arc<MarkerLog>;

/// Markers on their way to the disk thread. Once the disk thread closes the log for its last
/// read, an add either finished its push before that read or returns false.
pub struct MarkerLog {
    queue: ArrayQueue<Marker>,
    /// Adds between their check of `closed` and the end of their push.
    adding: AtomicUsize,
    closed: AtomicBool,
}

impl MarkerLog {
    pub(crate) fn new(capacity: usize) -> MarkerLog {
        MarkerLog { queue: ArrayQueue::new(capacity), adding: AtomicUsize::new(0), closed: AtomicBool::new(false) }
    }

    /// Push `marker` unless the log is closed or full. False when it was not kept.
    fn push(&self, marker: Marker) -> bool {
        // SeqCst pairs this increment and load with `close`'s store and load: either `close`
        // sees this add, and waits for it, or this add sees the log closed.
        self.adding.fetch_add(1, Ordering::SeqCst);
        let kept = !self.closed.load(Ordering::SeqCst) && self.queue.push(marker).is_ok();
        self.adding.fetch_sub(1, Ordering::SeqCst);
        kept
    }

    pub(crate) fn pop(&self) -> Option<Marker> {
        self.queue.pop()
    }

    /// Take no more markers, and wait for an add that is pushing to finish. The wait is as long
    /// as one queue push.
    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        while self.adding.load(Ordering::SeqCst) > 0 {
            std::thread::yield_now();
        }
    }

    #[cfg(test)]
    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
}

/// The marker log of one take. Cloning it gives another handle to the same log.
#[derive(Clone)]
pub struct TakeMarkers {
    queue: MarkerQueue,
    /// Take frame 0 on the studio's clock, when the take has a start time.
    origin: Option<Tick>,
    status: Arc<Status>,
    kept: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
}

impl TakeMarkers {
    pub(crate) fn new(queue: MarkerQueue, origin: Option<Tick>, status: Arc<Status>) -> TakeMarkers {
        TakeMarkers { queue, origin, status, kept: Arc::new(AtomicU64::new(0)), dropped: Arc::new(AtomicU64::new(0)) }
    }

    /// The take frame of `at` on the studio's clock: frames since the take's start, or for a take
    /// without a start time the frames its collect thread has reached.
    pub fn frame(&self, at: Tick) -> u64 {
        match self.origin {
            Some(o) => u64::try_from(at.0.saturating_sub(o.0).max(0) / FRAME_TICKS).unwrap_or(0),
            None => self.status.frames.load(Ordering::Relaxed),
        }
    }

    /// Add a marker named `name` at `at` on the studio's clock. False when the queue is full or
    /// the take was asked to stop; the marker is then not kept.
    pub fn add(&self, at: Tick, name: &str) -> bool {
        self.add_frame(self.frame(at), name)
    }

    /// Add a marker named `name` at take frame `at`. False as for [`TakeMarkers::add`].
    pub fn add_frame(&self, at: u64, name: &str) -> bool {
        // Once the take is asked to stop, the disk thread may have journaled its last markers.
        let ended = matches!(self.status.state(), Some(crate::State::Finishing | crate::State::Done | crate::State::Stopped));
        let name: String = name.chars().take(MAX_NAME_CHARS).collect();
        if ended || !self.queue.push(Marker { at, name }) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        self.kept.fetch_add(1, Ordering::Relaxed);
        true
    }

    /// Markers the take kept.
    pub fn kept(&self) -> u64 {
        self.kept.load(Ordering::Relaxed)
    }

    /// Markers that were not kept because the queue was full or the take was asked to stop.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_are_placed_from_the_start_time_and_a_full_queue_counts_what_it_drops() {
        let status = Arc::new(Status::default());
        status.state.store(crate::State::Recording as u8, Ordering::Release);
        let queue: MarkerQueue = Arc::new(MarkerLog::new(2));
        let m = TakeMarkers::new(Arc::clone(&queue), Some(Tick(1_000)), Arc::clone(&status));
        assert_eq!(m.frame(Tick(1_000 + 480 * FRAME_TICKS)), 480);
        assert_eq!(m.frame(Tick(0)), 0, "a time before the start is frame 0");
        assert!(m.add(Tick(1_000 + 48_000 * FRAME_TICKS), "Goal"));
        assert!(m.add_frame(7, &"x".repeat(MAX_NAME_CHARS + 10)));
        assert!(!m.add_frame(9, "No room"), "the queue holds two");
        assert_eq!((m.kept(), m.dropped()), (2, 1));
        assert_eq!(queue.pop(), Some(Marker { at: 48_000, name: "Goal".into() }));
        assert_eq!(queue.pop().map(|m| m.name.chars().count()), Some(MAX_NAME_CHARS));
        status.state.store(crate::State::Finishing as u8, Ordering::Release);
        assert!(!m.add_frame(1, "Late"), "a take asked to stop keeps no marker");
        status.state.store(crate::State::Done as u8, Ordering::Release);
        assert!(!m.add_frame(1, "Later"), "a take that ended keeps no marker");
        assert_eq!(m.dropped(), 3);
        // Without a start time the marker goes where the take has reached.
        status.frames.store(960, Ordering::Relaxed);
        let free = TakeMarkers::new(queue, None, status);
        assert_eq!(free.frame(Tick(123_456_789)), 960);
    }

    #[test]
    fn closing_the_log_waits_for_an_add_that_is_pushing_and_then_takes_no_more() {
        let log = Arc::new(MarkerLog::new(4));
        // An add that has found the log open and not pushed yet.
        log.adding.fetch_add(1, Ordering::SeqCst);
        let closed = Arc::new(AtomicBool::new(false));
        let closer = {
            let (log, closed) = (Arc::clone(&log), Arc::clone(&closed));
            std::thread::spawn(move || {
                log.close();
                closed.store(true, Ordering::SeqCst);
                log.pop()
            })
        };
        while !log.is_closed() {
            std::thread::yield_now();
        }
        for _ in 0..10_000 {
            std::thread::yield_now();
        }
        assert!(!closed.load(Ordering::SeqCst), "the close waits for the add");
        let late = Marker { at: 5, name: "Late".into() };
        assert!(log.queue.push(late.clone()).is_ok());
        log.adding.fetch_sub(1, Ordering::SeqCst);
        assert_eq!(closer.join().ok().flatten(), Some(late), "the last read takes the marker that add pushed");
        assert!(!log.push(Marker { at: 6, name: "After".into() }), "a closed log takes no marker");
        assert_eq!(log.pop(), None);
    }
}
