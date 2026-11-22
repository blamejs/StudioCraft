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

/// Why [`TakeMarkers::add_numbered`] did not keep a marker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddError {
    /// The take is ending, or the queue is full.
    NotKept,
    /// Another add or rename was going into the queue at the same moment.
    Busy,
}

impl std::fmt::Display for AddError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AddError::NotKept => write!(f, "the take is ending or {QUEUED} markers are waiting to be written"),
            AddError::Busy => write!(f, "another marker is being added or renamed in the take"),
        }
    }
}

/// Why [`TakeMarkers::rename`] did not rename a marker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RenameError {
    /// The take gave no marker that number.
    Unknown,
    /// The take runs and the queue is full.
    Full,
    /// Another add or rename was going into the queue at the same moment.
    Busy,
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
            RenameError::Unknown => write!(f, "the take has no marker with that number"),
            RenameError::Full => write!(f, "{QUEUED} markers and renames are waiting to be written"),
            RenameError::Busy => write!(f, "{}", AddError::Busy),
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
    /// The number of the last marker kept. Markers kept are numbered 1 to this value.
    numbered: Arc<AtomicU64>,
    /// Set while an add or a rename goes into the queue, or a rename into the ended take's
    /// session file. A second caller does not wait for it.
    busy: Arc<AtomicBool>,
}

/// Holds [`TakeMarkers`]'s busy flag and clears it when dropped.
struct Busy<'a>(&'a AtomicBool);

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
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
            busy: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The busy flag, or `None` while another add or rename holds it.
    fn busy(&self) -> Option<Busy<'_>> {
        self.busy.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).ok().map(|_| Busy(&self.busy))
    }

    /// Whether the take was asked to stop: the disk thread may have journaled its last markers.
    pub fn ended(&self) -> bool {
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

    /// Add a marker named `name` at `at` on the studio's clock. False when the queue is full,
    /// the take was asked to stop, or another add or rename of the take was going into the
    /// queue at the same moment; the marker is then not kept.
    pub fn add(&self, at: Tick, name: &str) -> bool {
        self.add_frame(self.frame(at), name)
    }

    /// Add a marker named `name` at take frame `at`. False as for [`TakeMarkers::add`].
    pub fn add_frame(&self, at: u64, name: &str) -> bool {
        self.add_numbered(at, name).is_ok()
    }

    /// Add a marker named `name` at take frame `at`, as [`TakeMarkers::add_frame`] does. Returns
    /// its number in the take, which [`TakeMarkers::rename`] takes, or why it was not kept.
    pub fn add_numbered(&self, at: u64, name: &str) -> Result<u64, AddError> {
        let name: String = name.chars().take(MAX_NAME_CHARS).collect();
        // A marker that is not kept takes no number.
        let Some(busy) = self.busy() else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return Err(AddError::Busy);
        };
        let number = self.numbered.load(Ordering::Relaxed).saturating_add(1);
        if self.ended() || !self.queue.push(Entry::Add(Marker { number, at, name })) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return Err(AddError::NotKept);
        }
        self.numbered.store(number, Ordering::Relaxed);
        drop(busy);
        self.kept.fetch_add(1, Ordering::Relaxed);
        Ok(number)
    }

    /// Rename the take's marker `number`, now named `old`, to `name` (its first
    /// [`MAX_NAME_CHARS`] characters). While the take runs, the rename goes to the journal with
    /// the next sync, and the take's session gets the new name; `old` is not checked, and the
    /// last rename of the marker wins. A number the take has not given to a marker it kept is
    /// an error. Once the take has ended with a session file, the marker is renamed in that
    /// file, which is written again in one step ([`crate::session::rename_take_marker`]). A file
    /// that no longer has the marker named `old` is an error, even when `name` is `old`. `old` is
    /// cut to [`MAX_NAME_CHARS`] characters as the take cut the name it kept.
    pub fn rename(&self, number: u64, old: &str, name: &str) -> Result<(), RenameError> {
        self.rename_with(number, old, name, crate::session::rename_take_marker)
    }

    /// [`TakeMarkers::rename`], with `write` renaming the marker in an ended take's session file.
    fn rename_with(
        &self,
        number: u64,
        old: &str,
        name: &str,
        write: impl FnOnce(&std::path::Path, &std::path::Path, u64, &str, &str) -> std::io::Result<()>,
    ) -> Result<(), RenameError> {
        let name: String = name.chars().take(MAX_NAME_CHARS).collect();
        let old: String = old.chars().take(MAX_NAME_CHARS).collect();
        // Held until the rename is in the queue, so the marker's add is in it first, or until the
        // session file is written, so two renames do not both write the file from one read.
        let _busy = self.busy().ok_or(RenameError::Busy)?;
        if number == 0 || number > self.numbered.load(Ordering::Relaxed) {
            return Err(RenameError::Unknown);
        }
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
        write(&dir, session, number, &old, &name).map_err(|e| RenameError::Session(e.to_string()))
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
        assert_eq!(number, 2, "numbers count the markers the take keeps");
        assert_eq!(m.rename(0, "Goal", "Late"), Err(RenameError::Unknown));
        assert_eq!(m.rename(3, "Goal", "Late"), Err(RenameError::Unknown), "a number the take has not given");
        m.rename(number, "Goal", &"y".repeat(MAX_NAME_CHARS + 1)).unwrap();
        assert_eq!(m.rename(number, "Goal", "Late"), Err(RenameError::Full), "the queue holds three");
        assert_eq!(m.add_numbered(20, "Dropped"), Err(AddError::NotKept));
        assert!(matches!(queue.pop(), Some(Entry::Add(_))));
        assert!(matches!(queue.pop(), Some(Entry::Add(_))));
        assert_eq!(queue.pop(), Some(Entry::Rename { number: 2, name: "y".repeat(MAX_NAME_CHARS) }));
        assert_eq!(m.rename(3, "Dropped", "Late"), Err(RenameError::Unknown), "a dropped marker's number names no marker");
        // The dropped marker took no number, so the next marker kept is number 3.
        assert_eq!(m.add_numbered(30, "Next"), Ok(3));
        assert!(matches!(queue.pop(), Some(Entry::Add(Marker { number: 3, .. }))));
        // While another add or rename is going into the queue, neither waits for it.
        let held = m.busy();
        assert!(held.is_some());
        assert_eq!(m.add_numbered(40, "Busy"), Err(AddError::Busy));
        assert_eq!(m.rename(3, "Next", "Late"), Err(RenameError::Busy));
        drop(held);
        assert_eq!(queue.pop(), None);
        // Asked to stop, the take takes no rename until it has ended.
        status.state.store(State::Finishing as u8, Ordering::Release);
        assert_eq!(m.rename(number, "Goal", "Late"), Err(RenameError::Finishing));
        assert_eq!(queue.pop(), None);
        // Ended without a session file of its own.
        status.state.store(State::Done as u8, Ordering::Release);
        assert_eq!(m.rename(number, "Goal", "Late"), Err(RenameError::NoSession));
        // The log closed while the take still shows as running: the take is ending.
        status.state.store(State::Recording as u8, Ordering::Release);
        queue.close();
        assert_eq!(m.rename(number, "Goal", "Late"), Err(RenameError::Finishing));
        assert_eq!(queue.pop(), None);
    }

    #[test]
    fn an_ended_take_holds_the_busy_flag_until_its_session_file_is_written() {
        let status = Arc::new(Status::default());
        status.state.store(State::Recording as u8, Ordering::Release);
        let queue: MarkerQueue = Arc::new(MarkerLog::new(4));
        let m = TakeMarkers::new(Arc::clone(&queue), Some(Tick(0)), Arc::clone(&status));
        assert_eq!(m.add_numbered(1, "Goal"), Ok(1));
        assert_eq!(m.add_numbered(2, "Save"), Ok(2));
        // While the take runs, `old` is not checked, so a rename to it still goes to the queue.
        assert_eq!(m.rename(1, "Kickoff", "Goal"), Ok(()));
        assert!(matches!(queue.pop(), Some(Entry::Add(_))));
        assert!(matches!(queue.pop(), Some(Entry::Add(_))));
        assert_eq!(queue.pop(), Some(Entry::Rename { number: 1, name: "Goal".into() }));
        status.state.store(State::Done as u8, Ordering::Release);
        assert!(status.session.set(std::path::PathBuf::from("take").join("take.scraft")).is_ok());
        let mut during = None;
        let written = m.rename_with(1, "Goal", "Kickoff", |_, _, number, old, name| {
            assert_eq!((number, old, name), (1, "Goal", "Kickoff"));
            during = Some(m.rename_with(2, "Save", "Final", |_, _, _, _, _| Ok(())));
            Ok(())
        });
        assert_eq!(written, Ok(()));
        assert_eq!(during, Some(Err(RenameError::Busy)), "a second rename does not write the file from the same read");
        assert_eq!(m.rename_with(2, "Save", "Final", |_, _, _, _, _| Ok(())), Ok(()), "the next rename writes it");
        // An ended take's file is checked even for a rename to the name the marker has.
        let mut checked = false;
        assert_eq!(
            m.rename_with(2, "Final", "Final", |_, _, _, _, _| {
                checked = true;
                Ok(())
            }),
            Ok(())
        );
        assert!(checked);
        // A long old name is cut as the take cut the name it kept.
        let long = "z".repeat(MAX_NAME_CHARS + 5);
        let mut cut = String::new();
        assert_eq!(
            m.rename_with(2, &long, &long, |_, _, _, old, name| {
                assert_eq!(old, name);
                cut = old.to_string();
                Ok(())
            }),
            Ok(())
        );
        assert_eq!(cut.chars().count(), MAX_NAME_CHARS);
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
