//! A scripted backend for tests: applications start and quit, captures deliver, fail or stall as
//! the test says, without any system audio.

use std::collections::BTreeMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::{AppAudioError, AppInfo, AppKey, Backend, CaptureShared, OnData, PacketInfo, Permission, Support, Target, Wake, Watcher};

/// What a capture of one process does.
#[derive(Clone, Debug, PartialEq)]
pub enum Behavior {
    /// Delivers a packet of `frames` stereo frames of `value`, then reports running and delivers
    /// another each time it has waited 5 ms for a stop request. The first packet is at position 0
    /// and time [`QPC_START`], and each one follows the one before without a gap.
    Deliver { frames: usize, value: f32 },
    /// Delivers `after` packets of `frames` frames of zeros as `Deliver` does, then loses `lost`
    /// frames: the next packet's position and time move on by them, and it is marked as a
    /// discontinuity. Later packets follow without a gap.
    Gap { frames: usize, after: u64, lost: u64 },
    /// Delivers `packets` packets as `Deliver` does, then fails with this text, as a capture
    /// whose device goes away does.
    FailAfter { packets: u64, error: String },
    /// Fails at once with this text.
    Fail(String),
    /// The system refuses at once with this text ([`AppAudioError::Denied`]).
    Deny(String),
    /// The target has exited ([`AppAudioError::NotRunning`]).
    Exit,
    /// Starts and runs, then stops reporting until it is asked to stop, as a capture stuck in a
    /// wait does.
    Stall,
    /// Never returns from asking the system to start, and does not look at stop requests, until
    /// [`FakeBackend::release_hangs`], as an activation stuck in the system does.
    Hang,
    /// The system starts the stream, and no audio arrives until it is asked to stop.
    Quiet,
    /// Starts and ends without an error.
    End,
}

#[derive(Default)]
struct State {
    support: Option<Support>,
    apps: Vec<AppInfo>,
    peaks: BTreeMap<AppKey, f32>,
    behaviors: BTreeMap<u32, Behavior>,
    opened: Vec<Target>,
    scans: usize,
    peak_reads: usize,
    wakes: Vec<Wake>,
    watch_error: Option<String>,
    hold_next: bool,
    held: bool,
    /// Scans that have been held, in all.
    holds: usize,
    hold_peaks: bool,
    peaks_held: bool,
    watchers_dropped: usize,
    hangs_released: bool,
    hanging: usize,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

/// The scripted backend. Clones share one script.
#[derive(Clone, Default)]
pub struct FakeBackend {
    shared: Arc<Shared>,
}

/// An application with one process, for scripts.
pub fn app(exe: &str, pid: u32, playing: bool) -> AppInfo {
    let key = AppKey::from_exe(exe).unwrap_or(AppKey { name: exe.to_string(), path: None });
    let name = key.name.strip_suffix(".exe").unwrap_or(&key.name).to_string();
    AppInfo { key, name, path: format!("C:\\Apps\\{exe}"), pids: vec![pid], target_pid: pid, playing, within: None, slot: None, key_id: 0 }
}

impl FakeBackend {
    pub fn new() -> FakeBackend {
        FakeBackend::default()
    }

    fn with<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        let r = match self.shared.state.lock() {
            Ok(mut s) => f(&mut s),
            Err(p) => f(&mut p.into_inner()),
        };
        self.shared.changed.notify_all();
        r
    }

    /// Wait until the meters have been read `n` times in all, or `timeout` passes. Returns whether
    /// they were.
    pub fn wait_peak_reads(&self, n: usize, timeout: Duration) -> bool {
        let Ok(guard) = self.shared.state.lock() else { return false };
        self.shared.changed.wait_timeout_while(guard, timeout, |s| s.peak_reads < n).is_ok_and(|(s, _)| s.peak_reads >= n)
    }

    /// Replace the running applications, and wake the discovery thread as a system notification
    /// would.
    pub fn set_apps(&self, apps: Vec<AppInfo>) {
        let wakes = self.with(|s| {
            s.apps = apps;
            s.wakes.clone()
        });
        wakes.iter().for_each(Wake::wake);
    }

    /// Answer [`Backend::support`] with `support` (by default: supported).
    pub fn set_support(&self, support: Support) {
        self.with(|s| s.support = Some(support));
    }

    /// Make watching fail with `error`, or work again with `None`.
    pub fn set_watch_error(&self, error: Option<&str>) {
        self.with(|s| s.watch_error = error.map(str::to_string));
    }

    /// The peak level the meter of `key` reads.
    pub fn set_peak(&self, key: &AppKey, peak: f32) {
        self.with(|s| s.peaks.insert(key.clone(), peak));
    }

    /// Make the meter of `key` give no reading, as a meter that fails does.
    pub fn clear_peak(&self, key: &AppKey) {
        self.with(|s| s.peaks.remove(key));
    }

    /// Make the next meter reading wait, as a meter call stuck in the system does, until
    /// [`FakeBackend::release_peaks`].
    pub fn hold_next_peaks(&self) {
        self.with(|s| s.hold_peaks = true);
    }

    /// Wait until a meter reading is held, or `timeout` passes. Returns whether one is.
    pub fn wait_peaks_held(&self, timeout: Duration) -> bool {
        let Ok(guard) = self.shared.state.lock() else { return false };
        self.shared.changed.wait_timeout_while(guard, timeout, |s| !s.peaks_held).is_ok_and(|(s, _)| s.peaks_held)
    }

    /// Let a held meter reading finish.
    pub fn release_peaks(&self) {
        self.with(|s| s.peaks_held = false);
    }

    /// What a capture of `pid` does (by default it delivers silence).
    pub fn set_behavior(&self, pid: u32, behavior: Behavior) {
        self.with(|s| s.behaviors.insert(pid, behavior));
    }

    /// Every capture opened so far, in order.
    pub fn opened(&self) -> Vec<Target> {
        self.with(|s| s.opened.clone())
    }

    /// Scans so far.
    pub fn scans(&self) -> usize {
        self.with(|s| s.scans)
    }

    /// Make the next scan read the applications and then wait, as a scan stuck in a system call
    /// does, until [`FakeBackend::release_scan`].
    pub fn hold_next_scan(&self) {
        self.with(|s| s.hold_next = true);
    }

    /// Wait until a scan is held, or `timeout` passes. Returns whether one is.
    pub fn wait_scan_held(&self, timeout: Duration) -> bool {
        let Ok(guard) = self.shared.state.lock() else { return false };
        self.shared.changed.wait_timeout_while(guard, timeout, |s| !s.held).is_ok_and(|(s, _)| s.held)
    }

    /// Wait until the `n`th scan held since the backend was made is stuck and `discovery` reports
    /// the stall, or `timeout` passes. Returns whether it does.
    /// With a discovery bound of 0 ms the held scan has set the running thread's marker before it
    /// began, so the wait after that is for the clock to pass the bound, not for another thread.
    /// It ends early when the scan is released, and `timeout` bounds it when the thread stops
    /// while the scan is held.
    pub fn wait_scan_stalled(&self, discovery: &crate::Discovery, n: usize, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        let Ok(guard) = self.shared.state.lock() else { return false };
        let held = |s: &State| s.held && s.holds >= n;
        if !self.shared.changed.wait_timeout_while(guard, timeout, |s| !held(s)).is_ok_and(|(s, _)| held(&s)) {
            return false;
        }
        while !discovery.stalled() {
            if !self.shared.state.lock().is_ok_and(|s| s.held) || std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::yield_now();
        }
        true
    }

    /// Let a held scan return what it read.
    pub fn release_scan(&self) {
        self.with(|s| s.held = false);
    }

    /// Captures stuck in [`Behavior::Hang`] now.
    pub fn hanging(&self) -> usize {
        self.with(|s| s.hanging)
    }

    /// Wait until `n` captures are stuck in [`Behavior::Hang`], or `timeout` passes. Returns
    /// whether they are.
    pub fn wait_hanging(&self, n: usize, timeout: Duration) -> bool {
        let Ok(guard) = self.shared.state.lock() else { return false };
        self.shared.changed.wait_timeout_while(guard, timeout, |s| s.hanging < n).is_ok_and(|(s, _)| s.hanging >= n)
    }

    /// Let every capture in [`Behavior::Hang`] return, and later ones return at once.
    pub fn release_hangs(&self) {
        self.with(|s| s.hangs_released = true);
    }

    /// Wait until `n` watchers have been dropped in all, as a discovery thread does when it ends,
    /// or `timeout` passes. Returns whether they were.
    pub fn wait_watchers_dropped(&self, n: usize, timeout: Duration) -> bool {
        let Ok(guard) = self.shared.state.lock() else { return false };
        self.shared.changed.wait_timeout_while(guard, timeout, |s| s.watchers_dropped < n).is_ok_and(|(s, _)| s.watchers_dropped >= n)
    }
}

struct FakeWatcher(FakeBackend);

impl Drop for FakeWatcher {
    fn drop(&mut self) {
        self.0.with(|s| s.watchers_dropped += 1);
    }
}

impl Watcher for FakeWatcher {
    fn scan(&mut self) -> Result<Vec<AppInfo>, AppAudioError> {
        let (apps, hold) = self.0.with(|s| {
            s.scans += 1;
            let hold = std::mem::take(&mut s.hold_next);
            s.held |= hold;
            s.holds += usize::from(hold);
            (s.apps.clone(), hold)
        });
        if hold && let Ok(guard) = self.0.shared.state.lock() {
            drop(self.0.shared.changed.wait_while(guard, |s| s.held));
        }
        Ok(apps)
    }

    fn peaks(&mut self, apps: &[AppInfo], out: &mut dyn FnMut(usize, f32)) {
        let (peaks, hold) = self.0.with(|s| {
            let hold = std::mem::take(&mut s.hold_peaks);
            s.peaks_held |= hold;
            (s.peaks.clone(), hold)
        });
        if hold && let Ok(guard) = self.0.shared.state.lock() {
            drop(self.0.shared.changed.wait_while(guard, |s| s.peaks_held));
        }
        for (i, a) in apps.iter().enumerate() {
            if let Some(p) = peaks.get(&a.key) {
                out(i, *p);
            }
        }
        // The levels reach the table after this returns: a test that reads them waits with
        // `Discovery::wait_peak_passes`.
        self.0.with(|s| s.peak_reads += 1);
    }
}

impl Backend for FakeBackend {
    fn support(&self) -> Support {
        self.with(|s| s.support.clone()).unwrap_or(Support { supported: true, reason: None, permission: Permission::NotNeeded })
    }

    fn watch(&self, wake: Wake) -> Result<Box<dyn Watcher>, AppAudioError> {
        self.with(|s| match &s.watch_error {
            Some(e) => Err(AppAudioError::Failed(e.clone())),
            None => {
                s.wakes.push(wake);
                Ok(())
            }
        })?;
        Ok(Box::new(FakeWatcher(self.clone())))
    }

    fn capture(&self, target: Target, shared: &CaptureShared, on_data: &mut OnData) -> Result<(), AppAudioError> {
        let behavior = self.with(|s| {
            s.opened.push(target);
            s.behaviors.get(&target.pid).cloned()
        });
        match behavior.unwrap_or(Behavior::Deliver { frames: 480, value: 0.0 }) {
            Behavior::Fail(e) => Err(AppAudioError::Failed(e)),
            Behavior::Deny(e) => Err(AppAudioError::Denied(e)),
            Behavior::Exit => Err(AppAudioError::NotRunning(format!("process {}", target.pid))),
            Behavior::End => {
                shared.started();
                shared.running();
                Ok(())
            }
            Behavior::Hang => {
                if let Ok(mut guard) = self.shared.state.lock() {
                    guard.hanging += 1;
                    self.shared.changed.notify_all();
                    while !guard.hangs_released {
                        guard = match self.shared.changed.wait(guard) {
                            Ok(g) => g,
                            Err(p) => p.into_inner(),
                        };
                    }
                    guard.hanging -= 1;
                    self.shared.changed.notify_all();
                }
                Ok(())
            }
            Behavior::Stall => {
                shared.started();
                shared.running();
                // No heartbeat while it waits, as a thread stuck in a wait gives none.
                while !shared.wait_stop(Duration::from_secs(60)) {}
                Ok(())
            }
            Behavior::Quiet => {
                shared.started();
                while !shared.wait_stop(PACE) {
                    shared.beat();
                }
                Ok(())
            }
            Behavior::Deliver { frames, value } => deliver(shared, on_data, frames, value, u64::MAX, None).map(|_| ()),
            Behavior::Gap { frames, after, lost } => deliver(shared, on_data, frames, 0.0, u64::MAX, Some((after, lost))).map(|_| ()),
            Behavior::FailAfter { packets, error } => {
                deliver(shared, on_data, 480, 0.0, packets, None)?;
                if shared.stopping() { Ok(()) } else { Err(AppAudioError::Failed(error)) }
            }
        }
    }
}

/// How long a delivering capture waits for a stop request between packets.
const PACE: Duration = Duration::from_millis(5);

/// The performance counter time of a scripted capture's first packet, in 100 ns units.
pub const QPC_START: u64 = 1_000_000_000;

/// The performance counter time, in 100 ns units, of the frame at `position` of a scripted
/// capture.
pub fn qpc_at(position: u64) -> u64 {
    QPC_START.saturating_add(position.saturating_mul(10_000_000) / u64::from(crate::SAMPLE_RATE))
}

/// Deliver up to `limit` packets, each after waiting [`PACE`] for a stop request. With `gap`
/// `(after, lost)`, `lost` frames go missing after `after` packets.
fn deliver(
    shared: &CaptureShared,
    on_data: &mut OnData,
    frames: usize,
    value: f32,
    limit: u64,
    gap: Option<(u64, u64)>,
) -> Result<u64, AppAudioError> {
    let packet = vec![value; frames.saturating_mul(2)];
    let step = u64::try_from(frames).unwrap_or(u64::MAX);
    let mut position = 0u64;
    let mut send = |sent: u64, position: &mut u64| {
        let mut discontinuity = false;
        if let Some((after, lost)) = gap
            && sent == after
        {
            *position = position.saturating_add(lost);
            discontinuity = true;
        }
        let info = PacketInfo { position: *position, qpc_100ns: Some(qpc_at(*position)), discontinuity, silent: false };
        on_data(&packet, info);
        shared.packet(&info);
        *position = position.saturating_add(step);
    };
    shared.started();
    // The first packet arrives before the capture reports running, so a test that waits for the
    // running state finds it delivered.
    send(0, &mut position);
    shared.running();
    let mut sent = 1;
    while sent < limit && !shared.wait_stop(PACE) {
        shared.beat();
        send(sent, &mut position);
        sent += 1;
    }
    Ok(sent)
}
