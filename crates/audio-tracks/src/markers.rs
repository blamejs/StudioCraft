//! Markers set while a take runs. Any thread adds one without waiting: it goes into a bounded
//! queue, the disk thread writes it to the take's journal with the next sync, and the take's
//! session lists it as a marker at its place on the timeline.
//!
//! A marker is renamed the same way while the take runs: the rename goes into the same queue, and
//! the journal's rename line gives the marker its new name in the session the take writes at its
//! end and in the one repair writes. Once the take has ended, a rename changes the marker's name in
//! the take's session file, which is written again in one step.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use crossbeam_queue::ArrayQueue;
use studiocraft_media::Tick;

use crate::collect::FRAME_TICKS;
use crate::{State, Status};

/// Markers and renames waiting for the disk thread at most. A marker added while the queue is
/// full is not kept, and [`TakeMarkers::dropped`] counts it.
pub const QUEUED: usize = 1024;

/// Characters kept of a marker's name.
pub const MAX_NAME_CHARS: usize = 256;

/// A marker: its number in the take (from 1), its take frame and its name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Marker {
    pub number: u64,
    pub at: u64,
    pub name: String,
}

/// What the disk thread reads from the marker log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Entry {
    /// A marker to journal.
    Add(Marker),
    /// The new name of the take's marker `number`.
    Rename { number: u64, name: String },
}

/// The queue the disk thread reads markers from.
pub(crate) type MarkerQueue = Arc<MarkerLog>;

/// Markers on their way to the disk thread. Once the disk thread closes the log for its last
/// read, an add either finished its push before that read or returns false.
pub struct MarkerLog {
    queue: ArrayQueue<Entry>,
    /// Adds between their check of `closed` and the end of their push.
    adding: AtomicUsize,
    closed: AtomicBool,
}

impl MarkerLog {
    pub(crate) fn new(capacity: usize) -> MarkerLog {
        MarkerLog { queue: ArrayQueue::new(capacity), adding: AtomicUsize::new(0), closed: AtomicBool::new(false) }
    }

    /// Push `entry` unless the log is closed or full. False when it was not kept.
    fn push(&self, entry: Entry) -> bool {
        // SeqCst pairs this increment and load with `close`'s store and load: either `close`
        // sees this add, and waits for it, or this add sees the log closed.
        self.adding.fetch_add(1, Ordering::SeqCst);
        let kept = !self.closed.load(Ordering::SeqCst) && self.queue.push(entry).is_ok();
        self.adding.fetch_sub(1, Ordering::SeqCst);
        kept
    }

    pub(crate) fn pop(&self) -> Option<Entry> {
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

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
}

/// Why [`TakeMarkers::rename`] did not rename a marker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RenameError {
    /// The take runs and the queue is full.
    Full,
    /// The take is finishing its files; the marker can be renamed once it has ended.
    Finishing,
    /// The take ended without a session file.
    NoSession,
    /// The session file could not be written again, for this reason.
    Session(String),
}

impl std::fmt::Display for RenameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RenameError::Full => write!(f, "{QUEUED} markers and renames are waiting to be written"),
            RenameError::Finishing => write!(f, "the take is finishing its files; rename the marker once it has ended"),
            RenameError::NoSession => write!(f, "the take ended without a session file"),
            RenameError::Session(e) => write!(f, "the session file could not be written again: {e}"),
        }
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
    /// Numbers given to markers so far, kept or not.
    numbered: Arc<AtomicU64>,
}

impl TakeMarkers {
    pub(crate) fn new(queue: MarkerQueue, origin: Option<Tick>, status: Arc<Status>) -> TakeMarkers {
        TakeMarkers {
            queue,
            origin,
            status,
            kept: Arc::new(AtomicU64::new(0)),
            dropped: Arc::new(AtomicU64::new(0)),
            numbered: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Whether the take was asked to stop: the disk thread may have journaled its last markers.
    fn ended(&self) -> bool {
        matches!(self.status.state(), Some(State::Finishing | State::Done | State::Stopped))
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
        self.add_numbered(at, name).is_some()
    }

    /// Add a marker named `name` at take frame `at`, as [`TakeMarkers::add_frame`] does. Returns
    /// its number in the take, which [`TakeMarkers::rename`] takes, or `None` when it was not
    /// kept.
    pub fn add_numbered(&self, at: u64, name: &str) -> Option<u64> {
        let name: String = name.chars().take(MAX_NAME_CHARS).collect();
        let number = self.numbered.fetch_add(1, Ordering::Relaxed).saturating_add(1);
        if self.ended() || !self.queue.push(Entry::Add(Marker { number, at, name })) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        self.kept.fetch_add(1, Ordering::Relaxed);
        Some(number)
    }

    /// Rename the take's marker `number`, now named `old`, to `name` (its first
    /// [`MAX_NAME_CHARS`] characters). While the take runs, the rename goes to the journal with
    /// the next sync, and the take's session gets the new name; `old` is not checked, and the
    /// last rename of the marker wins. Once the take has ended with a
    /// session file, the marker is renamed in that file, which is written again in one step
    /// ([`crate::session::rename_take_marker`]). A file that no longer has the marker named `old`
    /// is an error.
    pub fn rename(&self, number: u64, old: &str, name: &str) -> Result<(), RenameError> {
        let name: String = name.chars().take(MAX_NAME_CHARS).collect();
        if !self.ended() {
            if self.queue.push(Entry::Rename { number, name: name.clone() }) {
                return Ok(());
            }
            // A closed log means the take is ending even when its state does not show it yet.
            if !self.queue.is_closed() && !self.ended() {
                return Err(RenameError::Full);
            }
        }
        if !matches!(self.status.state(), Some(State::Done | State::Stopped)) {
            return Err(RenameError::Finishing);
        }
        let session = self.status.session().ok_or(RenameError::NoSession)?;
        let dir = session.parent().map(|p| p.join(crate::session::FILES_FOLDER)).ok_or(RenameError::NoSession)?;
        crate::session::rename_take_marker(&dir, session, number, old, &name).map_err(|e| RenameError::Session(e.to_string()))
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
        assert_eq!(queue.pop(), Some(Entry::Add(Marker { number: 1, at: 48_000, name: "Goal".into() })));
        let long = match queue.pop() {
            Some(Entry::Add(m)) => m,
            other => panic!("not a marker: {other:?}"),
        };
        assert_eq!((long.number, long.name.chars().count()), (2, MAX_NAME_CHARS));
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
    fn a_rename_goes_to_the_queue_while_the_take_runs() {
        let status = Arc::new(Status::default());
        status.state.store(State::Recording as u8, Ordering::Release);
        let queue: MarkerQueue = Arc::new(MarkerLog::new(3));
        let m = TakeMarkers::new(Arc::clone(&queue), Some(Tick(0)), Arc::clone(&status));
        assert!(m.add_frame(1, "x"));
        let number = m.add_numbered(10, "Goal").unwrap();
        assert_eq!(number, 2, "numbers count every marker of the take");
        m.rename(number, "Goal",&"y".repeat(MAX_NAME_CHARS + 1)).unwrap();
        assert_eq!(m.rename(number, "Goal","Late"), Err(RenameError::Full), "the queue holds three");
        assert!(matches!(queue.pop(), Some(Entry::Add(_))));
        assert!(matches!(queue.pop(), Some(Entry::Add(_))));
        assert_eq!(queue.pop(), Some(Entry::Rename { number: 2, name: "y".repeat(MAX_NAME_CHARS) }));
        // Asked to stop, the take takes no rename until it has ended.
        status.state.store(State::Finishing as u8, Ordering::Release);
        assert_eq!(m.rename(number, "Goal","Late"), Err(RenameError::Finishing));
        assert_eq!(queue.pop(), None);
        // Ended without a session file of its own.
        status.state.store(State::Done as u8, Ordering::Release);
        assert_eq!(m.rename(number, "Goal","Late"), Err(RenameError::NoSession));
        // The log closed while the take still shows as running: the take is ending.
        status.state.store(State::Recording as u8, Ordering::Release);
        queue.close();
        assert_eq!(m.rename(number, "Goal","Late"), Err(RenameError::Finishing));
        assert_eq!(queue.pop(), None);
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
        let late = Entry::Add(Marker { number: 1, at: 5, name: "Late".into() });
        assert!(log.queue.push(late.clone()).is_ok());
        log.adding.fetch_sub(1, Ordering::SeqCst);
        assert_eq!(closer.join().ok().flatten(), Some(late), "the last read takes the marker that add pushed");
        assert!(!log.push(Entry::Add(Marker { number: 2, at: 6, name: "After".into() })), "a closed log takes no marker");
        assert_eq!(log.pop(), None);
    }
}
