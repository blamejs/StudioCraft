//! A running capture of one application: its thread, its state, and how its owner tells that it
//! stalled.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Instant;

use serde::Serialize;

use crate::{AppKey, WEDGE_MS};

/// Receives each packet of interleaved 48 kHz stereo `f32` samples, on the capture thread. It
/// must not block.
pub type OnData = Box<dyn FnMut(&[f32]) + Send>;

/// Milliseconds since the first call in this process: the clock heartbeats are stamped with.
pub fn now_ms() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    u64::try_from(EPOCH.get_or_init(Instant::now).elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// The process a capture records, with its child processes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Target {
    pub pid: u32,
}

/// Where a capture is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureState {
    /// The thread is asking the system for the capture.
    Starting,
    /// The system delivers audio.
    Running,
    /// The capture failed; [`AppCapture::take_error`] says why.
    Failed,
    /// The capture ended without an error.
    Ended,
}

impl CaptureState {
    fn from_u8(v: u8) -> CaptureState {
        match v {
            0 => CaptureState::Starting,
            1 => CaptureState::Running,
            2 => CaptureState::Failed,
            _ => CaptureState::Ended,
        }
    }
}

/// What a capture thread and its owner share. Every field is an atomic apart from the error text,
/// which is written once when the capture fails.
pub struct CaptureShared {
    key: AppKey,
    target: Target,
    state: AtomicU8,
    heartbeat: AtomicU64,
    packets: AtomicU64,
    stop: AtomicBool,
    error: Mutex<Option<String>>,
    /// Signaled when the state changes, for [`AppCapture::wait`].
    changed: Condvar,
    feed_dropped: AtomicU64,
    feed_starved: AtomicU64,
}

impl CaptureShared {
    pub(crate) fn new(key: AppKey, target: Target) -> CaptureShared {
        CaptureShared {
            key,
            target,
            state: AtomicU8::new(0),
            heartbeat: AtomicU64::new(now_ms()),
            packets: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            error: Mutex::new(None),
            changed: Condvar::new(),
            feed_dropped: AtomicU64::new(0),
            feed_starved: AtomicU64::new(0),
        }
    }

    /// The capture thread is alive. Call it at least every [`crate::CAPTURE_WAIT_MS`].
    pub fn beat(&self) {
        self.heartbeat.store(now_ms(), Ordering::Relaxed);
    }

    /// The system delivers audio.
    pub fn running(&self) {
        self.beat();
        if self.state.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            self.signal();
        }
    }

    fn signal(&self) {
        // Taking the lock orders the notification after a waiter's check of the state.
        drop(self.error.lock());
        self.changed.notify_all();
    }

    /// A packet arrived.
    pub fn packet(&self) {
        self.packets.fetch_add(1, Ordering::Relaxed);
    }

    /// Whether the owner asked the capture to stop.
    pub fn stopping(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    pub(crate) fn finish(&self, result: Result<(), String>) {
        match result {
            Ok(()) => self.state.store(3, Ordering::Release),
            Err(e) => {
                if let Ok(mut slot) = self.error.lock() {
                    *slot = Some(e);
                }
                self.state.store(2, Ordering::Release);
            }
        }
        self.signal();
    }

    pub(crate) fn status(&self) -> CaptureStatus {
        CaptureStatus {
            app: self.key.clone(),
            pid: self.target.pid,
            state: CaptureState::from_u8(self.state.load(Ordering::Acquire)),
            packets: self.packets.load(Ordering::Relaxed),
            dropped_blocks: self.feed_dropped.load(Ordering::Relaxed),
            pool_exhausted: self.feed_starved.load(Ordering::Relaxed),
        }
    }
}

/// A capture's counters, for status reports.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CaptureStatus {
    pub app: AppKey,
    pub pid: u32,
    pub state: CaptureState,
    /// Packets the system delivered.
    pub packets: u64,
    /// Blocks the app's feed dropped because more arrived at once than it holds.
    pub dropped_blocks: u64,
    /// Blocks the app's feed had to allocate because its block pool was empty.
    pub pool_exhausted: u64,
}

/// One application's capture. Dropping it asks the capture thread to stop and hands the thread
/// to a reaper; the owner never waits for it.
pub struct AppCapture {
    shared: Arc<CaptureShared>,
    thread: Option<JoinHandle<()>>,
}

impl AppCapture {
    pub(crate) fn new(shared: Arc<CaptureShared>, thread: JoinHandle<()>) -> AppCapture {
        AppCapture { shared, thread: Some(thread) }
    }

    pub fn state(&self) -> CaptureState {
        CaptureState::from_u8(self.shared.state.load(Ordering::Acquire))
    }

    pub fn target(&self) -> Target {
        self.shared.target
    }

    /// Why the capture failed, once.
    pub fn take_error(&self) -> Option<String> {
        self.shared.error.lock().ok().and_then(|mut e| e.take())
    }

    /// Whether the capture should be running but its thread has not reported for [`WEDGE_MS`] at
    /// `now` (from [`now_ms`]): it is stuck in a system call.
    pub fn stalled(&self, now: u64) -> bool {
        matches!(self.state(), CaptureState::Starting | CaptureState::Running)
            && now.saturating_sub(self.shared.heartbeat.load(Ordering::Relaxed)) > WEDGE_MS
    }

    /// Wait until the capture leaves [`CaptureState::Starting`] or `timeout` passes, and return its
    /// state.
    pub fn wait(&self, timeout: std::time::Duration) -> CaptureState {
        self.wait_while(timeout, |s| s == CaptureState::Starting)
    }

    /// Ask the capture thread to stop, without waiting. Dropping the capture does the same.
    pub fn stop(&self) {
        self.shared.stop.store(true, Ordering::Release);
    }

    /// Wait until the capture has failed or ended, or `timeout` passes, and return its state.
    pub fn wait_ended(&self, timeout: std::time::Duration) -> CaptureState {
        self.wait_while(timeout, |s| matches!(s, CaptureState::Starting | CaptureState::Running))
    }

    fn wait_while(&self, timeout: std::time::Duration, keep_waiting: impl Fn(CaptureState) -> bool) -> CaptureState {
        let Ok(guard) = self.shared.error.lock() else { return self.state() };
        let waited = self.shared.changed.wait_timeout_while(guard, timeout, |_| keep_waiting(self.state()));
        drop(waited);
        self.state()
    }

    /// Packets delivered so far.
    pub fn packets(&self) -> u64 {
        self.shared.packets.load(Ordering::Relaxed)
    }

    /// Record the app's feed counters, for [`crate::Discovery::captures`].
    pub fn set_feed_counters(&self, dropped_blocks: u64, pool_exhausted: u64) {
        self.shared.feed_dropped.store(dropped_blocks, Ordering::Relaxed);
        self.shared.feed_starved.store(pool_exhausted, Ordering::Relaxed);
    }
}

impl Drop for AppCapture {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            crate::reaper::reap(t);
        }
    }
}
