//! The discovery thread: it lists the running applications with audio while anything needs the
//! list, publishes each new list for any thread to read, and reads peak levels while a caller asks
//! for them.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;

use crate::capture::CaptureShared;
use crate::{
    AppAudioError, AppCapture, AppInfo, AppKey, AppList, Backend, CaptureState, CaptureStatus, MAX_CAPTURES, MAX_STUCK_CAPTURES, OnData, PEAK_SLOTS,
    PeakTable, Support, Target, Wake, Watcher,
};

/// How often the thread lists the applications again, in milliseconds, besides the times a system
/// notification wakes it.
pub const RESCAN_MS: u64 = 1_000;
/// The shortest time between two scans, however often system notifications wake the thread.
const MIN_RESCAN_MS: u64 = 250;
/// How often peak levels are read while a caller asks for them (about 30 times a second).
const PEAK_MS: u64 = 33;
/// How long to wait before watching again after the system refused.
const RETRY_WATCH_MS: u64 = 5_000;
/// Commands that can wait for the thread; a command that finds the queue full is dropped.
const COMMANDS: usize = 16;
/// A discovery thread inside one system call for longer than this counts as stuck.
pub const DISCOVERY_WEDGE_MS: u64 = 5_000;
/// The most discovery threads alive at once: the running one and at most one stuck in a system
/// call beside it.
const MAX_DISCOVERY_THREADS: usize = 2;

pub(crate) enum Cmd {
    Rescan,
    Wake,
    Stop,
}

struct Run {
    users: usize,
    tx: Option<SyncSender<Cmd>>,
    thread: Option<JoinHandle<()>>,
    /// A start was put off because too many discovery threads were alive; the next thread that
    /// ends starts a run when one is still wanted.
    pending: bool,
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
    /// Peak readings whose levels are in `peaks`, for [`Discovery::wait_peak_passes`].
    peak_passes: AtomicU64,
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
    /// Captures that count against [`MAX_CAPTURES`].
    captures: Arc<AtomicUsize>,
    live: Mutex<Vec<Weak<CaptureShared>>>,
    /// When the running thread entered its current system call ([`crate::now_ms`] plus 1), 0
    /// while it is not in one. Each run writes its own marker, and a run that stops is given a
    /// new one, so a thread of a stopped run cannot change the running thread's marker.
    busy_since: ArcSwap<AtomicU64>,
    /// When a start was put off because too many discovery threads were still alive
    /// ([`crate::now_ms`] plus 1), 0 while no start waits.
    waiting_since: AtomicU64,
    /// Discovery threads that have not ended, stuck ones included.
    alive: AtomicUsize,
    /// The stall bounds, in milliseconds: a capture activating, a started capture, and the
    /// discovery thread in one system call.
    activate_ms: AtomicU64,
    wedge_ms: AtomicU64,
    discovery_wedge_ms: AtomicU64,
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

/// Count a peak reading whose levels are in the table, and wake [`Discovery::wait_peak_passes`].
fn mark_peak_pass(inner: &Inner) {
    let Ok(slot) = inner.slot.lock() else { return };
    inner.peak_passes.fetch_add(1, Ordering::AcqRel);
    drop(slot);
    inner.published.notify_all();
}

fn leased(inner: &Inner) -> bool {
    crate::now_ms() < inner.lease_until.load(Ordering::Acquire)
}

/// Whether the running thread has been in one system call, or a start has waited for earlier
/// threads to end, for longer than the bound.
fn stalled(inner: &Inner) -> bool {
    let now = crate::now_ms().saturating_add(1);
    let bound = inner.discovery_wedge_ms.load(Ordering::Relaxed);
    let over = |since: u64| since != 0 && now.saturating_sub(since) > bound;
    over(inner.busy_since.load().load(Ordering::Acquire)) || over(inner.waiting_since.load(Ordering::Acquire))
}

fn stop(inner: &Inner, run: &mut Run) {
    // Under the list's lock, as `mark_fresh` checks the epoch, so the list of the stopped run is
    // not current while no run has scanned since, even when the next start is put off.
    {
        let _slot = inner.slot.lock();
        inner.fresh.store(false, Ordering::Release);
        inner.epoch.fetch_add(1, Ordering::AcqRel);
    }
    // A thread stuck in a call no longer counts as the running one.
    inner.busy_since.store(Arc::new(AtomicU64::new(0)));
    if let Some(tx) = run.tx.take() {
        let _ = tx.try_send(Cmd::Stop);
    }
    if let Some(t) = run.thread.take() {
        crate::reaper::reap(t);
    }
}

/// Start the thread unless it runs, the system has no support, or too many discovery threads are
/// still alive (one stuck in a system call beside one that has not ended yet). A start put off
/// that way happens when one of them ends; [`stalled`] reports it once it has waited longer than
/// the bound.
fn start(inner: &Arc<Inner>, run: &mut Run) {
    if run.tx.is_some() || !inner.backend.support().supported {
        return;
    }
    if inner.alive.load(Ordering::Acquire) >= MAX_DISCOVERY_THREADS {
        if !run.pending {
            run.pending = true;
            inner.waiting_since.store(crate::now_ms().saturating_add(1), Ordering::Release);
        }
        return;
    }
    run.pending = false;
    inner.waiting_since.store(0, Ordering::Release);
    let (tx, rx) = sync_channel(COMMANDS);
    // Under the list's lock, as `mark_fresh` checks the epoch, so a thread of an earlier run
    // cannot mark the new run's list fresh.
    let epoch = {
        let _slot = inner.slot.lock();
        inner.fresh.store(false, Ordering::Release);
        inner.epoch.fetch_add(1, Ordering::AcqRel) + 1
    };
    // The new run assigns every peak row again; rows an earlier run left are freed here,
    // under the run lock its scans also take.
    for row in 0..PEAK_SLOTS {
        inner.peaks.free(row);
    }
    let busy = Arc::new(AtomicU64::new(0));
    inner.busy_since.store(Arc::clone(&busy));
    inner.alive.fetch_add(1, Ordering::AcqRel);
    let thread_inner = Arc::clone(inner);
    let wake = Wake(tx.clone());
    let spawned = std::thread::Builder::new().name("app audio discovery".into()).spawn(move || discover(&thread_inner, &rx, &wake, epoch, &busy));
    match spawned {
        Ok(t) => {
            run.tx = Some(tx);
            run.thread = Some(t);
        }
        Err(e) => {
            inner.alive.fetch_sub(1, Ordering::AcqRel);
            publish(inner, None, Some(format!("the application list could not start: {e}")));
        }
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
                peak_passes: AtomicU64::new(0),
                visible: AtomicBool::new(false),
                peaks_until: AtomicU64::new(0),
                lease_until: AtomicU64::new(0),
                fresh: AtomicBool::new(false),
                epoch: AtomicU64::new(0),
                run: Mutex::new(Run { users: 0, tx: None, thread: None, pending: false }),
                captures: Arc::new(AtomicUsize::new(0)),
                live: Mutex::new(Vec::new()),
                busy_since: ArcSwap::from_pointee(AtomicU64::new(0)),
                waiting_since: AtomicU64::new(0),
                alive: AtomicUsize::new(0),
                activate_ms: AtomicU64::new(crate::ACTIVATE_MS),
                wedge_ms: AtomicU64::new(crate::WEDGE_MS),
                discovery_wedge_ms: AtomicU64::new(DISCOVERY_WEDGE_MS),
            }),
        }
    }

    /// Change the stall bounds, in milliseconds, from [`crate::ACTIVATE_MS`], [`crate::WEDGE_MS`]
    /// and [`DISCOVERY_WEDGE_MS`]: for a capture still activating, a started capture, and the
    /// discovery thread inside one system call. Captures opened later use the new bounds. Tests
    /// use it to reach a stall without waiting seconds.
    pub fn set_stall_limits(&self, activate_ms: u64, wedge_ms: u64, discovery_ms: u64) {
        self.inner.activate_ms.store(activate_ms, Ordering::Relaxed);
        self.inner.wedge_ms.store(wedge_ms, Ordering::Relaxed);
        self.inner.discovery_wedge_ms.store(discovery_ms, Ordering::Relaxed);
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
            start(&self.inner, &mut run);
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
            start(&self.inner, &mut run);
        }
    }

    /// Whether the thread runs.
    pub fn running(&self) -> bool {
        self.inner.run.lock().is_ok_and(|r| r.tx.is_some())
    }

    /// Whether the running thread has been inside one system call for longer than
    /// [`DISCOVERY_WEDGE_MS`] (or the bound [`Discovery::set_stall_limits`] set), so the list is
    /// no longer kept current. Read without a lock.
    pub fn stalled(&self) -> bool {
        stalled(&self.inner)
    }

    /// When the running thread is stuck in a system call, leave it and start another, once: while
    /// one stuck thread is still alive, no further thread is started beside it. Returns whether a
    /// new thread started. Costs one atomic read when nothing is stuck.
    pub fn recover(&self) -> bool {
        if !self.stalled() {
            return false;
        }
        let Ok(mut run) = self.inner.run.lock() else { return false };
        if run.tx.is_none() || !self.stalled() || self.inner.alive.load(Ordering::Acquire) >= MAX_DISCOVERY_THREADS {
            return false;
        }
        stop(&self.inner, &mut run);
        start(&self.inner, &mut run);
        run.tx.is_some()
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
    /// left over from an earlier run is never returned as current. With a zero `timeout` it does
    /// not wait.
    pub fn fresh_list(&self, timeout: Duration) -> Option<Arc<AppList>> {
        let guard = self.inner.slot.lock().ok()?;
        let (guard, _) = self.inner.published.wait_timeout_while(guard, timeout, |_| !self.inner.fresh.load(Ordering::Acquire)).ok()?;
        // Read under the lock, as `start` resets `fresh` under it, so the list belongs to this run.
        let list = self.inner.fresh.load(Ordering::Acquire).then(|| self.snapshot());
        drop(guard);
        list
    }

    /// Peak readings that have put their levels in the table so far.
    pub fn peak_passes(&self) -> u64 {
        self.inner.peak_passes.load(Ordering::Acquire)
    }

    /// Wait until `n` peak readings in all have put their levels in the table, or `timeout`
    /// passes. Returns whether they have.
    pub fn wait_peak_passes(&self, n: u64, timeout: Duration) -> bool {
        let Ok(guard) = self.inner.slot.lock() else { return false };
        let passes = || self.inner.peak_passes.load(Ordering::Acquire);
        drop(self.inner.published.wait_timeout_while(guard, timeout, |_| passes() < n));
        passes() >= n
    }

    /// Read peak levels while `visible` is true, for a caller that shows meters. Nothing is read
    /// otherwise.
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

    /// The application's current peak level, 0 to 1. `None` when no meter reading of it is
    /// current: before the first reading, once readings have stopped for longer than
    /// [`crate::PEAK_FRESH_MS`], and past the table's last row.
    pub fn peak(&self, app: &AppInfo) -> Option<f32> {
        let id = if app.key_id != 0 { app.key_id } else { app.key.id() };
        app.slot.and_then(|s| self.inner.peaks.get(s, id, crate::now_ms()))
    }

    /// Captures that count against [`MAX_CAPTURES`] now.
    pub fn capture_count(&self) -> usize {
        self.inner.captures.load(Ordering::Acquire)
    }

    /// Each capture whose owner holds it and that has not ended, with its counters.
    pub fn captures(&self) -> Vec<CaptureStatus> {
        let Ok(mut live) = self.inner.live.lock() else { return Vec::new() };
        live.retain(|w| w.strong_count() > 0);
        live.iter()
            .filter_map(Weak::upgrade)
            .filter(|s| !s.abandoned())
            .map(|s| s.status())
            .filter(|s| !matches!(s.state, CaptureState::Ended | CaptureState::Failed))
            .collect()
    }

    /// Capture `target` (the application `key`), giving each packet to `on_data` on the capture's
    /// own thread. Returns at once; the capture reports [`crate::CaptureState::Running`] once
    /// audio arrives. At most [`MAX_CAPTURES`] count at once. While an earlier capture of `key`
    /// that its owner let go is stuck in a system call, no new one starts
    /// ([`AppAudioError::Busy`]), so a stuck application costs one thread, not one per retry.
    /// While [`MAX_STUCK_CAPTURES`] such threads of any applications are alive, no capture starts
    /// ([`AppAudioError::Limit`]).
    pub fn open_capture(&self, key: AppKey, target: Target, mut on_data: OnData) -> Result<AppCapture, AppAudioError> {
        let support = self.support();
        if !support.supported {
            return Err(AppAudioError::Unsupported(support.reason.unwrap_or_default()));
        }
        let shared = {
            let Ok(mut live) = self.inner.live.lock() else { return Err(AppAudioError::Failed("the capture list is unavailable".into())) };
            live.retain(|w| w.strong_count() > 0);
            let now = crate::now_ms();
            let mut busy = false;
            for s in live.iter().filter_map(Weak::upgrade).filter(|s| s.abandoned() && s.stalled(now)) {
                // A thread that got stuck after its owner let go gives its place back here.
                s.release();
                busy |= *s.key() == key;
            }
            if busy {
                return Err(AppAudioError::Busy(key.to_string()));
            }
            // Threads whose owners let go and that no longer count against the limit: only the
            // thread itself still holds its capture, so each one is a thread still alive.
            let stuck = live
                .iter()
                .filter_map(Weak::upgrade)
                .filter(|s| s.abandoned() && !s.counted() && !matches!(s.state(), CaptureState::Ended | CaptureState::Failed))
                .count();
            if stuck >= MAX_STUCK_CAPTURES {
                return Err(AppAudioError::Limit);
            }
            if self.inner.captures.fetch_add(1, Ordering::AcqRel) >= MAX_CAPTURES {
                self.inner.captures.fetch_sub(1, Ordering::AcqRel);
                return Err(AppAudioError::Limit);
            }
            let limits = (self.inner.activate_ms.load(Ordering::Relaxed), self.inner.wedge_ms.load(Ordering::Relaxed));
            let shared = Arc::new(CaptureShared::new(key, target, Arc::clone(&self.inner.captures), limits));
            live.push(Arc::downgrade(&shared));
            shared
        };
        let (backend, thread_shared) = (Arc::clone(&self.inner.backend), Arc::clone(&shared));
        let spawned = std::thread::Builder::new().name("app audio capture".into()).spawn(move || {
            // Gives the capture's place back even if the backend panics.
            struct Release(Arc<CaptureShared>);
            impl Drop for Release {
                fn drop(&mut self) {
                    self.0.release();
                }
            }
            let guard = Release(thread_shared);
            guard.0.beat();
            let result = backend.capture(target, &guard.0, &mut on_data);
            guard.0.finish(result);
        });
        match spawned {
            Ok(t) => Ok(AppCapture::new(shared, t)),
            Err(e) => {
                shared.release();
                Err(AppAudioError::Failed(format!("the capture could not start: {e}")))
            }
        }
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
        a.key_id = a.key.id();
        a.slot = match slots.get(&a.key) {
            Some(s) => Some(*s),
            None => {
                let free = (0..PEAK_SLOTS).filter_map(|i| u8::try_from(i).ok()).find(|i| !slots.values().any(|v| v == i));
                if let Some(s) = free {
                    slots.insert(a.key.clone(), s);
                    peaks.assign(usize::from(s), a.key_id);
                }
                free
            }
        };
    }
}

fn discover(inner: &Arc<Inner>, rx: &Receiver<Cmd>, wake: &Wake, epoch: u64, busy: &AtomicU64) {
    let mine = || inner.epoch.load(Ordering::Acquire) == epoch;
    // Marks the time this run spends in a system call, so a stuck call shows in `stalled`. The
    // marker is this run's own; once the run stops, `stalled` no longer reads it.
    let enter = || busy.store(crate::now_ms().saturating_add(1), Ordering::Release);
    let leave = || busy.store(0, Ordering::Release);
    let mut watcher: Option<Box<dyn Watcher>> = None;
    let mut slots: BTreeMap<AppKey, u8> = BTreeMap::new();
    let mut apps: Vec<AppInfo> = Vec::new();
    // One peak reading's levels, by peak row, and which rows got one, allocated once.
    let mut read: Vec<Option<f32>> = vec![None; PEAK_SLOTS];
    let now = Instant::now();
    let (mut next_watch, mut next_scan, mut next_peak) = (now, now, now);
    let mut last_scan: Option<Instant> = None;
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
            enter();
            let watched = inner.backend.watch(wake.clone());
            leave();
            match watched {
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
                enter();
                let scanned = w.scan();
                leave();
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
                let done = Instant::now();
                last_scan = Some(done);
                next_scan = done + Duration::from_millis(RESCAN_MS);
            }
            if visible && now >= next_peak {
                // The meters are read without a lock, since a meter call can stall. An application
                // with no reading this time has no level, rather than its last one.
                read.fill(None);
                enter();
                w.peaks(&apps, &mut |i, p| {
                    if let Some(r) = apps.get(i).and_then(|a| a.slot).and_then(|s| read.get_mut(usize::from(s))) {
                        *r = Some(p);
                    }
                });
                leave();
                // Written under the run lock, as a new run frees and assigns rows under it, so a
                // run that stopped during the reading writes no level into another application's
                // row.
                let Ok(run) = inner.run.lock() else { break };
                if !mine() {
                    break;
                }
                let stamp = crate::now_ms();
                for s in apps.iter().filter_map(|a| a.slot).map(usize::from) {
                    match read.get(s).copied().flatten() {
                        Some(p) => inner.peaks.set(s, p, stamp),
                        None => inner.peaks.clear(s),
                    }
                }
                // Counted while the run lock still keeps another run from freeing the rows.
                mark_peak_pass(inner);
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
            Ok(Cmd::Rescan) => {
                // Notifications can arrive in bursts; scans stay at least MIN_RESCAN_MS apart.
                let now = Instant::now();
                let soonest = last_scan.map_or(now, |t| (t + Duration::from_millis(MIN_RESCAN_MS)).max(now));
                next_scan = next_scan.min(soonest);
            }
            Ok(Cmd::Wake) | Err(RecvTimeoutError::Timeout) => {}
        }
    }
    drop(watcher);
    let Ok(mut run) = inner.run.lock() else {
        inner.alive.fetch_sub(1, Ordering::AcqRel);
        return;
    };
    // A run started since this one stopped freed every row when it started and owns them now.
    if run.tx.is_none() {
        for s in slots.values() {
            inner.peaks.free(usize::from(*s));
        }
    }
    inner.alive.fetch_sub(1, Ordering::AcqRel);
    // A start put off while this thread was alive happens now, if the list is still wanted.
    if run.pending {
        if run.tx.is_none() && (run.users > 0 || leased(inner)) {
            start(inner, &mut run);
        } else {
            run.pending = false;
            inner.waiting_since.store(0, Ordering::Release);
        }
    }
}
