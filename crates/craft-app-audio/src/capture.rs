//! A running capture of one application: its thread, its state, and how its owner tells that it
//! stalled.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::{AppAudioError, AppKey};

/// Receives each packet of interleaved 48 kHz stereo `f32` samples with where the packet sits on
/// the system's clocks, on the capture thread. It must not block.
pub type OnData = Box<dyn FnMut(&[f32], PacketInfo) + Send>;

/// Where a captured packet sits on the system's clocks, and whether audio was lost before it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct PacketInfo {
    /// The position of the packet's first frame in the system's stream, in frames from the start
    /// of the capture.
    pub position: u64,
    /// The system's performance counter time at which the packet's first frame was recorded, in
    /// 100 ns units: on Windows, `QueryPerformanceCounter` scaled to 100 ns. `None` when the
    /// system marked the time as uncertain.
    pub qpc_100ns: Option<u64>,
    /// The system lost audio between the previous packet and this one.
    pub discontinuity: bool,
    /// The system marked the packet silent; its samples are zeros.
    pub silent: bool,
}

impl PacketInfo {
    /// The packet as the system describes it: the device position and performance counter time
    /// of its first frame, and the system's flags for a gap before it, an uncertain time and
    /// silence.
    pub fn from_system(position: u64, qpc_100ns: u64, discontinuity: bool, timestamp_error: bool, silent: bool) -> PacketInfo {
        PacketInfo { position, qpc_100ns: (!timestamp_error).then_some(qpc_100ns), discontinuity, silent }
    }
}

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
    /// The system started the stream, and no audio has arrived yet.
    Started,
    /// Audio arrives.
    Running,
    /// The capture failed; [`AppCapture::take_error`] says why.
    Failed,
    /// The capture ended without an error.
    Ended,
}

const STARTING: u8 = 0;
const RUNNING: u8 = 1;
const FAILED: u8 = 2;
const ENDED: u8 = 3;
const STARTED: u8 = 4;

impl CaptureState {
    fn from_u8(v: u8) -> CaptureState {
        match v {
            STARTING => CaptureState::Starting,
            STARTED => CaptureState::Started,
            RUNNING => CaptureState::Running,
            FAILED => CaptureState::Failed,
            _ => CaptureState::Ended,
        }
    }
}

/// What a capture thread and its owner share. Every field is an atomic apart from the error,
/// which is written once when the capture fails.
pub struct CaptureShared {
    key: AppKey,
    target: Target,
    state: AtomicU8,
    heartbeat: AtomicU64,
    packets: AtomicU64,
    discontinuities: AtomicU64,
    stop: AtomicBool,
    error: Mutex<Option<AppAudioError>>,
    /// Signaled when the state changes or a stop is asked for.
    changed: Condvar,
    feed_dropped: AtomicU64,
    feed_starved: AtomicU64,
    /// The owner let go of the capture before its thread ended.
    abandoned: AtomicBool,
    /// The capture still counts against [`crate::MAX_CAPTURES`] in `count`.
    counted: AtomicBool,
    count: Arc<AtomicUsize>,
    /// The stall bounds for this capture: while activating, and once started.
    activate_ms: u64,
    wedge_ms: u64,
}

impl CaptureShared {
    pub(crate) fn new(key: AppKey, target: Target, count: Arc<AtomicUsize>, (activate_ms, wedge_ms): (u64, u64)) -> CaptureShared {
        CaptureShared {
            key,
            target,
            state: AtomicU8::new(STARTING),
            heartbeat: AtomicU64::new(now_ms()),
            packets: AtomicU64::new(0),
            discontinuities: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            error: Mutex::new(None),
            changed: Condvar::new(),
            feed_dropped: AtomicU64::new(0),
            feed_starved: AtomicU64::new(0),
            abandoned: AtomicBool::new(false),
            counted: AtomicBool::new(true),
            count,
            activate_ms,
            wedge_ms,
        }
    }

    /// The capture thread is alive. Call it at least every [`crate::CAPTURE_WAIT_MS`].
    pub fn beat(&self) {
        self.heartbeat.store(now_ms(), Ordering::Relaxed);
    }

    /// The system started the stream; no audio has arrived yet.
    pub fn started(&self) {
        self.beat();
        if self.state.compare_exchange(STARTING, STARTED, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            self.signal();
        }
    }

    /// Audio arrives. Call it when the first packet does.
    pub fn running(&self) {
        self.beat();
        let from = self.state.load(Ordering::Acquire);
        if matches!(from, STARTING | STARTED) && self.state.compare_exchange(from, RUNNING, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            self.signal();
        }
    }

    fn signal(&self) {
        // Taking the lock orders the notification after a waiter's check of the state.
        drop(self.error.lock());
        self.changed.notify_all();
    }

    /// A packet arrived, described by `info`.
    pub fn packet(&self, info: &PacketInfo) {
        self.packets.fetch_add(1, Ordering::Relaxed);
        if info.discontinuity {
            self.discontinuities.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Whether the owner asked the capture to stop.
    pub fn stopping(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    /// Wait until the owner asks the capture to stop or `timeout` passes. Returns whether it asked.
    pub fn wait_stop(&self, timeout: Duration) -> bool {
        let Ok(guard) = self.error.lock() else { return self.stopping() };
        drop(self.changed.wait_timeout_while(guard, timeout, |_| !self.stopping()));
        self.stopping()
    }

    fn ask_stop(&self) {
        self.stop.store(true, Ordering::Release);
        self.signal();
    }

    pub(crate) fn finish(&self, result: Result<(), AppAudioError>) {
        // The count goes down before the state says the capture ended.
        self.release();
        match result {
            Ok(()) => self.state.store(ENDED, Ordering::Release),
            Err(e) => {
                if let Ok(mut slot) = self.error.lock() {
                    *slot = Some(e);
                }
                self.state.store(FAILED, Ordering::Release);
            }
        }
        self.signal();
    }

    /// Stop counting the capture against the limit, once.
    pub(crate) fn release(&self) {
        if self.counted.swap(false, Ordering::AcqRel) {
            self.count.fetch_sub(1, Ordering::AcqRel);
        }
    }

    pub(crate) fn key(&self) -> &AppKey {
        &self.key
    }

    pub(crate) fn state(&self) -> CaptureState {
        CaptureState::from_u8(self.state.load(Ordering::Acquire))
    }

    pub(crate) fn abandoned(&self) -> bool {
        self.abandoned.load(Ordering::Acquire)
    }

    /// Whether the capture still counts against [`crate::MAX_CAPTURES`].
    pub(crate) fn counted(&self) -> bool {
        self.counted.load(Ordering::Acquire)
    }

    /// Whether the thread should be reporting and has not for too long: [`crate::ACTIVATE_MS`]
    /// while it asks the system to start, [`crate::WEDGE_MS`] after, unless the discovery set
    /// other bounds.
    pub(crate) fn stalled(&self, now: u64) -> bool {
        let quiet = now.saturating_sub(self.heartbeat.load(Ordering::Relaxed));
        match self.state() {
            CaptureState::Starting => quiet > self.activate_ms,
            CaptureState::Started | CaptureState::Running => quiet > self.wedge_ms,
            CaptureState::Failed | CaptureState::Ended => false,
        }
    }

    pub(crate) fn status(&self) -> CaptureStatus {
        CaptureStatus {
            app: self.key.clone(),
            pid: self.target.pid,
            state: self.state(),
            packets: self.packets.load(Ordering::Relaxed),
            discontinuities: self.discontinuities.load(Ordering::Relaxed),
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
    /// Packets the system marked as following lost audio.
    pub discontinuities: u64,
    /// Blocks the app's feed dropped because more arrived at once than it holds.
    pub dropped_blocks: u64,
    /// Blocks the app's feed had to allocate because its block pool was empty.
    pub pool_exhausted: u64,
}

/// One application's capture. Dropping it asks the capture thread to stop and hands the thread
/// to a reaper; the owner never waits for it. A thread that is stalled when it is dropped stops
/// counting against [`crate::MAX_CAPTURES`] at once.
pub struct AppCapture {
    shared: Arc<CaptureShared>,
    thread: Option<JoinHandle<()>>,
}

impl AppCapture {
    pub(crate) fn new(shared: Arc<CaptureShared>, thread: JoinHandle<()>) -> AppCapture {
        AppCapture { shared, thread: Some(thread) }
    }

    pub fn state(&self) -> CaptureState {
        self.shared.state()
    }

    pub fn target(&self) -> Target {
        self.shared.target
    }

    /// Why the capture failed, once.
    pub fn take_error(&self) -> Option<AppAudioError> {
        self.shared.error.lock().ok().and_then(|mut e| e.take())
    }

    /// Whether the capture's thread has not reported for too long at `now` (from [`now_ms`]): it
    /// is stuck in a system call. A capture still being activated gets [`crate::ACTIVATE_MS`], a
    /// started one [`crate::WEDGE_MS`].
    pub fn stalled(&self, now: u64) -> bool {
        self.shared.stalled(now)
    }

    /// Wait until audio arrives, the capture fails or ends, or `timeout` passes, and return its
    /// state.
    pub fn wait(&self, timeout: Duration) -> CaptureState {
        self.wait_while(timeout, |s| matches!(s, CaptureState::Starting | CaptureState::Started))
    }

    /// Ask the capture thread to stop, without waiting. Dropping the capture does the same.
    pub fn stop(&self) {
        self.shared.ask_stop();
    }

    /// Wait until the capture has failed or ended, or `timeout` passes, and return its state.
    pub fn wait_ended(&self, timeout: Duration) -> CaptureState {
        self.wait_while(timeout, |s| matches!(s, CaptureState::Starting | CaptureState::Started | CaptureState::Running))
    }

    fn wait_while(&self, timeout: Duration, keep_waiting: impl Fn(CaptureState) -> bool) -> CaptureState {
        let Ok(guard) = self.shared.error.lock() else { return self.state() };
        let waited = self.shared.changed.wait_timeout_while(guard, timeout, |_| keep_waiting(self.state()));
        drop(waited);
        self.state()
    }

    /// Packets delivered so far.
    pub fn packets(&self) -> u64 {
        self.shared.packets.load(Ordering::Relaxed)
    }

    /// Packets so far that the system marked as following lost audio.
    pub fn discontinuities(&self) -> u64 {
        self.shared.discontinuities.load(Ordering::Relaxed)
    }

    /// Record the app's feed counters, for [`crate::Discovery::captures`].
    pub fn set_feed_counters(&self, dropped_blocks: u64, pool_exhausted: u64) {
        self.shared.feed_dropped.store(dropped_blocks, Ordering::Relaxed);
        self.shared.feed_starved.store(pool_exhausted, Ordering::Relaxed);
    }
}

impl Drop for AppCapture {
    fn drop(&mut self) {
        self.shared.abandoned.store(true, Ordering::Release);
        self.shared.ask_stop();
        // A stuck thread may never return; it stops holding a place in the limit now. A thread
        // that is still answering gives its place back when it ends, within one wait.
        if self.shared.stalled(now_ms()) {
            self.shared.release();
        }
        if let Some(t) = self.thread.take() {
            crate::reaper::reap(t);
        }
    }
}
