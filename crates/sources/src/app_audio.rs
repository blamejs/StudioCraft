//! What one application plays: the `app_audio` source.
//!
//! The source names its application by [`AppKey`], so it follows the application when it quits
//! and starts again with a new process id. Its thread runs a small state machine every 250 ms:
//!
//! | State | Meaning |
//! |---|---|
//! | Unbound | No application is chosen. |
//! | Waiting | The application is not running or has no audio; the source is silent. |
//! | Binding | The capture is starting. |
//! | Capturing | The application's audio reaches the mixer. |
//! | Backoff | The capture failed or stalled; it opens again after 250 ms, 500 ms, 1 s, 2 s, then every 5 s. A capture the system refused waits 5 s at once. |
//!
//! The application list comes from the shared [`Discovery`], which the source keeps running while
//! it exists. The schema reads the newest list and never lists the applications itself or waits
//! for them; a schema request, and each check of the list's stamp by an open form, keeps the
//! discovery running for a minute, so a form opened before any source exists is filled too and
//! stays current while it is open.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

use craft_app_audio::{AppAudioError, AppCapture, AppKey, AppList, Backoff, CaptureState, Discovery, DiscoveryUse, SAMPLE_RATE, Target, now_ms};
use studiocraft_model::{ChoiceOption, Field, FieldKind, Settings, SettingsSchema};
use studiocraft_plugin::{Pace, Plugin, PluginError, Source, SourceCaps, SourceInstance, SourceIo};

use crate::{AudioFeed, FeedStats};

/// How often the source's thread checks its capture and the application list.
const CHECK: Duration = Duration::from_millis(250);
/// The source's error after a capture failed or stopped responding. The window shows each
/// source error through its `@msg` row; the system's own text goes to the log.
pub(crate) const CAPTURE_FAILED: &str = "The application's audio could not be captured. The capture is opened again shortly.";
/// The error for an `app` setting that is neither empty nor an application value.
pub(crate) const BAD_APP: &str = "The application setting is not a value from the application list.";
/// The source's error after the system refused to capture the application.
pub(crate) const CAPTURE_DENIED: &str = "The system refused to capture this application's audio. The capture is tried again every 5 seconds.";
/// The source's error while the running applications cannot be listed.
pub(crate) const LIST_FAILED: &str = "The running applications could not be listed. The list is read again shortly.";
/// The source's error when the system reports no reason of its own.
const UNAVAILABLE: &str = craft_app_audio::NOT_AVAILABLE;
/// How long closing a gate waits for a callback inside it to leave.
const CLOSE_WAIT: Duration = Duration::from_millis(2);
/// Lets one capture's callback reach the mixer until the source closes it. The callback passes
/// with one compare-and-swap and no lock; closing waits only while a callback is inside, and at
/// most [`CLOSE_WAIT`]. The callback turns samples into blocks and queues them without blocking,
/// so it leaves within one push (about 4 µs) unless the system has stopped scheduling the capture
/// thread; then closing returns without it, and [`Gate::left`] tells the source when it has gone.
pub(crate) struct Gate(AtomicU8);

const OPEN: u8 = 0;
const BUSY: u8 = 1;
const CLOSED: u8 = 2;
/// Closed while a callback was inside, and that callback has left.
const LEFT: u8 = 3;

impl Gate {
    pub(crate) fn new() -> Gate {
        Gate(AtomicU8::new(OPEN))
    }

    /// Run `f` if the gate is open.
    pub(crate) fn pass(&self, f: impl FnOnce()) {
        if self.0.compare_exchange(OPEN, BUSY, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return;
        }
        f();
        if self.0.compare_exchange(BUSY, OPEN, Ordering::AcqRel, Ordering::Acquire).is_err() {
            self.0.store(LEFT, Ordering::Release);
        }
    }

    /// Close the gate, and return once no callback is inside it or [`CLOSE_WAIT`] has passed.
    /// Returns whether no callback is inside.
    pub(crate) fn close(&self) -> bool {
        if self.0.swap(CLOSED, Ordering::AcqRel) != BUSY {
            return true;
        }
        let deadline = Instant::now() + CLOSE_WAIT;
        while self.0.load(Ordering::Acquire) != LEFT {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::yield_now();
        }
        true
    }

    /// Whether a callback that was inside when [`Gate::close`] gave up has left since.
    pub(crate) fn left(&self) -> bool {
        self.0.load(Ordering::Acquire) == LEFT
    }
}

/// Where the source is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum State {
    Unbound,
    Waiting,
    Binding,
    Capturing,
    Backoff { until: Instant },
}

/// A running capture as the state machine sees it.
pub(crate) trait Handle {
    fn state(&self) -> CaptureState;
    fn target(&self) -> Target;
    fn stalled(&self, now_ms: u64) -> bool;
    fn take_error(&self) -> Option<AppAudioError>;
}

impl Handle for AppCapture {
    fn state(&self) -> CaptureState {
        AppCapture::state(self)
    }
    fn target(&self) -> Target {
        AppCapture::target(self)
    }
    fn stalled(&self, now_ms: u64) -> bool {
        AppCapture::stalled(self, now_ms)
    }
    fn take_error(&self) -> Option<AppAudioError> {
        AppCapture::take_error(self)
    }
}

/// What the state machine asks of the world: where the application is, and a new capture.
pub(crate) trait Env {
    type H: Handle;
    /// Why this system cannot capture one application, when it cannot.
    fn unsupported(&self) -> Option<String>;
    /// Why the running applications could not be listed, when they could not.
    fn list_error(&self) -> Option<String>;
    /// The process a capture of `key` targets, when the application is listed.
    fn find(&self, key: &AppKey) -> Option<u32>;
    /// A new capture, or `None` when it cannot open yet: an earlier capture's callback is still
    /// handing samples on, and a later check tries again.
    fn open(&mut self, key: &AppKey, target: Target) -> Result<Option<Self::H>, AppAudioError>;
}

/// The source's state machine, with no threads of its own, so tests drive it step by step.
pub(crate) struct Machine<H> {
    pub(crate) key: Option<AppKey>,
    pub(crate) state: State,
    pub(crate) handle: Option<H>,
    backoff: Backoff,
    /// The last failure, reported by every check until a capture runs again.
    error: Option<AppAudioError>,
}

impl<H: Handle> Machine<H> {
    pub(crate) fn new(key: Option<AppKey>) -> Self {
        Machine { key, state: State::Unbound, handle: None, backoff: Backoff::default(), error: None }
    }

    /// Choose another application: the capture starts over. Returns whether anything changed.
    pub(crate) fn rebind(&mut self, key: Option<AppKey>) -> bool {
        if key == self.key {
            return false;
        }
        self.key = key;
        self.handle = None;
        self.backoff.reset();
        self.error = None;
        self.state = State::Unbound;
        true
    }

    fn fail(&mut self, now: Instant, why: AppAudioError) -> Result<(), AppAudioError> {
        // Dropping the capture hands its thread to the reaper; this thread does not wait for it.
        self.handle = None;
        let wait = match why {
            // The system will refuse again soon; the longest wait comes at once.
            AppAudioError::Denied(_) => self.backoff.fail_long(),
            _ => self.backoff.fail(),
        };
        self.state = State::Backoff { until: now + wait };
        if let AppAudioError::NotRunning(_) = why {
            // The application exited between the list and the capture: silence, and no error,
            // until the list catches up.
            self.error = None;
            return Ok(());
        }
        self.error = Some(why.clone());
        Err(why)
    }

    fn settle(&mut self, state: State) -> Result<(), AppAudioError> {
        self.state = state;
        self.error = None;
        Ok(())
    }

    /// The application is not listed: silence. A failure to list is reported; the next capture
    /// of an application that starts again opens without the waits of earlier failures.
    fn absent(&mut self, list_error: Option<String>) -> Result<(), AppAudioError> {
        self.handle = None;
        self.backoff.reset();
        self.settle(State::Waiting)?;
        list_error.map_or(Ok(()), |e| Err(AppAudioError::List(e)))
    }

    fn pending(&self) -> Result<(), AppAudioError> {
        self.error.clone().map_or(Ok(()), Err)
    }

    /// One check. An error is the failure that put the machine in its wait to try again; every
    /// check reports it until a capture runs or the application is gone.
    pub(crate) fn step(&mut self, env: &mut impl Env<H = H>, now: Instant, ms: u64) -> Result<(), AppAudioError> {
        let Some(key) = self.key.clone() else {
            self.handle = None;
            return self.settle(State::Unbound);
        };
        if let Some(why) = env.unsupported() {
            self.handle = None;
            self.state = State::Waiting;
            return Err(AppAudioError::Unsupported(why));
        }
        let found = env.find(&key);
        if found.is_none() {
            // The application quit, or is not running: silence, and no capture error.
            return self.absent(env.list_error());
        }
        // A failed scan keeps the last list: the capture goes on with the target listed then, and
        // the failure is reported while no other error is.
        let list_error = env.list_error();
        match self.listed(env, key, found, now, ms) {
            Ok(()) => list_error.map_or(Ok(()), |e| Err(AppAudioError::List(e))),
            other => other,
        }
    }

    /// One check of an application the list holds, at process `found`.
    fn listed(&mut self, env: &mut impl Env<H = H>, key: AppKey, found: Option<u32>, now: Instant, ms: u64) -> Result<(), AppAudioError> {
        if let State::Backoff { until } = self.state
            && now < until
        {
            return self.pending();
        }
        if let Some(h) = &self.handle {
            match (found, h.state()) {
                (None, _) => return self.absent(env.list_error()),
                (_, CaptureState::Failed) => {
                    let why = h.take_error().unwrap_or_else(|| AppAudioError::Failed("the capture stopped".into()));
                    return self.fail(now, why);
                }
                (_, CaptureState::Starting | CaptureState::Started | CaptureState::Running) if h.stalled(ms) => {
                    return self.fail(now, AppAudioError::Failed(format!("capturing {key} stopped responding; it is opened again")));
                }
                // The application started again with a new process, or the capture ended: bind
                // again below.
                (Some(pid), state) if pid != h.target().pid || state == CaptureState::Ended => self.handle = None,
                (Some(_), CaptureState::Running) => {
                    self.backoff.reset();
                    return self.settle(State::Capturing);
                }
                // Starting, or started with no audio yet: the backoff is not reset until audio
                // arrives.
                (Some(_), _) => {
                    self.state = State::Binding;
                    return self.pending();
                }
            }
        }
        let Some(pid) = found else {
            return self.absent(env.list_error());
        };
        match env.open(&key, Target { pid }) {
            Ok(h) => {
                self.handle = h;
                self.state = State::Binding;
                self.pending()
            }
            Err(e) => self.fail(now, e),
        }
    }
}

/// The real world: the shared discovery's newest list and its captures.
struct Live<'a> {
    discovery: &'a Discovery,
    /// The discovery thread was stuck in a system call when this check began.
    stalled: bool,
    unsupported: Option<&'a str>,
    list: &'a AppList,
    io: &'a SourceIo,
    stats: &'a mut Option<Arc<FeedStats>>,
    gate: &'a mut Option<Arc<Gate>>,
    closing: &'a mut Option<Arc<Gate>>,
}

impl Env for Live<'_> {
    type H = AppCapture;

    fn unsupported(&self) -> Option<String> {
        self.unsupported.map(str::to_string)
    }

    fn list_error(&self) -> Option<String> {
        if self.stalled || self.discovery.stalled() {
            return Some(craft_app_audio::LIST_STALLED.to_string());
        }
        self.list.error.clone()
    }

    fn find(&self, key: &AppKey) -> Option<u32> {
        self.list.find(key).map(|a| a.target_pid)
    }

    fn open(&mut self, key: &AppKey, target: Target) -> Result<Option<AppCapture>, AppAudioError> {
        // A capture of an earlier choice or process stops on its own thread; its gate closes so
        // nothing it still delivers reaches the mixer. While its callback is still inside, no new
        // capture opens.
        if let Some(old) = self.gate.take()
            && !old.close()
        {
            self.io.audio.hold(true);
            *self.closing = Some(old);
        }
        if self.closing.is_some() {
            return Ok(None);
        }
        let mut feed = AudioFeed::new(SAMPLE_RATE, craft_app_audio::CHANNELS, Arc::clone(&self.io.audio), self.io.clock);
        *self.stats = Some(feed.stats());
        let gate = Arc::new(Gate::new());
        *self.gate = Some(Arc::clone(&gate));
        self.discovery.open_capture(key.clone(), target, Box::new(move |samples: &[f32]| gate.pass(|| feed.push(samples)))).map(Some)
    }
}

/// The chosen application: `None` when `app` is missing, null or empty, and an error when it is
/// any other value that is not an application key.
fn checked_key(s: &Settings) -> Result<Option<AppKey>, PluginError> {
    let raw = match s.get("app") {
        None | Some(serde_json::Value::Null) => return Ok(None),
        Some(raw) => raw,
    };
    let bad = || PluginError::Failed(BAD_APP.to_string());
    match raw.as_str() {
        Some(text) if text.trim().is_empty() => Ok(None),
        Some(text) => AppKey::decode(text).map(Some).ok_or_else(bad),
        None => Err(bad()),
    }
}

fn schema(list: &AppList, saved: Option<&Settings>) -> SettingsSchema {
    let mut options = vec![ChoiceOption::new("", "Choose an application")];
    // Programs whose files share a name show their paths, so the entries differ.
    let label = |a: &craft_app_audio::AppInfo| {
        let shared = list.apps.iter().filter(|b| b.key.name == a.key.name).nth(1).is_some();
        if shared && !a.path.is_empty() { format!("{} ({})", a.name, a.path) } else { a.name.clone() }
    };
    let mut values: Vec<String> = list.apps.iter().map(craft_app_audio::AppInfo::choice).collect();
    // The saved value is kept as it was written. When it finds a listed application (at its path,
    // in any letter case, or by its file name after the program moved), that entry carries the
    // saved value, so the form shows one entry for the application and selects it. A chosen
    // application that is not running stays listed under its program name, so its setting is
    // kept.
    let mut kept = None;
    if let Some(raw) = saved.and_then(|s| s.str("app"))
        && let Some(key) = AppKey::decode(raw)
    {
        match list.find(&key).and_then(|a| list.apps.iter().position(|b| std::ptr::eq(a, b))) {
            Some(i) => {
                if let Some(v) = values.get_mut(i) {
                    *v = raw.to_string();
                }
            }
            None => {
                let stem = key.name.rsplit_once('.').map_or(key.name.as_str(), |(stem, _)| stem).to_string();
                let shared = list.apps.iter().any(|a| a.key.name == key.name);
                let shown = match &key.path {
                    Some(p) if shared => format!("{stem} ({p})"),
                    _ => stem,
                };
                kept = Some(ChoiceOption::new(raw.to_string(), shown));
            }
        }
    }
    options.extend(list.apps.iter().zip(values).map(|(a, v)| ChoiceOption::new(v, label(a))));
    options.extend(kept);
    SettingsSchema::new(vec![Field::new("app", "Application", FieldKind::Choice { options }, "")])
}

struct Instance {
    discovery: Discovery,
    _use: DiscoveryUse,
    /// Why this system cannot capture one application, read once when the source starts.
    unsupported: Option<String>,
    list: Arc<AppList>,
    machine: Machine<AppCapture>,
    stats: Option<Arc<FeedStats>>,
    /// Open while the newest capture may hand samples to the mixer.
    gate: Option<Arc<Gate>>,
    /// The last failure written to the log, so each one is logged once.
    logged: Option<String>,
    /// The saved `app` value is not an application value: the source stays silent and reports
    /// [`BAD_APP`] until a valid choice is made.
    bad_setting: bool,
    /// The gate of an earlier capture whose callback was still inside when the gate closed. No
    /// new capture opens until it has left; then the queue is emptied.
    closing: Option<Arc<Gate>>,
}

impl Instance {
    fn new(discovery: Discovery, key: Option<AppKey>) -> Instance {
        let support = discovery.support();
        let unsupported = (!support.supported).then(|| support.reason.unwrap_or_else(|| UNAVAILABLE.into()));
        let usage = discovery.acquire();
        let list = discovery.snapshot();
        Instance {
            discovery,
            _use: usage,
            unsupported,
            list,
            machine: Machine::new(key),
            stats: None,
            gate: None,
            logged: None,
            bad_setting: false,
            closing: None,
        }
    }

    /// The error the window shows for the machine's `why`: the system's reason it cannot capture,
    /// the capture limit, or a fixed message for a refusal, a list that cannot be read, and every
    /// other failure. The system's own text for those goes to the log, once per failure.
    fn shown_error(&mut self, why: AppAudioError) -> String {
        let shown = match &why {
            AppAudioError::Unsupported(reason) => return reason.clone(),
            AppAudioError::Limit => return why.to_string(),
            AppAudioError::Denied(_) => CAPTURE_DENIED,
            AppAudioError::List(_) => LIST_FAILED,
            AppAudioError::NotRunning(_) | AppAudioError::Busy(_) | AppAudioError::Failed(_) => CAPTURE_FAILED,
        };
        let text = why.to_string();
        if self.logged.as_deref() != Some(text.as_str()) {
            log::warn!("application audio capture: {text}");
            self.logged = Some(text);
        }
        shown.to_string()
    }

    /// A new choice: the earlier capture's samples stop reaching the mixer, and once its callback
    /// has left the feed, the blocks it queued are thrown away. A callback still inside after the
    /// gate's bounded wait is waited for by the next checks, which open no new capture until it
    /// has left and its blocks are thrown away too; until then the mixer reads nothing from the
    /// queue, so blocks that callback adds late are never played.
    fn start_over(&mut self, io: &SourceIo) {
        if let Some(gate) = self.gate.take()
            && !gate.close()
        {
            io.audio.hold(true);
            self.closing = Some(gate);
        }
        io.audio.clear();
    }

    fn check(&mut self, io: &SourceIo) -> Result<(), PluginError> {
        if self.bad_setting {
            return Err(PluginError::Failed(BAD_APP.to_string()));
        }
        if let Some(gate) = &self.closing {
            if !gate.left() {
                return Ok(());
            }
            // The callback has left, so everything it queued is in the queue now.
            io.audio.clear();
            io.audio.hold(false);
            self.closing = None;
        }
        // A discovery thread stuck in a system call is replaced once; a list it cannot read is
        // reported below.
        let stalled = self.discovery.stalled();
        self.discovery.recover();
        if self.discovery.generation() != self.list.generation {
            self.list = self.discovery.snapshot();
        }
        let mut live = Live {
            discovery: &self.discovery,
            stalled,
            unsupported: self.unsupported.as_deref(),
            list: &self.list,
            io,
            stats: &mut self.stats,
            gate: &mut self.gate,
            closing: &mut self.closing,
        };
        let result = self.machine.step(&mut live, Instant::now(), now_ms());
        // A dropped capture (quit, failed or stalled) stops on its own thread; its gate closes so a
        // stalled one that wakes later adds nothing. What it already queued is the application's
        // own recent sound and still plays, unless its callback was still inside the gate.
        if self.machine.handle.is_none()
            && let Some(gate) = self.gate.take()
            && !gate.close()
        {
            io.audio.hold(true);
            self.closing = Some(gate);
        }
        if let (Some(h), Some(s)) = (&self.machine.handle, &self.stats) {
            h.set_feed_counters(s.dropped.load(Ordering::Relaxed), s.pool_exhausted.load(Ordering::Relaxed));
        }
        if result.is_ok() {
            // A capture that runs, or an application that is not running, ends the failure; the
            // next one is logged even when its text is the same.
            self.logged = None;
        }
        result.map_err(|why| PluginError::Failed(self.shown_error(why)))
    }
}

impl SourceInstance for Instance {
    fn pace(&self) -> Pace {
        Pace::Every(CHECK)
    }
    fn produce(&mut self, io: &SourceIo) -> Result<(), PluginError> {
        self.check(io)
    }
    fn update(&mut self, s: &Settings, io: &SourceIo) -> Result<(), PluginError> {
        // A value that is not an application key changes nothing: the capture keeps running.
        let key = checked_key(s)?;
        self.bad_setting = false;
        if self.machine.rebind(key) {
            self.start_over(io);
        }
        self.check(io)
    }
}

/// What one application plays.
pub struct AppAudioSource {
    discovery: Discovery,
}

impl AppAudioSource {
    /// The source, listing applications through `discovery`.
    pub fn new(discovery: Discovery) -> Self {
        Self { discovery }
    }

    /// The newest list for a settings form, without waiting. The request keeps the list current
    /// for [`SCHEMA_LEASE`], so a form opened before any source exists is filled too: the first
    /// scan publishes a new list, the stamp changes, and the form reads the schema again.
    fn list(&self) -> Arc<AppList> {
        if self.discovery.support().supported {
            self.discovery.lease(SCHEMA_LEASE);
            self.discovery.recover();
        }
        self.discovery.snapshot()
    }
}

/// How long a schema request, or a check of the list's stamp, keeps the application list current.
const SCHEMA_LEASE: Duration = Duration::from_secs(60);

impl Plugin for AppAudioSource {
    fn id(&self) -> &'static str {
        "app_audio"
    }
    fn name(&self) -> &'static str {
        "Application audio capture"
    }
    fn schema(&self) -> SettingsSchema {
        schema(&self.list(), None)
    }
    fn schema_for(&self, settings: &Settings) -> SettingsSchema {
        schema(&self.list(), Some(settings))
    }
    fn lists_stamp(&self) -> Option<u64> {
        // An open form checks the stamp every few seconds; each check keeps the list current, so
        // a form left open longer than one lease still sees applications start and quit, and a
        // discovery thread stuck in a system call is replaced.
        if self.discovery.support().supported {
            self.discovery.lease(SCHEMA_LEASE);
            self.discovery.recover();
        }
        Some(self.discovery.generation())
    }
}

impl Source for AppAudioSource {
    fn caps(&self) -> SourceCaps {
        SourceCaps { video: false, audio: true }
    }
    fn create(&self, settings: &Settings, io: &SourceIo) -> Result<Box<dyn SourceInstance>, PluginError> {
        // A saved value that is not an application value (written by hand, or by a later version)
        // does not stop the collection from loading: the source starts silent, reports BAD_APP,
        // and keeps the setting as it is until another application is chosen.
        let key = checked_key(settings);
        let mut i = Instance::new(self.discovery.clone(), key.as_ref().ok().cloned().flatten());
        i.bad_setting = key.is_err();
        // A failure here is reported by the next check as well; the source still starts, silent.
        let _ = i.check(io);
        Ok(Box::new(i))
    }
}

#[cfg(test)]
mod tests;
