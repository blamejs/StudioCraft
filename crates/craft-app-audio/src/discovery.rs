//! The discovery thread: it lists the running applications with audio while anything needs the
//! list, publishes each new list for any thread to read, and reads peak levels while a panel shows
//! them.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;

use crate::capture::CaptureShared;
use crate::{
    AppAudioError, AppCapture, AppInfo, AppKey, AppList, Backend, CaptureState, CaptureStatus, MAX_CAPTURES, OnData, PEAK_SLOTS, PeakTable, Support,
    Target, Wake, Watcher,
};

/// How often the thread lists the applications again, in milliseconds, besides the times a system
/// notification wakes it.
pub const RESCAN_MS: u64 = 1_000;
/// How often peak levels are read while a panel shows them (about 30 times a second).
const PEAK_MS: u64 = 33;
/// How long to wait before watching again after the system refused.
const RETRY_WATCH_MS: u64 = 5_000;
/// Commands that can wait for the thread; a command that finds the queue full is dropped.
const COMMANDS: usize = 16;
/// How long [`Discovery::list_now`] waits for a scan.
const LIST_NOW_WAIT: Duration = Duration::from_secs(2);
const LIST_TIMEOUT: &str = "the system did not list the applications in time";

pub(crate) enum Cmd {
    Rescan,
    Wake,
    Stop,
}

struct Run {
    users: usize,
    tx: Option<SyncSender<Cmd>>,
    thread: Option<JoinHandle<()>>,
}

struct Inner {
    backend: Arc<dyn Backend>,
    /// The newest list, read without a lock.
    list: ArcSwap<AppList>,
    /// Taken only by writers of the list and by callers that choose to wait for one
    /// ([`Discovery::wait_newer`], [`Discovery::fresh_list`]); readers of the list never take it.
    slot: Mutex<()>,
    /// Signaled after each new list, for [`Discovery::wait_newer`].
    published: Condvar,
    generation: AtomicU64,
    peaks: PeakTable,
    visible: AtomicBool,
    /// Peak levels are also read until this time ([`crate::now_ms`]), for readers that poll.
    peaks_until: AtomicU64,
    /// The thread keeps running until this time ([`crate::now_ms`]) without any `DiscoveryUse`.
    lease_until: AtomicU64,
    /// The running thread has scanned once, so the list is not left over from an earlier run.
    fresh: AtomicBool,
    /// Which run of the thread may publish; a thread from an earlier run stops at its next wake.
    epoch: AtomicU64,
    run: Mutex<Run>,
    captures: AtomicUsize,
    live: Mutex<Vec<Weak<CaptureShared>>>,
}

/// The list of running applications with audio, and the captures of them. Clones share one
/// thread and one list.
#[derive(Clone)]
pub struct Discovery {
    inner: Arc<Inner>,
}

/// Keeps the discovery thread running; the thread stops when the last one is dropped.
pub struct DiscoveryUse {
    inner: Arc<Inner>,
}

impl Drop for DiscoveryUse {
    fn drop(&mut self) {
        let Ok(mut run) = self.inner.run.lock() else { return };
        run.users = run.users.saturating_sub(1);
        if run.users == 0 && !leased(&self.inner) {
            stop(&self.inner, &mut run);
        }
    }
}

/// Record that the thread of run `epoch` has scanned, and wake [`Discovery::fresh_list`].
fn mark_fresh(inner: &Inner, epoch: u64) {
    if inner.fresh.load(Ordering::Acquire) {
        return;
    }
    // Under the list's lock, so a waiter cannot miss the wake between its check and its wait.
    let Ok(_slot) = inner.slot.lock() else { return };
    if inner.epoch.load(Ordering::Acquire) == epoch {
        inner.fresh.store(true, Ordering::Release);
        inner.published.notify_all();
    }
}

fn leased(inner: &Inner) -> bool {
    crate::now_ms() < inner.lease_until.load(Ordering::Acquire)
}

fn stop(inner: &Inner, run: &mut Run) {
    inner.epoch.fetch_add(1, Ordering::AcqRel);
    if let Some(tx) = run.tx.take() {
        let _ = tx.try_send(Cmd::Stop);
    }
    if let Some(t) = run.thread.take() {
        crate::reaper::reap(t);
    }
}

impl Discovery {
    /// A discovery on `backend`. Nothing runs until [`Discovery::acquire`].
    pub fn new(backend: Arc<dyn Backend>) -> Discovery {
        Discovery {
            inner: Arc::new(Inner {
                backend,
                list: ArcSwap::from_pointee(AppList::default()),
                slot: Mutex::new(()),
                published: Condvar::new(),
                generation: AtomicU64::new(0),
                peaks: PeakTable::default(),
                visible: AtomicBool::new(false),
                peaks_until: AtomicU64::new(0),
                lease_until: AtomicU64::new(0),
                fresh: AtomicBool::new(false),
                epoch: AtomicU64::new(0),
                run: Mutex::new(Run { users: 0, tx: None, thread: None }),
                captures: AtomicUsize::new(0),
                live: Mutex::new(Vec::new()),
            }),
        }
    }

    /// The one discovery of this process, on the system's backend.
    pub fn system() -> Discovery {
        static SYSTEM: OnceLock<Discovery> = OnceLock::new();
        SYSTEM.get_or_init(|| Discovery::new(crate::system_backend())).clone()
    }

    /// Whether this system captures one application's audio, and why not.
    pub fn support(&self) -> Support {
        self.inner.backend.support()
    }

    /// Keep the list current until the returned value is dropped. The first use starts the
    /// thread; on a system without support nothing starts.
    pub fn acquire(&self) -> DiscoveryUse {
        if let Ok(mut run) = self.inner.run.lock() {
            run.users = run.users.saturating_add(1);
            self.start(&mut run);
        }
        DiscoveryUse { inner: Arc::clone(&self.inner) }
    }

    /// Keep the list current for the next `time` as well, without a [`DiscoveryUse`]: for a
    /// settings form that needs the list before any source holds it. Each call extends the time;
    /// once it ends and no `DiscoveryUse` remains, the thread stops by itself within a scan.
    pub fn lease(&self, time: Duration) {
        let until = crate::now_ms().saturating_add(u64::try_from(time.as_millis()).unwrap_or(u64::MAX));
        self.inner.lease_until.fetch_max(until, Ordering::AcqRel);
        if let Ok(mut run) = self.inner.run.lock() {
            self.start(&mut run);
        }
    }

    fn start(&self, run: &mut Run) {
        if run.tx.is_some() || !self.support().supported {
            return;
        }
        let (tx, rx) = sync_channel(COMMANDS);
        // Under the list's lock, as `mark_fresh` checks the epoch, so a thread of an earlier run
        // cannot mark the new run's list fresh.
        let epoch = {
            let _slot = self.inner.slot.lock();
            self.inner.fresh.store(false, Ordering::Release);
            self.inner.epoch.fetch_add(1, Ordering::AcqRel) + 1
        };
        // The new run assigns every peak row again; rows an earlier run left are freed here,
        // under the run lock its scans also take.
        for row in 0..PEAK_SLOTS {
            self.inner.peaks.free(row);
        }
        let inner = Arc::clone(&self.inner);
        let wake = Wake(tx.clone());
        let spawned = std::thread::Builder::new().name("app audio discovery".into()).spawn(move || discover(&inner, &rx, &wake, epoch));
        match spawned {
            Ok(t) => {
                run.tx = Some(tx);
                run.thread = Some(t);
            }
            Err(e) => self.publish_error(&format!("the application list could not start: {e}")),
        }
    }

    /// Whether the thread runs.
    pub fn running(&self) -> bool {
        self.inner.run.lock().is_ok_and(|r| r.tx.is_some())
    }

    /// The newest list, read without a lock.
    pub fn snapshot(&self) -> Arc<AppList> {
        self.inner.list.load_full()
    }

    /// The newest list's generation, read without a lock: compare it with a list's
    /// [`AppList::generation`] to tell whether [`Discovery::snapshot`] has a newer one.
    pub fn generation(&self) -> u64 {
        self.inner.generation.load(Ordering::Acquire)
    }

    /// Wait until a list newer than generation `after` is published, or `timeout` passes, and
    /// return the newest list.
    pub fn wait_newer(&self, after: u64, timeout: Duration) -> Arc<AppList> {
        if let Ok(guard) = self.inner.slot.lock() {
            drop(self.inner.published.wait_timeout_while(guard, timeout, |_| self.inner.list.load().generation <= after));
        }
        self.snapshot()
    }

    /// The newest list once the running thread has scanned at least once, waiting at most
    /// `timeout` for that first scan. `None` when the thread has not scanned in time, so a list
    /// left over from an earlier run is never returned as current.
    pub fn fresh_list(&self, timeout: Duration) -> Option<Arc<AppList>> {
        let guard = self.inner.slot.lock().ok()?;
        let (guard, _) = self.inner.published.wait_timeout_while(guard, timeout, |_| !self.inner.fresh.load(Ordering::Acquire)).ok()?;
        // Read under the lock, as `start` resets `fresh` under it, so the list belongs to this run.
        let list = self.inner.fresh.load(Ordering::Acquire).then(|| self.snapshot());
        drop(guard);
        list
    }

    /// Read peak levels while `visible` is true (a panel shows the list). Costs nothing otherwise.
    pub fn set_visible(&self, visible: bool) {
        self.inner.visible.store(visible, Ordering::Release);
        self.send(Cmd::Wake);
    }

    /// Read peak levels for the next `time` as well, for a reader that asks for the list now and
    /// then instead of showing it. Each call extends the time; nothing is read after it ends.
    pub fn read_peaks_for(&self, time: Duration) {
        let until = crate::now_ms().saturating_add(u64::try_from(time.as_millis()).unwrap_or(u64::MAX));
        self.inner.peaks_until.fetch_max(until, Ordering::AcqRel);
        self.send(Cmd::Wake);
    }

    /// List the applications again now.
    pub fn refresh(&self) {
        self.send(Cmd::Rescan);
    }

    fn send(&self, cmd: Cmd) {
        if let Ok(run) = self.inner.run.lock()
            && let Some(tx) = &run.tx
        {
            let _ = tx.try_send(cmd);
        }
    }

    /// The application's newest peak level, 0 to 1, while the list is visible.
    pub fn peak(&self, app: &AppInfo) -> Option<f32> {
        app.slot.and_then(|s| self.inner.peaks.get(s, &app.key))
    }

    /// The list now: the thread's newest list while it runs, once it has scanned, or else one scan
    /// on a thread of its own; either way it waits at most 2 s and then returns an empty list with
    /// the error. On a system without support, an empty list with the reason.
    pub fn list_now(&self) -> Arc<AppList> {
        let support = self.support();
        if !support.supported {
            return Arc::new(AppList { generation: 0, apps: Vec::new(), error: support.reason });
        }
        if self.running() {
            // The thread's own scan is the one to wait for; a second scan beside it would wait on
            // the same system calls.
            return self
                .fresh_list(LIST_NOW_WAIT)
                .unwrap_or_else(|| Arc::new(AppList { generation: self.generation(), apps: Vec::new(), error: Some(LIST_TIMEOUT.into()) }));
        }
        let (tx, rx) = sync_channel(1);
        let backend = Arc::clone(&self.inner.backend);
        let (wake_tx, _wake_rx) = sync_channel(1);
        let spawned = std::thread::Builder::new().name("app audio scan".into()).spawn(move || {
            let result = backend.watch(Wake(wake_tx)).and_then(|mut w| w.scan());
            let _ = tx.send(result);
        });
        let result = match spawned {
            Ok(t) => {
                let r = rx.recv_timeout(LIST_NOW_WAIT);
                if r.is_ok() {
                    let _ = t.join();
                } else {
                    crate::reaper::reap(t);
                }
                r.unwrap_or_else(|_| Err(AppAudioError::Failed(LIST_TIMEOUT.into())))
            }
            Err(e) => Err(AppAudioError::Failed(e.to_string())),
        };
        match result {
            Ok(mut apps) => {
                AppList::arrange(&mut apps);
                Arc::new(AppList { generation: self.generation(), apps, error: None })
            }
            Err(e) => Arc::new(AppList { generation: self.generation(), apps: Vec::new(), error: Some(e.to_string()) }),
        }
    }

    /// Captures that run now.
    pub fn capture_count(&self) -> usize {
        self.inner.captures.load(Ordering::Acquire)
    }

    /// Each capture that has not ended, with its counters.
    pub fn captures(&self) -> Vec<CaptureStatus> {
        let Ok(mut live) = self.inner.live.lock() else { return Vec::new() };
        live.retain(|w| w.strong_count() > 0);
        live.iter().filter_map(Weak::upgrade).map(|s| s.status()).filter(|s| !matches!(s.state, CaptureState::Ended | CaptureState::Failed)).collect()
    }

    /// Capture `target` (the application `key`), giving each packet to `on_data` on the capture's
    /// own thread. Returns at once; the capture reports [`crate::CaptureState::Running`] once the
    /// system delivers audio. At most [`MAX_CAPTURES`] run at once.
    pub fn open_capture(&self, key: AppKey, target: Target, mut on_data: OnData) -> Result<AppCapture, AppAudioError> {
        let support = self.support();
        if !support.supported {
            return Err(AppAudioError::Unsupported(support.reason.unwrap_or_default()));
        }
        if self.inner.captures.fetch_add(1, Ordering::AcqRel) >= MAX_CAPTURES {
            self.inner.captures.fetch_sub(1, Ordering::AcqRel);
            return Err(AppAudioError::Limit);
        }
        let shared = Arc::new(CaptureShared::new(key, target));
        if let Ok(mut live) = self.inner.live.lock() {
            live.retain(|w| w.strong_count() > 0);
            live.push(Arc::downgrade(&shared));
        }
        let (inner, thread_shared) = (Arc::clone(&self.inner), Arc::clone(&shared));
        let spawned = std::thread::Builder::new().name("app audio capture".into()).spawn(move || {
            // Counts the capture until its thread ends, even when it ends long after its owner let go.
            struct Running(Arc<Inner>);
            impl Drop for Running {
                fn drop(&mut self) {
                    self.0.captures.fetch_sub(1, Ordering::AcqRel);
                }
            }
            thread_shared.beat();
            // The count goes down before the state says the capture ended.
            let result = {
                let running = Running(inner);
                running.0.backend.capture(target, &thread_shared, &mut on_data)
            };
            thread_shared.finish(result.map_err(|e| e.to_string()));
        });
        match spawned {
            Ok(t) => Ok(AppCapture::new(shared, t)),
            Err(e) => {
                self.inner.captures.fetch_sub(1, Ordering::AcqRel);
                Err(AppAudioError::Failed(format!("the capture could not start: {e}")))
            }
        }
    }

    fn publish_error(&self, error: &str) {
        publish(&self.inner, None, Some(error.to_string()));
    }
}

/// Publish a new list when it differs from the newest one.
fn publish(inner: &Inner, apps: Option<Vec<AppInfo>>, error: Option<String>) {
    // The lock orders writers and the wake of waiters; readers load the list without it.
    let Ok(slot) = inner.slot.lock() else { return };
    let current = inner.list.load();
    let apps = apps.unwrap_or_else(|| current.apps.clone());
    if current.apps == apps && current.error == error && inner.generation.load(Ordering::Acquire) > 0 {
        return;
    }
    let generation = inner.generation.load(Ordering::Acquire).saturating_add(1);
    inner.list.store(Arc::new(AppList { generation, apps, error }));
    inner.generation.store(generation, Ordering::Release);
    drop(slot);
    inner.published.notify_all();
}

/// Give each listed application a peak row, keeping the rows of applications listed before, and
/// free the rows of the ones that left.
pub(crate) fn assign_slots(peaks: &PeakTable, slots: &mut BTreeMap<AppKey, u8>, apps: &mut [AppInfo]) {
    slots.retain(|k, s| {
        let keep = apps.iter().any(|a| a.key == *k);
        if !keep {
            peaks.free(usize::from(*s));
        }
        keep
    });
    for a in apps.iter_mut() {
        a.slot = match slots.get(&a.key) {
            Some(s) => Some(*s),
            None => {
                let free = (0..PEAK_SLOTS).filter_map(|i| u8::try_from(i).ok()).find(|i| !slots.values().any(|v| v == i));
                if let Some(s) = free {
                    slots.insert(a.key.clone(), s);
                    peaks.assign(usize::from(s), &a.key);
                }
                free
            }
        };
    }
}

fn discover(inner: &Arc<Inner>, rx: &Receiver<Cmd>, wake: &Wake, epoch: u64) {
    let mine = || inner.epoch.load(Ordering::Acquire) == epoch;
    let mut watcher: Option<Box<dyn Watcher>> = None;
    let mut slots: BTreeMap<AppKey, u8> = BTreeMap::new();
    let mut apps: Vec<AppInfo> = Vec::new();
    let now = Instant::now();
    let (mut next_watch, mut next_scan, mut next_peak) = (now, now, now);
    while mine() {
        // A lease that ran out with no use left stops the thread.
        if !leased(inner)
            && let Ok(mut run) = inner.run.lock()
            && run.users == 0
            && mine()
        {
            stop(inner, &mut run);
            break;
        }
        let now = Instant::now();
        if watcher.is_none() && now >= next_watch {
            match inner.backend.watch(wake.clone()) {
                Ok(w) => {
                    watcher = Some(w);
                    next_scan = now;
                }
                Err(e) => {
                    let Ok(_run) = inner.run.lock() else { break };
                    if !mine() {
                        break;
                    }
                    publish(inner, Some(Vec::new()), Some(e.to_string()));
                    mark_fresh(inner, epoch);
                    next_watch = now + Duration::from_millis(RETRY_WATCH_MS);
                }
            }
        }
        let visible = inner.visible.load(Ordering::Acquire) || crate::now_ms() < inner.peaks_until.load(Ordering::Acquire);
        if let Some(w) = watcher.as_mut() {
            if now >= next_scan {
                let scanned = w.scan();
                // Under the run lock, where runs start and stop, so a run stopped during the scan
                // changes neither the list nor the peak rows of a run started since.
                let Ok(_run) = inner.run.lock() else { break };
                if !mine() {
                    break;
                }
                match scanned {
                    Ok(mut found) => {
                        AppList::arrange(&mut found);
                        assign_slots(&inner.peaks, &mut slots, &mut found);
                        apps = found;
                        publish(inner, Some(apps.clone()), None);
                    }
                    Err(e) => publish(inner, None, Some(e.to_string())),
                }
                mark_fresh(inner, epoch);
                next_scan = Instant::now() + Duration::from_millis(RESCAN_MS);
            }
            if visible && now >= next_peak {
                // Read under the run lock, as a new run frees and assigns rows under it, so a run
                // that stops cannot write a level into a row another application has since. Each
                // reading asks the meter interfaces the scan opened for one value apiece.
                let Ok(run) = inner.run.lock() else { break };
                if !mine() {
                    break;
                }
                let peaks = &inner.peaks;
                w.peaks(&apps, &mut |i, p| {
                    if let Some(s) = apps.get(i).and_then(|a| a.slot) {
                        peaks.set(usize::from(s), p);
                    }
                });
                drop(run);
                next_peak = Instant::now() + Duration::from_millis(PEAK_MS);
            }
        }
        let mut deadline = if watcher.is_some() { next_scan } else { next_watch };
        if visible && watcher.is_some() {
            deadline = deadline.min(next_peak);
        }
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Cmd::Stop) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(Cmd::Rescan) => next_scan = Instant::now(),
            Ok(Cmd::Wake) | Err(RecvTimeoutError::Timeout) => {}
        }
    }
    // A run started since this one stopped freed every row when it started and owns them now.
    if let Ok(run) = inner.run.lock()
        && run.tx.is_none()
    {
        for s in slots.values() {
            inner.peaks.free(usize::from(*s));
        }
    }
}
