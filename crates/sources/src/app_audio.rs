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
//! | Backoff | The capture failed or stalled; it opens again after 250 ms, 500 ms, 1 s, 2 s, then every 5 s. |
//!
//! The application list comes from the shared [`Discovery`], which the source keeps running while
//! it exists. The schema reads the newest list and never lists the applications itself; a schema
//! request keeps the discovery running for a minute, so a form opened before any source exists
//! is filled too.

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
/// The source's error when the system reports no reason of its own.
const UNAVAILABLE: &str = craft_app_audio::NOT_AVAILABLE;
/// Lets one capture's callback reach the mixer until the source closes it. The callback passes
/// with one compare-and-swap and no lock; closing waits only while a callback is inside. The
/// callback turns samples into blocks and queues them without blocking, so that wait lasts one
/// push (about 4 µs) plus any time the system leaves the capture thread unscheduled.
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

    /// Close the gate, and return once no callback is inside it.
    pub(crate) fn close(&self) {
        if self.0.swap(CLOSED, Ordering::AcqRel) != BUSY {
            return;
        }
        while self.0.load(Ordering::Acquire) != LEFT {
            std::thread::yield_now();
        }
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
    fn take_error(&self) -> Option<String>;
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
    fn take_error(&self) -> Option<String> {
        AppCapture::take_error(self)
    }
}

/// What the state machine asks of the world: where the application is, and a new capture.
pub(crate) trait Env {
    type H: Handle;
    /// Why this system cannot capture one application, when it cannot.
    fn unsupported(&self) -> Option<String>;
    /// The process a capture of `key` targets, when the application is listed.
    fn find(&self, key: &AppKey) -> Option<u32>;
    fn open(&mut self, key: &AppKey, target: Target) -> Result<Self::H, AppAudioError>;
}

/// The source's state machine, with no threads of its own, so tests drive it step by step.
pub(crate) struct Machine<H> {
    pub(crate) key: Option<AppKey>,
    pub(crate) state: State,
    pub(crate) handle: Option<H>,
    backoff: Backoff,
    /// The last failure, reported by every check until a capture runs again.
    error: Option<String>,
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

    fn fail(&mut self, now: Instant, why: String) -> Result<(), String> {
        // Dropping the capture hands its thread to the reaper; this thread does not wait for it.
        self.handle = None;
        self.state = State::Backoff { until: now + self.backoff.fail() };
        self.error = Some(why.clone());
        Err(why)
    }

    fn settle(&mut self, state: State) -> Result<(), String> {
        self.state = state;
        self.error = None;
        Ok(())
    }

    /// One check. An error is the failure that put the machine in its wait to try again; every
    /// check reports it until a capture runs or the application is gone.
    pub(crate) fn step(&mut self, env: &mut impl Env<H = H>, now: Instant, ms: u64) -> Result<(), String> {
        let Some(key) = self.key.clone() else {
            self.handle = None;
            return self.settle(State::Unbound);
        };
        if let Some(why) = env.unsupported() {
            self.handle = None;
            self.state = State::Waiting;
            return Err(why);
        }
        let found = env.find(&key);
        if let State::Backoff { until } = self.state
            && now < until
        {
            // The application quit while the source waited: silence, and no error.
            if found.is_none() {
                self.handle = None;
                return self.settle(State::Waiting);
            }
            return self.error.clone().map_or(Ok(()), Err);
        }
        if let Some(h) = &self.handle {
            match (found, h.state()) {
                // The application quit: silence, and no error.
                (None, _) => {
                    self.handle = None;
                    return self.settle(State::Waiting);
                }
                (_, CaptureState::Failed) => {
                    let why = h.take_error().unwrap_or_else(|| "the capture stopped".into());
                    return self.fail(now, why);
                }
                (_, CaptureState::Starting | CaptureState::Running) if h.stalled(ms) => {
                    return self.fail(now, format!("capturing {key} stopped responding; it is opened again"));
                }
                // The application started again with a new process, or the capture ended: bind
                // again below.
                (Some(pid), state) if pid != h.target().pid || state == CaptureState::Ended => self.handle = None,
                (Some(_), CaptureState::Running) => {
                    self.backoff.reset();
                    return self.settle(State::Capturing);
                }
                (Some(_), _) => {
                    self.state = State::Binding;
                    return self.error.clone().map_or(Ok(()), Err);
                }
            }
        }
        let Some(pid) = found else {
            return self.settle(State::Waiting);
        };
        match env.open(&key, Target { pid }) {
            Ok(h) => {
                self.handle = Some(h);
                self.state = State::Binding;
                self.error.clone().map_or(Ok(()), Err)
            }
            Err(e) => self.fail(now, e.to_string()),
        }
    }
}

/// The real world: the shared discovery's newest list and its captures.
struct Live<'a> {
    discovery: &'a Discovery,
    unsupported: Option<&'a str>,
    list: &'a AppList,
    io: &'a SourceIo,
    stats: &'a mut Option<Arc<FeedStats>>,
    gate: &'a mut Option<Arc<Gate>>,
}

impl Env for Live<'_> {
    type H = AppCapture;

    fn unsupported(&self) -> Option<String> {
        self.unsupported.map(str::to_string)
    }

    fn find(&self, key: &AppKey) -> Option<u32> {
        self.list.find(key).map(|a| a.target_pid)
    }

    fn open(&mut self, key: &AppKey, target: Target) -> Result<AppCapture, AppAudioError> {
        let mut feed = AudioFeed::new(SAMPLE_RATE, craft_app_audio::CHANNELS, Arc::clone(&self.io.audio), self.io.clock);
        *self.stats = Some(feed.stats());
        // A capture of an earlier choice stops on its own thread; its gate closes so nothing it
        // still delivers reaches the mixer.
        let gate = Arc::new(Gate::new());
        if let Some(old) = self.gate.replace(Arc::clone(&gate)) {
            old.close();
        }
        self.discovery.open_capture(key.clone(), target, Box::new(move |samples: &[f32]| gate.pass(|| feed.push(samples))))
    }
}

fn key_of(s: &Settings) -> Option<AppKey> {
    s.str("app").and_then(AppKey::decode)
}

fn schema(list: &AppList, saved: Option<&Settings>) -> SettingsSchema {
    let mut options = vec![ChoiceOption::new("", "Choose an application")];
    // Programs whose files share a name show their paths, so the entries differ.
    let label = |a: &craft_app_audio::AppInfo| {
        let shared = list.apps.iter().filter(|b| b.key.name == a.key.name).nth(1).is_some();
        if shared && !a.path.is_empty() { format!("{} ({})", a.name, a.path) } else { a.name.clone() }
    };
    options.extend(list.apps.iter().map(|a| ChoiceOption::new(a.choice(), label(a))));
    // A chosen application that is not running stays listed, so its setting is kept.
    if let Some(key) = saved.and_then(key_of)
        && !options.iter().any(|o| o.value == key.encode())
    {
        let shown = match &key.path {
            Some(p) => format!("{} ({p})", key.name),
            None => key.name.clone(),
        };
        options.push(ChoiceOption::new(key.encode(), shown));
    }
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
}

impl Instance {
    fn new(discovery: Discovery, settings: &Settings) -> Instance {
        let support = discovery.support();
        let unsupported = (!support.supported).then(|| support.reason.unwrap_or_else(|| UNAVAILABLE.into()));
        let usage = discovery.acquire();
        let list = discovery.snapshot();
        Instance { discovery, _use: usage, unsupported, list, machine: Machine::new(key_of(settings)), stats: None, gate: None, logged: None }
    }

    /// The error the window shows for the machine's `why`: the system's reason it cannot capture,
    /// the capture limit, or [`CAPTURE_FAILED`] for every other failure, whose own text is logged.
    fn shown_error(&mut self, why: String) -> String {
        if self.unsupported.as_deref() == Some(why.as_str()) || why == AppAudioError::Limit.to_string() {
            return why;
        }
        if self.logged.as_deref() != Some(why.as_str()) {
            log::warn!("application audio capture: {why}");
            self.logged = Some(why);
        }
        CAPTURE_FAILED.to_string()
    }

    /// A new choice: the earlier capture's samples stop reaching the mixer, and once its callback
    /// has left the feed, the blocks it queued are thrown away.
    fn start_over(&mut self, io: &SourceIo) {
        if let Some(gate) = self.gate.take() {
            gate.close();
        }
        while io.audio.pop().is_some() {}
    }

    fn check(&mut self, io: &SourceIo) -> Result<(), PluginError> {
        if self.discovery.generation() != self.list.generation {
            self.list = self.discovery.snapshot();
        }
        let mut live = Live {
            discovery: &self.discovery,
            unsupported: self.unsupported.as_deref(),
            list: &self.list,
            io,
            stats: &mut self.stats,
            gate: &mut self.gate,
        };
        let result = self.machine.step(&mut live, Instant::now(), now_ms());
        // A dropped capture (quit, failed or stalled) stops on its own thread; its gate closes so a
        // stalled one that wakes later adds nothing. What it already queued is the application's
        // own recent sound and still plays.
        if self.machine.handle.is_none()
            && let Some(gate) = self.gate.take()
        {
            gate.close();
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
        if self.machine.rebind(key_of(s)) {
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

    /// The newest list for a settings form. The request keeps the list current for
    /// [`SCHEMA_LEASE`], so a form opened before any source exists is filled too. When the
    /// discovery thread has just started, the request waits for its first scan, at most
    /// [`FIRST_LIST_WAIT`]; otherwise it reads the newest list without waiting.
    fn list(&self) -> Arc<AppList> {
        if !self.discovery.support().supported {
            return self.discovery.snapshot();
        }
        self.discovery.lease(SCHEMA_LEASE);
        self.discovery.fresh_list(FIRST_LIST_WAIT).unwrap_or_else(|| self.discovery.snapshot())
    }
}

/// How long a schema request keeps the application list current.
const SCHEMA_LEASE: Duration = Duration::from_secs(60);
/// How long a schema request waits for a newly started discovery's first scan.
const FIRST_LIST_WAIT: Duration = Duration::from_secs(1);

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
        Some(self.discovery.generation())
    }
}

impl Source for AppAudioSource {
    fn caps(&self) -> SourceCaps {
        SourceCaps { video: false, audio: true }
    }
    fn create(&self, settings: &Settings, io: &SourceIo) -> Result<Box<dyn SourceInstance>, PluginError> {
        let mut i = Instance::new(self.discovery.clone(), settings);
        // A failure here is reported by the next check as well; the source still starts, silent.
        let _ = i.check(io);
        Ok(Box::new(i))
    }
}

#[cfg(test)]
mod tests;
