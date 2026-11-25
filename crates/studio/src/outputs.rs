//! Starting and stopping the recording, the stream and the replay buffer, and saving replays.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use craft_hwenc::{
    Budget, Cancel, Candidate, Choice, Clock, Error as HwError, FfmpegProber, Fingerprint, Input, Loaded, Os, Outcome, Preference, ProbeCache,
    Prober, SystemClock, Target, Vendor,
};
use studiocraft_encode::SessionError;
use studiocraft_encode::h264::RECORDING_KEYFRAME_S;
use studiocraft_encode::hardware::{self, Hardware};
use studiocraft_engine::mixer::MixerCmd;
use studiocraft_media::Tick;
use studiocraft_media::audio::SAMPLE_RATE;
use studiocraft_media::queue::Fifo;
use studiocraft_media::stats::EncoderStats;
use studiocraft_media::video::{PixelFormat, PooledFrame};
use studiocraft_model::{KeyRef, Profile, RecordingFormat, RecordingQuality, Settings};
use studiocraft_outputs::encoders::HARDWARE;
use studiocraft_outputs::replay::MAX_SAVES;
use studiocraft_outputs::stream::diagnose;
use studiocraft_platform::ffmpeg;
use studiocraft_plugin::{
    Availability, DestinationCounts, DestinationSetup, DestinationState, DestinationStatus, Encoder, EncoderEnv, EncoderKind, OutputInstance,
    OutputJob, OutputStatus, PluginError, RateControl, StreamCopyState, VideoFormat, VideoParams,
};

use crate::{Notice, Studio, StudioError};

/// The outputs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OutputKind {
    Recording,
    Streaming,
    /// The replay buffer: the newest seconds kept in memory until a save writes them to a file.
    Replay,
}

impl OutputKind {
    fn noun(self) -> &'static str {
        match self {
            OutputKind::Recording => "recording",
            OutputKind::Streaming => "stream",
            OutputKind::Replay => "replay buffer",
        }
    }
}

/// What the interface shows about an output.
#[derive(Clone, Debug, PartialEq)]
pub struct OutputState {
    /// Running (frames go to it).
    pub active: bool,
    /// Finishing its file or stream after a stop.
    pub stopping: bool,
    pub status: OutputStatus,
    /// The file, or the server.
    pub target: String,
    pub elapsed: Duration,
    /// Bytes written so far.
    pub bytes: u64,
    /// For a running stream, how many of its destinations are live and how many wait to try
    /// again; zero otherwise.
    pub destinations: DestinationCounts,
}

impl OutputState {
    pub fn status_text(&self) -> String {
        match (&self.status, self.stopping) {
            (_, true) => "stopping".into(),
            (OutputStatus::Starting, _) => "starting".into(),
            (OutputStatus::Running, _) => "running".into(),
            (OutputStatus::Stopped, _) => "stopped".into(),
            (OutputStatus::Failed(e), _) => format!("failed: {e}"),
        }
    }
}

struct Active {
    id: u64,
    instance: Box<dyn OutputInstance>,
    stats: Arc<EncoderStats>,
    started: Instant,
    /// The studio clock's time of the output's first frame and sample.
    start: Tick,
    target: String,
    /// The hardware encoder it runs, chosen by the `hardware` choice or by name.
    hardware: Option<HardwareRun>,
}

/// A hardware encoder an output runs, chosen through the `hardware` encoder or by name.
#[derive(Clone, Debug, PartialEq)]
struct HardwareRun {
    candidate: &'static Candidate,
    /// The ffmpeg program it runs in.
    program: PathBuf,
    /// Whether the automatic choice picked it.
    auto: bool,
}

/// The longest one test encode of a hardware encoder may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// The longest the hardware encoder tests of one start may take together.
const PROBE_BUDGET: Duration = Duration::from_secs(10);

/// The file in the settings folder that keeps the hardware encoders' test results between
/// sessions.
pub const HARDWARE_TESTS_FILE: &str = "hardware-encoder-tests.txt";

/// Where the test results are kept between sessions, and what saves them.
#[derive(Default)]
struct Saved {
    /// The file; none keeps the results for the session only.
    path: Option<PathBuf>,
    /// Set once the session's cache has read the file.
    read: Arc<AtomicBool>,
    /// Raised when the results are forgotten; a test thread started before saves nothing.
    epoch: Arc<AtomicU64>,
    /// Held while the file is written or removed.
    writing: Arc<Mutex<()>>,
    /// The epoch and fingerprint the file was last saved for, so an encoder that fails while it
    /// runs is taken out of the file as well.
    last: Arc<Mutex<Option<(u64, Fingerprint)>>>,
    /// The last fingerprint and the program file's size and modification time it was made for, so
    /// `ffmpeg -version` runs again only after the program changed.
    known: Arc<Mutex<Option<(String, Fingerprint)>>>,
}

/// The driver text of the saved tests' fingerprint: the graphics adapter's ids, name and driver,
/// and this build's version, so an update tests the built-in encoders again.
fn tested_with(adapter: Option<String>) -> String {
    format!("{}; StudioCraft {}", adapter.unwrap_or_default(), env!("CARGO_PKG_VERSION"))
}

/// What a test thread reads from and saves to the file of [`Saved`].
struct SavedRun {
    path: PathBuf,
    /// The ffmpeg program the tests run in.
    program: PathBuf,
    /// The graphics adapter's ids and name, and its driver's name and version.
    driver: String,
    /// [`Saved::read`] of the cache the tests use.
    read: Arc<AtomicBool>,
    /// [`Saved::epoch`] when the tests started, and the current one.
    epoch: u64,
    current: Arc<AtomicU64>,
    writing: Arc<Mutex<()>>,
    last: Arc<Mutex<Option<(u64, Fingerprint)>>>,
    known: Arc<Mutex<Option<(String, Fingerprint)>>>,
}

impl SavedRun {
    /// The fingerprint of the ffmpeg program and the driver. The file's results are read into
    /// `cache` first, once per cache. When the program was replaced while the app runs, the
    /// session's results for the old one are forgotten.
    fn read(&self, cache: &ProbeCache, cancel: &Cancel) -> Fingerprint {
        let stamp = craft_hwenc::program_stamp(&self.program);
        let known = self.known.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        let f = match (stamp, known) {
            (Some(s), Some((was, f))) if s == was && f.program == self.program && f.driver == self.driver => f,
            (stamp, known) => {
                let mut f = Fingerprint::of(&self.program, self.driver.clone(), cancel, PROBE_TIMEOUT);
                f.also.push(PathBuf::from(IN_PROCESS));
                if cancel.is_canceled() {
                    return f;
                }
                if known.is_some_and(|(_, k)| k.program == f.program && k.ffmpeg_version != f.ffmpeg_version) {
                    cache.clear();
                }
                if let Some(s) = stamp {
                    *self.known.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some((s, f.clone()));
                }
                f
            }
        };
        // A canceled `-version` gives no version, so the file is read by the next tests instead.
        if !cancel.is_canceled() && !self.read.swap(true, Ordering::SeqCst) {
            match cache.load(&self.path, &f) {
                Loaded::Corrupt(why) => log::warn!("the saved hardware encoder tests at {} were not read: {why}", self.path.display()),
                loaded => log::info!("{}: {loaded}", self.path.display()),
            }
        }
        f
    }

    /// Save `cache` for `fingerprint`, unless the results were forgotten since the tests started.
    fn save(&self, cache: &ProbeCache, fingerprint: &Fingerprint) {
        let _writing = self.writing.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.current.load(Ordering::SeqCst) != self.epoch {
            return;
        }
        match cache.save(&self.path, fingerprint) {
            Ok(_) => *self.last.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some((self.epoch, fingerprint.clone())),
            Err(e) => log::warn!("the hardware encoder tests could not be saved to {}: {e}", self.path.display()),
        }
    }
}

impl Saved {
    /// Save `cache` again for the fingerprint it was last saved for, which leaves out the
    /// encoders that failed while they ran. Nothing is written when the results were forgotten
    /// since then.
    fn save_again(&self, cache: &ProbeCache) {
        let Some(path) = &self.path else { return };
        let _writing = self.writing.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let last = self.last.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        let Some((_, fingerprint)) = last.filter(|(e, _)| *e == self.epoch.load(Ordering::SeqCst)) else { return };
        if let Err(e) = cache.save(path, &fingerprint) {
            log::warn!("the hardware encoder tests could not be saved to {}: {e}", path.display());
        }
    }
}

/// The notice when the hardware encoder tests could not run at all.
const TESTS_FAILED: &str = "The hardware encoder tests could not run: {error}";

struct Stopping {
    kind: OutputKind,
    target: String,
    /// Whether the user was already told the output failed while it ran.
    failed: bool,
    handle: JoinHandle<Result<(), PluginError>>,
    /// Where the stop thread puts the state of a stream's copy, whether the copy's failure was
    /// already shown while the stream ran, and the stream's number ([`Outputs::streams`]).
    copy: Option<(CopySlot, bool, u64)>,
    /// The file of the stream's copy, for [`Outputs::copy_ended`].
    copy_path: Option<PathBuf>,
}

/// A replay being written to its file.
struct Saving {
    path: PathBuf,
    handle: JoinHandle<Result<(), PluginError>>,
}

#[derive(Default)]
pub(crate) struct Outputs {
    active: HashMap<OutputKind, Active>,
    stopping: Vec<Stopping>,
    saving: Vec<Saving>,
    next_id: u64,
    /// Video encoders each ffmpeg program offers, asked once per program.
    encoders: HashMap<PathBuf, Vec<String>>,
    /// Notices of outputs stopped on the calling thread, for the next [`Outputs::poll`].
    finished: Vec<Option<Notice>>,
    /// The hardware encoders' test results for this session.
    probes: Arc<ProbeCache>,
    /// The file that keeps them between sessions.
    saved: Saved,
    /// Held by a running test, so tests for two outputs do not hold an encoder at once.
    probing: Arc<Mutex<()>>,
    /// How often the graphics device changed; a test for an earlier device is not used.
    device_changes: u64,
    /// Outputs waiting for their hardware encoder tests, which run on their own thread.
    testing: HashMap<OutputKind, Testing>,
    /// Canceled tests whose threads may still run; the next tests wait for them, so two
    /// ffmpeg tests do not hold an encoder at once.
    canceled: Vec<JoinHandle<Result<Resolved, StudioError>>>,
    /// Hardware choices whose tests finished, for the start that follows.
    resolved: HashMap<OutputKind, Resolved>,
    /// Stream keys held for this session by name, for destinations whose key is
    /// [`KeyRef::Held`]. They are never saved.
    held_keys: HashMap<String, String>,
    /// Where the key of each destination of the last stream started comes from, by id.
    stream_keys: HashMap<String, KeyRef>,
    /// The fingerprint of each destination's server and key when the last stream started, by id,
    /// for the go-live check.
    stream_prints: HashMap<String, u64>,
    /// Each destination's server when the last stream started, by id.
    stream_servers: HashMap<String, String>,
    /// Each destination's name when the last stream started, by id, for its notices.
    stream_names: HashMap<String, String>,
    /// The name of the profile the last stream started with; `None` once that profile is removed.
    stream_profile: Option<String>,
    /// By profile name, the keys its streams' automatic connection checks are kept under
    /// ([`stream_check_key`]), the last [`MAX_CHECK_KEYS`] of each.
    stream_check_keys: HashMap<String, Vec<String>>,
    /// The reconnect settings the last stream started with.
    stream_reconnect: Option<studiocraft_model::ReconnectSettings>,
    /// The last [`MAX_UNFINISHED`] outputs, by kind and target, that stopped without finishing
    /// their file or stream cleanly, oldest first.
    unfinished: Vec<(OutputKind, String)>,
    /// The running stream's destinations' reconnect counts as last seen.
    reconnects: crate::events::ReconnectWatch,
    /// Reconnect events seen, each with its stream's start on the studio clock, for
    /// [`Outputs::reconnects_now`]; filled as a stream stops too.
    lost_connections: Vec<(crate::events::Kind, Tick)>,
    /// Which lost connections and reconnects of the running stream got a notice.
    reconnect_notices: crate::events::ReconnectNotices,
    /// Whether a failure of the running stream's copy got a notice.
    copy_noticed: bool,
    /// The failure of the running stream's copy once it got its notice, for
    /// [`Outputs::running_copy_failure`].
    copy_failure: Option<String>,
    /// The file of the running stream's copy, for [`Outputs::copy_ended`].
    copy_path: Option<PathBuf>,
    /// The state of the last stream's copy once that stream has ended, for [`Outputs::stream_copy`].
    last_copy: Option<StreamCopyState>,
    /// The copies of streams that ended, each file, whether it was finished and its
    /// [`StreamCopyState::start_ns`], oldest first, for [`Outputs::copy_ended`] and
    /// [`Outputs::copy_start`]: the last [`MAX_UNFINISHED`], and every copy that ended since the
    /// last poll began, however many there are, until the next poll.
    ended_copies: Vec<(PathBuf, bool, u64)>,
    /// How many streams have started; the last one's number. Only the copy of that stream
    /// becomes [`Outputs::last_copy`] when its stop finishes.
    streams: u64,
}

/// Where a stop thread puts the state of the stream's copy once the stream has ended.
type CopySlot = Arc<Mutex<Option<StreamCopyState>>>;

/// Stop `instance`, then put the state of its stream's copy in `slot`.
fn stop_instance(instance: &mut dyn OutputInstance, slot: Option<&CopySlot>) -> Result<(), PluginError> {
    let r = instance.stop();
    if let Some(slot) = slot {
        *slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = instance.stream_copy();
    }
    r
}

/// The notice for a stream's copy once the stream has ended; none for a failure already shown
/// while the stream ran (`noticed`).
fn copy_notice(c: &StreamCopyState, noticed: bool) -> Option<Notice> {
    let path = ("path", c.path.display().to_string());
    match &c.failure {
        Some(_) if noticed => None,
        Some(e) => Some(Notice::new("The stream's copy could not be finished: {error}", vec![("error", e.clone())], true)),
        None if c.lost_packets > 0 => Some(Notice::new(
            "Stream copy saved to {path}; {packets} packets were left out while the disk fell behind",
            vec![path, ("packets", c.lost_packets.to_string())],
            true,
        )),
        None => Some(Notice::new("Stream copy saved to {path}", vec![path], false)),
    }
}

/// Unfinished outputs [`Outputs`] keeps for [`Outputs::take_unfinished`].
const MAX_UNFINISHED: usize = 16;

/// What [`resolve_hardware`] gives: the encoder id, the hardware encoder when it is one, and a
/// notice.
type Resolved = (String, Option<HardwareRun>, Option<Notice>);

/// Hardware encoder tests running for an output that is about to start.
struct Testing {
    cancel: Cancel,
    handle: JoinHandle<Result<Resolved, StudioError>>,
    started: Instant,
    /// The profile the tests ran for; a result for another profile is not used.
    profile: Profile,
    /// [`Outputs::device_changes`] when the tests started.
    device: u64,
    /// Whether another output held a hardware encoder when the tests started.
    beside_encoder: bool,
}

/// Whether a destination with status `s` still tries to connect: it connects, is live, waits to
/// try again or waits for the network.
fn still_trying(s: &DestinationStatus) -> bool {
    matches!(
        s,
        DestinationStatus::Retrying { .. } | DestinationStatus::WaitingForNetwork { .. } | DestinationStatus::Connecting | DestinationStatus::Live
    )
}

/// The notice for an output whose stop returned `r`; none for a failure already shown while it
/// ran.
fn finish_notice(kind: OutputKind, target: &str, failed: bool, r: Result<(), PluginError>) -> Option<Notice> {
    match r {
        Ok(()) => Some(match kind {
            OutputKind::Recording => Notice::new("Recording saved to {path}", vec![("path", target.to_string())], false),
            OutputKind::Streaming => Notice::new("The stream ended", Vec::new(), false),
            OutputKind::Replay => Notice::new("The replay buffer stopped", Vec::new(), false),
        }),
        Err(e) if failed => {
            log::warn!("{} ended with an error: {e}", kind.noun());
            None
        }
        Err(e) => {
            let template = match kind {
                OutputKind::Recording => "The recording could not be finished: {error}",
                OutputKind::Streaming => "The stream could not be finished: {error}",
                OutputKind::Replay => "The replay buffer could not be stopped: {error}",
            };
            Some(Notice::new(template, vec![("error", e.to_string())], true))
        }
    }
}

/// What changed in [`Outputs::poll`].
#[derive(Default)]
pub(crate) struct Events {
    pub notices: Vec<Notice>,
    pub sinks_changed: bool,
    /// Outputs whose audio queue the mixer should drop.
    pub removed: Vec<u64>,
}

impl Outputs {
    pub(crate) fn any_active(&self) -> bool {
        !self.active.is_empty() || !self.stopping.is_empty()
    }

    pub(crate) fn video_sinks(&self) -> Vec<Arc<Fifo<Arc<PooledFrame>>>> {
        let mut kinds: Vec<_> = self.active.keys().copied().collect();
        kinds.sort_by_key(|k| *k == OutputKind::Streaming);
        kinds.iter().filter_map(|k| self.active.get(k)).map(|a| a.instance.video()).collect()
    }

    pub(crate) fn stats(&self) -> Vec<Arc<EncoderStats>> {
        self.active.values().map(|a| Arc::clone(&a.stats)).collect()
    }

    /// The stream keys held for this session, by name.
    pub(crate) fn held_keys(&self) -> &HashMap<String, String> {
        &self.held_keys
    }

    /// The fingerprint of each destination of the running or last stream, by id.
    pub(crate) fn stream_prints(&self) -> &HashMap<String, u64> {
        &self.stream_prints
    }

    /// The running stream's status; `None` while no stream runs.
    pub(crate) fn stream_status(&self) -> Option<OutputStatus> {
        self.active.get(&OutputKind::Streaming).map(|a| a.instance.status())
    }

    /// The destinations of the running stream when it was started with the profile `profile`.
    pub(crate) fn stream_destinations_of(&self, profile: &str) -> Option<Vec<DestinationState>> {
        let a = self.active.get(&OutputKind::Streaming)?;
        (self.stream_profile.as_deref() == Some(profile)).then(|| a.instance.destinations())
    }

    /// A reconnect event for each destination of the running stream, or of a stream stopped since
    /// the last call, that lost its connection since the last call, with that stream's start on
    /// the studio clock. Reads one counter per destination, and their states only when the
    /// counters moved.
    pub(crate) fn reconnects_now(&mut self) -> Vec<(crate::events::Kind, Tick)> {
        self.watch_reconnects();
        std::mem::take(&mut self.lost_connections)
    }

    /// Add the running stream's destinations that lost their connection since the last look to
    /// [`Outputs::lost_connections`].
    fn watch_reconnects(&mut self) {
        let Some(stream) = self.active.get(&OutputKind::Streaming) else {
            // Forgets the counts of a stream that ended.
            self.reconnects.observe(None, 0, Vec::new);
            return;
        };
        let seen = self.reconnects.observe(Some(stream.id), stream.instance.reconnects(), || stream.instance.destinations());
        self.lost_connections.extend(seen.into_iter().map(|kind| (kind, stream.start)));
    }

    /// Whether the loss of the running stream's destination `id`'s connection at `now` gets a
    /// notice. None does once the stream has stopped, or when the destination is not trying to
    /// connect again (it gave up, or was stopped).
    pub(crate) fn loss_notice(&mut self, id: &str, now: Tick) -> bool {
        let Some(stream) = self.active.get(&OutputKind::Streaming) else { return false };
        self.reconnect_notices.follow(Some(stream.id));
        let trying = stream.instance.destinations().iter().any(|d| d.id == id && still_trying(&d.status));
        trying && self.reconnect_notices.lost(id, now)
    }

    /// The name destination `id` had when the last stream started, or the id when it had none.
    pub(crate) fn stream_destination_name(&self, id: &str) -> String {
        self.stream_names.get(id).filter(|n| !n.is_empty()).map_or_else(|| id.to_string(), String::clone)
    }

    /// The running stream's destinations whose reconnect gets a notice now. Reads one counter
    /// per destination, and their states only when the counters moved after a loss notice.
    pub(crate) fn recovered_now(&mut self) -> Vec<String> {
        let Some(stream) = self.active.get(&OutputKind::Streaming) else {
            self.reconnect_notices.follow(None);
            return Vec::new();
        };
        self.reconnect_notices.follow(Some(stream.id));
        self.reconnect_notices.recovered(stream.instance.recoveries(), || stream.instance.destinations())
    }

    /// The studio clock's time of the first frame and sample of `kind`, while it runs.
    pub(crate) fn start_of(&self, kind: OutputKind) -> Option<Tick> {
        self.active.get(&kind).map(|a| a.start)
    }

    /// The encoder counters of `kind`, while it runs. They keep counting until its file is
    /// finished.
    pub(crate) fn stats_of(&self, kind: OutputKind) -> Option<Arc<EncoderStats>> {
        self.active.get(&kind).map(|a| Arc::clone(&a.stats))
    }

    pub(crate) fn state(&self, kind: OutputKind) -> Option<OutputState> {
        if let Some(a) = self.active.get(&kind) {
            return Some(OutputState {
                active: true,
                stopping: false,
                status: a.instance.status(),
                // A stream's destinations start and stop while it runs.
                target: if kind == OutputKind::Streaming { a.instance.target() } else { a.target.clone() },
                elapsed: a.started.elapsed(),
                bytes: a.stats.bytes.load(std::sync::atomic::Ordering::Relaxed),
                destinations: a.instance.destination_counts(),
            });
        }
        if let Some(t) = self.testing.get(&kind) {
            // Waiting for the hardware encoder tests: the next toggle cancels the start.
            return Some(OutputState {
                active: false,
                stopping: false,
                status: OutputStatus::Starting,
                target: String::new(),
                elapsed: t.started.elapsed(),
                bytes: 0,
                destinations: DestinationCounts::default(),
            });
        }
        self.stopping.iter().find(|s| s.kind == kind).map(|s| OutputState {
            active: false,
            stopping: true,
            status: OutputStatus::Running,
            target: s.target.clone(),
            elapsed: Duration::ZERO,
            bytes: 0,
            destinations: DestinationCounts::default(),
        })
    }

    /// Stop `kind` on a thread of its own (finishing a file can take seconds); the id of its
    /// audio queue.
    fn stop(&mut self, kind: OutputKind, failed: bool) -> Option<u64> {
        if kind == OutputKind::Streaming {
            // The last lost connections of a stream are seen before it goes.
            self.watch_reconnects();
        }
        let a = self.active.remove(&kind)?;
        let target = if kind == OutputKind::Streaming { a.instance.target() } else { a.target.clone() };
        if kind == OutputKind::Streaming {
            self.copy_failure = None;
            self.copy_path = None;
        }
        let before = a.instance.stream_copy();
        let copy_path = before.as_ref().map(|c| c.path.clone());
        let copy = before.is_some().then(|| (CopySlot::default(), std::mem::take(&mut self.copy_noticed), self.streams));
        if before.is_some() {
            // Shown while the copy finishes, until the stop's state replaces it.
            self.last_copy = before;
        }
        let slot = copy.as_ref().map(|(s, _, _)| Arc::clone(s));
        // The instance stays reachable here, so it can be stopped on this thread when no thread
        // can be started for it.
        let cell = Arc::new(Mutex::new(Some(a.instance)));
        let for_thread = Arc::clone(&cell);
        let handle = std::thread::Builder::new().name(format!("stop {}", kind.noun())).spawn(move || match for_thread.lock() {
            Ok(mut i) => i.take().map_or(Ok(()), |mut i| stop_instance(i.as_mut(), slot.as_ref())),
            Err(_) => Err(PluginError::Unavailable("the output was lost".into())),
        });
        match handle {
            Ok(handle) => self.stopping.push(Stopping { kind, target, failed, handle, copy, copy_path }),
            Err(e) => {
                log::error!("no thread to finish the {}: {e}", kind.noun());
                let slot = copy.as_ref().map(|(s, _, _)| s);
                let r = match cell.lock() {
                    Ok(mut i) => i.take().map_or(Ok(()), |mut i| stop_instance(i.as_mut(), slot)),
                    Err(_) => Err(PluginError::Unavailable("the output was lost".into())),
                };
                self.note_finish(kind, &target, !failed && r.is_ok());
                self.finished.push(finish_notice(kind, &target, failed, r));
                if let Some(n) = self.take_copy(copy) {
                    self.finished.push(Some(n));
                }
            }
        }
        Some(a.id)
    }

    /// Finished stops and failed outputs.
    pub(crate) fn poll(&mut self) -> Events {
        let mut ev = Events::default();
        // The tick has written the timelines of the copies that ended in the last poll, so only
        // the last ones are kept.
        let forgotten = self.ended_copies.len().saturating_sub(MAX_UNFINISHED);
        self.ended_copies.drain(..forgotten);
        let failed: Vec<(OutputKind, String, Option<HardwareRun>)> = self
            .active
            .iter()
            .filter_map(|(k, a)| match a.instance.status() {
                OutputStatus::Failed(e) => Some((*k, e, a.hardware.clone())),
                _ => None,
            })
            .collect();
        for (kind, e, hardware) in failed {
            let template = match kind {
                OutputKind::Recording => "The recording stopped: {error}",
                OutputKind::Streaming => "The stream stopped: {error}",
                OutputKind::Replay => "The replay buffer stopped: {error}",
            };
            ev.notices.push(Notice::new(template, vec![("error", e.to_string())], true));
            if let Some(hw) = hardware.filter(|hw| encoder_failed(hw.candidate, &e)) {
                self.probes.mark_failed(&hw.program, hw.candidate, e.clone());
                self.saved.save_again(&self.probes);
                if hw.auto {
                    let args = vec![("encoder", hw.candidate.name.to_string())];
                    ev.notices.push(Notice::new("{encoder} stopped working; the next start tries another encoder", args, true));
                }
            }
            if let Some(id) = self.stop(kind, true) {
                ev.removed.push(id);
                ev.sinks_changed = true;
            }
        }
        let (done, running): (Vec<_>, Vec<_>) = self.stopping.drain(..).partition(|s| s.handle.is_finished());
        self.stopping = running;
        ev.notices.extend(self.finished.drain(..).flatten());
        for s in done {
            match s.handle.join() {
                Ok(r) => {
                    self.note_finish(s.kind, &s.target, !s.failed && r.is_ok());
                    ev.notices.extend(finish_notice(s.kind, &s.target, s.failed, r));
                }
                Err(_) => {
                    self.note_finish(s.kind, &s.target, false);
                    let template = match s.kind {
                        OutputKind::Recording => "The recording could not be finished",
                        OutputKind::Streaming => "The stream could not be finished",
                        OutputKind::Replay => "The replay buffer could not be stopped",
                    };
                    ev.notices.push(Notice::new(template, Vec::new(), true));
                }
            }
            ev.notices.extend(self.take_copy(s.copy));
        }
        // A copy that stops while its stream goes on is shown once.
        if !self.copy_noticed
            && let Some(e) = self.active.get(&OutputKind::Streaming).and_then(|a| a.instance.stream_copy()).and_then(|c| c.failure)
        {
            self.copy_noticed = true;
            self.copy_failure = Some(e.clone());
            ev.notices.push(Notice::new("The stream's copy stopped; the stream goes on: {error}", vec![("error", e)], true));
        }
        let (saved, writing): (Vec<_>, Vec<_>) = self.saving.drain(..).partition(|s| s.handle.is_finished());
        self.saving = writing;
        for s in saved {
            ev.notices.push(save_notice(&s.path, s.handle.join()));
        }
        ev
    }

    /// Keep the state a stop thread put in `copy`'s slot as the last stream's copy, and its
    /// notice. The state of a stream that a newer stream followed is not kept.
    fn take_copy(&mut self, copy: Option<(CopySlot, bool, u64)>) -> Option<Notice> {
        let (slot, noticed, stream) = copy?;
        let state = slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take()?;
        let notice = copy_notice(&state, noticed);
        self.note_copy_ended(state, stream == self.streams);
        notice
    }

    /// Keep `state`, the copy of a stream that ended, among the ended copies, and as the last
    /// stream's copy when `latest`, that stream being the last one that started.
    fn note_copy_ended(&mut self, state: StreamCopyState, latest: bool) {
        self.ended_copies.push((state.path.clone(), state.finished, state.start_ns));
        if latest {
            self.last_copy = Some(state);
        }
    }

    /// [`StreamCopyState::start_ns`] of the copy of a stream at `path` that ended, the latest one
    /// with that file name; `None` when no such copy is known.
    pub(crate) fn copy_start(&self, path: &Path) -> Option<u64> {
        self.ended_copies.iter().rev().find(|(p, ..)| p == path).map(|(.., start)| *start)
    }

    /// Whether the copy of a stream at `path` was finished: `None` while its stream runs or is
    /// still stopping, else `Some(true)` when its file was finished and `Some(false)` when it was
    /// not, or when how it ended is no longer known. A copy that runs or stops is asked about
    /// before an earlier copy with the same file name that ended.
    pub(crate) fn copy_ended(&self, path: &Path) -> Option<bool> {
        let stopping = self.stopping.iter().any(|s| s.copy_path.as_deref() == Some(path));
        let running = self.active.contains_key(&OutputKind::Streaming) && self.copy_path.as_deref() == Some(path);
        if stopping || running {
            return None;
        }
        Some(self.ended_copies.iter().rev().find(|(p, ..)| p == path).is_some_and(|(_, finished, _)| *finished))
    }

    /// Why the running stream's copy stopped while the stream goes on, once [`Outputs::poll`] has
    /// seen it. It reads no state from the stream, so it can run each frame.
    pub(crate) fn running_copy_failure(&self) -> Option<&str> {
        self.copy_failure.as_deref()
    }

    /// Note that a stream starts: it has no copy failure shown yet, no copy file until the caller
    /// sets [`Outputs::copy_path`], and no earlier stream's copy is shown as its own.
    fn stream_started(&mut self) {
        self.copy_noticed = false;
        self.copy_failure = None;
        self.copy_path = None;
        self.last_copy = None;
        self.streams += 1;
    }

    /// The copy of the running stream, or else of the last stream that ended, when it had one.
    pub(crate) fn stream_copy(&self) -> Option<StreamCopyState> {
        match self.active.get(&OutputKind::Streaming) {
            Some(a) => a.instance.stream_copy(),
            None => self.last_copy.clone(),
        }
    }

    /// Replays still being written.
    pub(crate) fn saving(&self) -> usize {
        self.saving.len()
    }

    /// Whether a stopped `kind` writing to `target` is still finishing its file.
    pub(crate) fn finishing(&self, kind: OutputKind, target: &str) -> bool {
        self.stopping.iter().any(|s| s.kind == kind && s.target == target)
    }

    /// Note how a stopped `kind` writing to `target` ended: `clean` when it finished its file or
    /// stream without an error.
    fn note_finish(&mut self, kind: OutputKind, target: &str, clean: bool) {
        if clean {
            return;
        }
        if self.unfinished.len() >= MAX_UNFINISHED {
            self.unfinished.remove(0);
        }
        self.unfinished.push((kind, target.to_string()));
    }

    /// Whether a stopped `kind` writing to `target` ended without finishing its file or stream
    /// cleanly. The answer is given once: the output is forgotten.
    pub(crate) fn take_unfinished(&mut self, kind: OutputKind, target: &str) -> bool {
        let Some(i) = self.unfinished.iter().rposition(|(k, t)| *k == kind && t == target) else {
            return false;
        };
        self.unfinished.remove(i);
        true
    }

    /// A stopped `kind` writing to `target` that finishes its file when the returned sender is
    /// dropped, with an error when `fails`, for tests.
    #[cfg(test)]
    pub(crate) fn hold_finishing(&mut self, kind: OutputKind, target: &str, fails: bool) -> std::sync::mpsc::Sender<()> {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let handle = std::thread::spawn(move || {
            let _ = rx.recv();
            if fails { Err(PluginError::Failed("the disk is full".into())) } else { Ok(()) }
        });
        self.stopping.push(Stopping { kind, target: target.to_string(), failed: false, handle, copy: None, copy_path: None });
        tx
    }

    /// Wait for every held `kind` from [`Self::hold_finishing`] to finish and forget it, noting how
    /// it ended as [`Self::poll`] does, for tests.
    #[cfg(test)]
    pub(crate) fn finish_held(&mut self, kind: OutputKind) {
        let (held, rest): (Vec<_>, Vec<_>) = self.stopping.drain(..).partition(|s| s.kind == kind);
        self.stopping = rest;
        for s in held {
            let clean = matches!(s.handle.join(), Ok(Ok(()))) && !s.failed;
            self.note_finish(s.kind, &s.target, clean);
        }
    }

    /// An error while [`MAX_SAVES`] replays are being written: each one holds its packets in
    /// memory until its file is written.
    fn check_save_room(&self) -> Result<(), StudioError> {
        if self.saving.len() >= MAX_SAVES {
            return Err(StudioError::Output(format!("{MAX_SAVES} replays are already being saved; try again when one is done")));
        }
        Ok(())
    }

    /// Forget the hardware encoders' test results after the graphics device changed; the next
    /// start tests them again.
    pub(crate) fn forget_hardware_tests(&mut self) {
        // Tests still running for the old device write to the old cache, and their outputs
        // are tested again.
        self.probes = Arc::default();
        self.device_changes += 1;
        // The file is not read again, so every encoder is tested on the new device, and the next
        // save replaces the file.
        self.saved.read = Arc::new(AtomicBool::new(true));
        self.saved.epoch.fetch_add(1, Ordering::SeqCst);
    }

    /// Keep the hardware encoders' test results in `file` between sessions, or for the session
    /// only with none.
    pub(crate) fn keep_tests_in(&mut self, file: Option<PathBuf>) {
        // Tests already running save nothing, and the next tests read the new file.
        self.saved.epoch.fetch_add(1, Ordering::SeqCst);
        self.saved = Saved { path: file, ..Saved::default() };
    }

    /// Forget the hardware encoders' test results and remove the file that keeps them, so the next
    /// start tests every encoder again.
    pub(crate) fn retest_hardware(&mut self) -> Result<(), StudioError> {
        self.forget_hardware_tests();
        let Some(path) = &self.saved.path else { return Ok(()) };
        let _writing = self.saved.writing.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(StudioError::Output(format!("the saved hardware encoder tests at {} could not be removed: {e}", path.display()))),
        }
    }

    /// Finish everything before the app closes.
    pub(crate) fn stop_all_blocking(&mut self) {
        // A canceled test stops its ffmpeg at once.
        for t in self.testing.values() {
            t.cancel.cancel();
        }
        for (_, t) in self.testing.drain() {
            let _ = t.handle.join();
        }
        for h in self.canceled.drain(..) {
            let _ = h.join();
        }
        let mut ended = Vec::new();
        let mut copies = Vec::new();
        for (kind, mut a) in self.active.drain() {
            let target = if kind == OutputKind::Streaming { a.instance.target() } else { a.target.clone() };
            ended.push((kind, target, a.instance.stop().is_ok()));
            copies.extend(a.instance.stream_copy());
        }
        for s in std::mem::take(&mut self.stopping) {
            let clean = matches!(s.handle.join(), Ok(Ok(()))) && !s.failed;
            ended.push((s.kind, s.target, clean));
            // The app closes: the copy's notice would not be shown.
            let _ = self.take_copy(s.copy);
        }
        for c in copies {
            self.note_copy_ended(c, true);
        }
        for (kind, target, clean) in ended {
            self.note_finish(kind, &target, clean);
        }
        for s in self.saving.drain(..) {
            let _ = s.handle.join();
        }
    }
}

/// The notice for a replay whose save to `path` ended with `r` (`Err` when its thread panicked).
fn save_notice(path: &Path, r: std::thread::Result<Result<(), PluginError>>) -> Notice {
    match r {
        Ok(Ok(())) => Notice::new("Replay saved to {path}", vec![("path", path.display().to_string())], false),
        Ok(Err(e)) => Notice::new("The replay could not be saved: {error}", vec![("error", e.to_string())], true),
        Err(_) => Notice::new("The replay could not be saved", Vec::new(), true),
    }
}

/// Create an empty file for a new recording in `folder`, named from the local date and time, and
/// return its path. The file is created only under a name that no file has yet, so two recordings
/// started in the same second get different files. The folder is created when it is missing.
pub fn recording_path(folder: &Path, extension: &str) -> std::io::Result<PathBuf> {
    named_path(folder, "", extension)
}

/// [`recording_path`] for a saved replay: the name starts with `Replay `.
pub fn replay_path(folder: &Path) -> std::io::Result<PathBuf> {
    named_path(folder, "Replay ", "mkv")
}

/// [`recording_path`] for the copy of a stream: the name starts with `Stream `.
pub fn stream_copy_path(folder: &Path) -> std::io::Result<PathBuf> {
    named_path(folder, crate::recordings::STREAM_COPY_PREFIX, "mkv")
}

fn named_path(folder: &Path, prefix: &str, extension: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(folder)?;
    let stamp = format!("{prefix}{}", chrono::Local::now().format("%Y-%m-%d %H-%M-%S"));
    for n in 1..1000 {
        let path = if n == 1 { folder.join(format!("{stamp}.{extension}")) } else { folder.join(format!("{stamp} ({n}).{extension}")) };
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(_) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, "999 recordings already have this second's name"))
}

/// The folder recordings go to: the one in the settings, else the user's Videos folder.
pub fn recording_folder(configured: &str) -> PathBuf {
    let configured = configured.trim();
    if !configured.is_empty() {
        return PathBuf::from(configured);
    }
    dirs::video_dir().or_else(dirs::home_dir).unwrap_or_else(|| PathBuf::from("."))
}

/// What an output is started with.
#[derive(Debug, PartialEq)]
struct Plan {
    output_id: &'static str,
    /// The output plug-in's settings.
    settings: Settings,
    encoder_id: String,
    /// The video encoder's settings, from the profile.
    encoder_settings: Settings,
    rate: RateControl,
    audio_kbps: u32,
}

/// How profile `p` starts the output `kind`.
fn plan(p: &Profile, kind: OutputKind) -> Plan {
    match kind {
        OutputKind::Recording => Plan {
            output_id: "file",
            settings: Settings::new(),
            encoder_id: p.output.video_encoder.clone(),
            encoder_settings: p.output.video_encoder_settings(),
            rate: p.output.quality.quality_level().map_or(RateControl::Lossless, RateControl::Quality),
            audio_kbps: 192,
        },
        OutputKind::Streaming => Plan {
            output_id: "rtmp",
            // The destinations and their keys are added at the start ([`stream_settings`]).
            settings: Settings::new(),
            encoder_id: p.stream.video_encoder.clone(),
            encoder_settings: p.stream.video_encoder_settings(),
            rate: RateControl::Bitrate {
                kbps: p.stream.video_bitrate_kbps.clamp(100, 200_000),
                keyframe_s: p.stream.keyframe_interval_s.clamp(1, 20),
            },
            audio_kbps: p.stream.audio_bitrate_kbps,
        },
        OutputKind::Replay => {
            let r = p.replay.sanitized();
            Plan {
                output_id: "replay",
                settings: Settings::new().with("seconds", r.seconds).with("memory_mb", r.memory_mb),
                // The replay buffer runs the built-in encoder, with its settings from Settings › Output.
                encoder_id: REPLAY_ENCODER.into(),
                encoder_settings: p.output.encoder_settings.get(REPLAY_ENCODER).cloned().unwrap_or_default(),
                // The recording's quality; the built-in encoder has no lossless mode, so Lossless keeps High.
                rate: RateControl::Quality(p.output.quality.quality_level().or(RecordingQuality::High.quality_level()).unwrap_or(23)),
                audio_kbps: 192,
            }
        }
    }
}

/// The video encoder the replay buffer runs.
const REPLAY_ENCODER: &str = "h264";

/// How many streams of a profile have their automatic connection checks shown.
pub(crate) const MAX_CHECK_KEYS: usize = 16;

/// A key, unique to each stream started in this process, that its automatic connection checks are
/// kept under. A profile name can't contain `:`, so no profile's checks share it.
fn stream_check_key() -> String {
    static STREAMS: AtomicU64 = AtomicU64::new(0);
    format!("stream:{}", STREAMS.fetch_add(1, Ordering::Relaxed))
}

/// The stream output's settings for profile `p`: each destination with its server and its key,
/// the profile's key or one of `held`, the keys held for this session by name.
fn stream_settings(p: &Profile, held: &HashMap<String, String>) -> Result<Settings, StudioError> {
    p.stream.check().map_err(StudioError::Output)?;
    p.stream.reconnect.check().map_err(StudioError::Output)?;
    if !p.stream.destinations.iter().any(|d| d.enabled && !d.server.trim().is_empty()) {
        return Err(StudioError::Output("set a server in Settings › Stream first".into()));
    }
    let mut list = Vec::new();
    for d in &p.stream.destinations {
        let key = match &d.key {
            KeyRef::None => "",
            KeyRef::Profile => p.stream.key.as_str(),
            // A destination that is off needs no key until it is started.
            KeyRef::Held(name) => match held.get(name) {
                Some(key) => key.as_str(),
                None if !d.enabled => "",
                None => return Err(StudioError::Output(format!("the stream key `{name}` for destination `{}` is not set for this session", d.id))),
            },
        };
        list.push(serde_json::json!({ "id": d.id, "server": d.server.trim(), "key": key, "enabled": d.enabled, "service": d.service }));
    }
    let r = &p.stream.reconnect;
    let reconnect = serde_json::json!({
        "enabled": r.enabled, "first_delay_s": r.first_delay_s, "max_delay_s": r.max_delay_s, "max_tries": r.max_tries
    });
    Ok(Settings::new()
        .with("destinations", list)
        .with("reconnect", reconnect)
        .with("check_service", p.stream.check_service)
        .with("profile", p.name.as_str()))
}

impl Plan {
    /// The job that runs `encoder` with this plan's encoder settings, rate and audio bitrate.
    fn job(&self, encoder: Arc<dyn Encoder>, video: VideoFormat, ffmpeg: PathBuf, stats: Arc<EncoderStats>, start: Tick) -> OutputJob {
        OutputJob {
            video,
            sample_rate: SAMPLE_RATE,
            video_encoder: encoder,
            video_encoder_settings: self.encoder_settings.clone(),
            rate: self.rate,
            audio_bitrate_kbps: self.audio_kbps,
            ffmpeg,
            stats,
            start,
            captions: None,
        }
    }
}

/// Where the output `kind` can run encoders, when the ffmpeg program listed `offered` (empty when
/// that list could not be read): every output also runs the built-in ones.
fn encoder_env(offered: &[String]) -> EncoderEnv<'_> {
    EncoderEnv { builtin: true, ..EncoderEnv::ffmpeg(offered) }
}

/// Whether the output `kind` can start with `encoder`, when the ffmpeg program listed `offered`
/// (empty when that list could not be read). A stream runs the built-in encoders only.
fn check_encoder(encoder: &dyn Encoder, offered: &[String], kind: OutputKind) -> Result<(), StudioError> {
    let tab = match kind {
        OutputKind::Recording | OutputKind::Replay => "Output",
        OutputKind::Streaming => "Stream",
    };
    if encoder.kind() != EncoderKind::Video {
        return Err(StudioError::Output(format!("{} is not a video encoder; choose another encoder in Settings › {tab}", encoder.name())));
    }
    if kind == OutputKind::Streaming && encoder.ffmpeg().is_some() {
        return Err(StudioError::Output(format!(
            "{} runs inside ffmpeg, and streams run the built-in encoders; choose another encoder in Settings › Stream",
            encoder.name()
        )));
    }
    match encoder.availability(&encoder_env(offered)) {
        a if a.can_start() => Ok(()),
        Availability::NotRunHere => {
            Err(StudioError::Output(format!("{} cannot run in a {}; choose another encoder in Settings › {tab}", encoder.name(), kind.noun())))
        }
        _ => {
            let codec = encoder.ffmpeg().map_or(encoder.codec().id(), |f| f.ffmpeg_codec());
            Err(StudioError::Output(format!("this ffmpeg has no {codec} encoder; choose another encoder in Settings › {tab}")))
        }
    }
}

/// The stream's video encoder in profile `p`: its name and whether it can run, or why the stream
/// cannot start with it. A stream runs no ffmpeg, so the check needs no encoder list.
pub(crate) fn stream_encoder(registry: &studiocraft_plugin::Registry, p: &Profile) -> Result<(String, Availability), StudioError> {
    let encoder = registry.encoder(&p.stream.video_encoder)?;
    // The stream output's own check (`studiocraft_outputs::stream`): RTMP carries H.264 only.
    if encoder.kind() == EncoderKind::Video && !encoder.streams() {
        return Err(StudioError::Output(format!(
            "streams need an H.264 encoder, and {} is not one; choose another encoder in Settings › Stream",
            encoder.name()
        )));
    }
    check_encoder(encoder.as_ref(), &[], OutputKind::Streaming)?;
    Ok((encoder.name().to_string(), encoder.availability(&encoder_env(&[]))))
}

/// Whether an output's failure `error` came from the hardware encoder `candidate`: ffmpeg names the
/// encoder in its messages, or the error is one a missing device gives. A full session is not the
/// encoder's fault: it can have a free session at the next start.
fn encoder_failed(candidate: &Candidate, error: &str) -> bool {
    match craft_hwenc::classify(error) {
        craft_hwenc::Outcome::Busy(_) => false,
        craft_hwenc::Outcome::NoDevice(_) => true,
        // A built-in encoder's messages start with its name, which contains the candidate's.
        _ => error.contains(candidate.ffmpeg_codec) || error.contains(candidate.name),
    }
}

/// Tests the hardware encoders for an MKV recording or a stream: an encoder that also runs in this
/// process (NVENC, Media Foundation, VideoToolbox and VA-API H.264 and HEVC) is tested by opening it
/// there and encoding one frame at the output's size. For a recording the others are tested in the
/// ffmpeg program; a stream does not use them.
struct BuiltInFirst {
    ffmpeg: FfmpegProber,
    /// Encoders that run only inside ffmpeg are not used.
    in_process_only: bool,
    /// The recording's width and height, which a VA-API HEVC driver may refuse.
    size: (u32, u32),
    fps: (u32, u32),
    rate: RateControl,
}

impl BuiltInFirst {
    /// Test `encoder` on its own thread, waiting at most `limit` and the ffmpeg tests' timeout.
    fn test_within(&self, encoder: &'static dyn Hardware, cancel: &Cancel, limit: Duration) -> Outcome {
        // A test left running at its limit may still hold the encoder.
        if left_running(encoder.id()) {
            return Outcome::Busy(format!("the last test of {} has not ended", encoder.name()));
        }
        let (size, fps, rate) = (self.size, self.fps, self.rate);
        let limit = self.ffmpeg.timeout().map_or(limit, |t| t.min(limit));
        let end = TestEnd { id: encoder.id().to_string(), done: Arc::default() };
        let done = Arc::clone(&end.done);
        let work = move || {
            let _end = end;
            test_encoder(encoder, size, fps, rate)
        };
        let o = wait_on_thread(work, cancel, limit, &SystemClock);
        if matches!(o, Outcome::TimedOut(_) | Outcome::Canceled) {
            leave_running(encoder.id(), &done);
        }
        o
    }
}

/// The ids of the built-in encoders whose test was left running on its thread.
static LEFT_RUNNING: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Whether a test of built-in encoder `id` was left running and has not ended.
fn left_running(id: &str) -> bool {
    LEFT_RUNNING.lock().unwrap_or_else(std::sync::PoisonError::into_inner).iter().any(|r| r == id)
}

/// Record that the test of `id` was left running, unless it is `done` already.
fn leave_running(id: &str, done: &AtomicBool) {
    let mut left = LEFT_RUNNING.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if !done.load(Ordering::SeqCst) {
        left.push(id.to_string());
    }
}

/// The end of a built-in encoder's test on its thread, which takes it off [`LEFT_RUNNING`].
struct TestEnd {
    id: String,
    done: Arc<AtomicBool>,
}

impl Drop for TestEnd {
    fn drop(&mut self) {
        let mut left = LEFT_RUNNING.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        self.done.store(true, Ordering::SeqCst);
        left.retain(|r| *r != self.id);
    }
}

/// How often [`wait_on_thread`] looks for the test's outcome.
const IN_PROCESS_POLL: Duration = Duration::from_millis(5);

/// Run `work` on its own thread and wait for its outcome for at most `limit` on `clock`. A test
/// that does not end in time, or is canceled, is left to end on its thread, and its outcome is
/// dropped.
fn wait_on_thread(work: impl FnOnce() -> Outcome + Send + 'static, cancel: &Cancel, limit: Duration, clock: &dyn Clock) -> Outcome {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let spawned = std::thread::Builder::new().name("hardware-encoder-test".into()).spawn(move || {
        let _ = tx.send(work());
    });
    if let Err(e) = spawned {
        return Outcome::Failed(format!("the test could not start: {e}"));
    }
    let started = clock.now();
    loop {
        match rx.try_recv() {
            Ok(o) => return o,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => return Outcome::Failed("the test ended without a result".into()),
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
        if cancel.is_canceled() {
            return Outcome::Canceled;
        }
        let elapsed = clock.now().saturating_duration_since(started);
        if elapsed >= limit {
            return Outcome::TimedOut(limit);
        }
        clock.pause(IN_PROCESS_POLL.min(limit - elapsed));
    }
}

/// Open `encoder` at `size`, `fps` and `rate`, and encode one gray frame.
fn test_encoder(encoder: &dyn Hardware, size: (u32, u32), fps: (u32, u32), rate: RateControl) -> Outcome {
    let (width, height) = size;
    let params = VideoParams { width, height, fps_num: fps.0, fps_den: fps.1, rate };
    let mut session = match encoder.try_open(&params) {
        Ok(s) => s,
        // Not kept in the test cache: the next start tests again.
        Err(SessionError::Busy(why)) => return Outcome::Busy(why),
        Err(SessionError::Other(PluginError::Unavailable(why))) => return Outcome::NoDevice(why),
        Err(SessionError::Other(e)) => return Outcome::Failed(e.to_string()),
    };
    // A gray NV12 frame, allocated fallibly: an 8192 × 8192 output needs 96 MiB.
    let luma = (width as usize).saturating_mul(height as usize);
    let mut gray = Vec::new();
    if let Err(e) = gray.try_reserve_exact(luma.saturating_add(luma / 2)) {
        return Outcome::Failed(format!("no memory for a {width}x{height} test frame: {e}"));
    }
    gray.resize(luma, 16u8);
    gray.resize(luma + luma / 2, 128u8);
    let mut packets = Vec::new();
    if let Err(e) = session.encode(&gray, 0, &mut packets) {
        return Outcome::Failed(e.to_string());
    }
    if let Err(e) = session.flush(&mut packets) {
        return Outcome::Failed(e.to_string());
    }
    if packets.is_empty() { Outcome::Failed(format!("{} returned no packet for its test frame", encoder.name())) } else { Outcome::Works }
}

/// The path the tests and failures of the encoders that run in this process are kept under, apart
/// from the same candidates run in ffmpeg.
const IN_PROCESS: &str = "<in process>";

impl Prober for BuiltInFirst {
    fn program(&self) -> &Path {
        self.ffmpeg.program()
    }
    fn runs_in(&self, candidate: &'static Candidate) -> &Path {
        match hardware::for_candidate(candidate.id) {
            Some(_) => Path::new(IN_PROCESS),
            None => self.ffmpeg.program(),
        }
    }
    fn probe(&self, candidate: &'static Candidate, input: Input, cancel: &Cancel) -> Outcome {
        self.probe_within(candidate, input, cancel, Duration::MAX)
    }
    fn probe_within(&self, candidate: &'static Candidate, input: Input, cancel: &Cancel, limit: Duration) -> Outcome {
        match hardware::for_candidate(candidate.id).filter(|_| input == Input::Nv12) {
            Some(_) if cancel.is_canceled() => Outcome::Canceled,
            Some(encoder) => self.test_within(encoder, cancel, limit),
            None if self.in_process_only => Outcome::NoDevice(format!("{} runs only inside ffmpeg", candidate.name)),
            // Without ffmpeg the encoders that run in this process are still tested.
            None => match self.ffmpeg.probe_within(candidate, input, cancel, limit) {
                Outcome::FfmpegMissing(why) => Outcome::NoDevice(why),
                o => o,
            },
        }
    }
    fn timeout(&self) -> Option<Duration> {
        self.ffmpeg.timeout()
    }
    fn args(&self, candidate: &'static Candidate, input: Input) -> Vec<String> {
        match hardware::for_candidate(candidate.id).filter(|_| input == Input::Nv12) {
            // Kept apart from the ffmpeg encoder's results in the cache.
            Some(encoder) => vec![
                encoder.id().to_string(),
                format!("{:?}", self.rate),
                format!("{}/{}", self.fps.0, self.fps.1),
                format!("{}x{}", self.size.0, self.size.1),
            ],
            None => self.ffmpeg.args(candidate, input),
        }
    }
}

/// Whether the `hardware` choice of output `kind` runs the built-in hardware encoders in this
/// process: a stream, and an MKV recording that is not lossless, since they have no lossless mode.
fn runs_built_in(p: &Profile, kind: OutputKind, rate: RateControl) -> bool {
    match kind {
        OutputKind::Streaming => true,
        OutputKind::Recording => p.output.format == RecordingFormat::Mkv && rate != RateControl::Lossless,
        OutputKind::Replay => false,
    }
}

/// `resolved` with a hardware encoder that also runs in this process replaced by that built-in
/// encoder.
fn built_in_twin(resolved: Resolved) -> Resolved {
    match resolved {
        (id, Some(run), notice) => match hardware::for_candidate(&id) {
            Some(encoder) => (encoder.id().to_string(), Some(run), notice),
            None => (id, Some(run), notice),
        },
        other => other,
    }
}

/// What the `hardware` encoder runs: the encoder id, the hardware encoder when it is one, and a
/// notice when the automatic choice fell back to software. `preference` is the profile's
/// hardware encoder setting and `vendor` the graphics adapter's PCI vendor id. A stream takes
/// H.264 only; a recording takes the named encoder's codec, and the automatic choice uses H.264.
/// A `lossless` recording only uses an encoder that can run losslessly.
fn resolve_hardware(
    preference: &str,
    vendor: u32,
    stream: bool,
    lossless: bool,
    prober: &dyn Prober,
    cache: &ProbeCache,
    cancel: &Cancel,
) -> Result<(String, Option<HardwareRun>, Option<Notice>), StudioError> {
    let preference = Preference::parse(preference);
    let auto = preference == Preference::Auto;
    let codec = match &preference {
        Preference::Named(id) if !stream => craft_hwenc::candidate(id).map_or(craft_hwenc::Codec::H264, |c| c.codec),
        _ => craft_hwenc::Codec::H264,
    };
    let target = Target { vendor: Vendor::from_pci_id(vendor), os: Os::current(), codec, input: Input::Nv12, lossless };
    match craft_hwenc::resolve_within(&preference, target, prober, cache, cancel, Budget::new(PROBE_BUDGET)) {
        Ok(Choice::Hardware(c)) => Ok((c.id.to_string(), Some(HardwareRun { candidate: c, program: prober.runs_in(c).to_path_buf(), auto }), None)),
        Ok(Choice::Software { tried }) => {
            for (c, outcome) in &tried {
                log::info!("{} was not used: {outcome}", c.name);
            }
            // A stream falls back to the built-in H.264 encoder, a recording to x264.
            let (software, label) = if stream { (STREAM_SOFTWARE_ENCODER, "H.264 (built in)") } else { (SOFTWARE_ENCODER, "Software (x264)") };
            let notice = (auto && !tried.is_empty())
                .then(|| Notice::new("No hardware encoder passed its test; using {encoder}", vec![("encoder", label.to_string())], false));
            Ok((software.to_string(), None, notice))
        }
        Err(HwError::Canceled) => Err(StudioError::Output("the hardware encoder tests were canceled".into())),
        Err(e) => Err(StudioError::Output(format!("{e}; choose another hardware encoder in Settings › Video"))),
    }
}

/// The encoder the `hardware` choice falls back to.
const SOFTWARE_ENCODER: &str = "x264";
/// The encoder the `hardware` choice of a stream falls back to.
const STREAM_SOFTWARE_ENCODER: &str = REPLAY_ENCODER;

/// The saved settings of encoder `id` for the output `kind`.
fn encoder_settings(p: &Profile, kind: OutputKind, id: &str) -> Settings {
    let saved = if kind == OutputKind::Streaming { &p.stream.encoder_settings } else { &p.output.encoder_settings };
    saved.get(id).cloned().unwrap_or_default()
}

impl Studio {
    /// Keep the hardware encoders' test results between sessions in `file`, or for the session
    /// only with none. The file holds the results for one ffmpeg program, its version and the
    /// graphics driver; a file for others is replaced at the next save.
    pub fn keep_hardware_tests_in(&mut self, file: Option<PathBuf>) {
        self.outputs.keep_tests_in(file);
    }

    /// Forget the hardware encoders' test results, in the session and in the file that keeps
    /// them, so the next start tests every encoder again.
    pub fn retest_hardware_encoders(&mut self) -> Result<(), StudioError> {
        self.outputs.retest_hardware()
    }

    /// Hold `key` for this session under `name`, for the stream destinations whose key is
    /// [`KeyRef::Held`] with that name; an empty key forgets it. Held keys are never saved. A
    /// running stream keeps the keys it started with, and a destination started again with
    /// [`Studio::set_stream_destination`] takes the key held then.
    pub fn hold_stream_key(&mut self, name: &str, key: &str) {
        if key.is_empty() {
            self.outputs.held_keys.remove(name);
        } else {
            self.outputs.held_keys.insert(name.to_string(), key.to_string());
        }
    }

    /// Whether a key is held for this session under `name`.
    pub fn stream_key_held(&self, name: &str) -> bool {
        self.outputs.held_keys.contains_key(name)
    }

    /// The name of the profile the running stream started with; `None` while no stream runs or
    /// once that profile was removed.
    pub fn stream_profile(&self) -> Option<&str> {
        self.outputs.stream_profile.as_deref().filter(|_| self.outputs.active.contains_key(&OutputKind::Streaming))
    }

    /// The reconnect settings the running stream started with; `None` while no stream runs.
    pub fn stream_reconnect(&self) -> Option<studiocraft_model::ReconnectSettings> {
        self.outputs.stream_reconnect.filter(|_| self.outputs.active.contains_key(&OutputKind::Streaming))
    }

    /// Record that the profile `old` was renamed `new`, so a stream it started stays its stream.
    pub fn stream_profile_renamed(&mut self, old: &str, new: &str) {
        if self.outputs.stream_profile.as_deref() == Some(old) {
            self.outputs.stream_profile = Some(new.to_string());
        }
        if let Some(keys) = self.outputs.stream_check_keys.remove(old) {
            self.outputs.stream_check_keys.insert(new.to_string(), keys);
        }
        diagnose::checks().rename_profile(old, new);
    }

    /// Record that the profile `name` was removed, so a later profile with the same name does not
    /// take over a stream it started.
    pub fn stream_profile_removed(&mut self, name: &str) {
        if self.outputs.stream_profile.as_deref() == Some(name) {
            self.outputs.stream_profile = None;
        }
        for key in self.outputs.stream_check_keys.remove(name).unwrap_or_default() {
            diagnose::checks().forget_profile(&key);
        }
        diagnose::checks().forget_profile(name);
    }

    /// Each destination of the running stream, in the profile's order; empty while no stream runs.
    pub fn stream_destinations(&self) -> Vec<DestinationState> {
        self.outputs.active.get(&OutputKind::Streaming).map(|a| a.instance.destinations()).unwrap_or_default()
    }

    /// Starts a connection check of the profile's stream destination `id`
    /// ([`studiocraft_outputs::stream::diagnose`]), with the service's other hosts, and with the
    /// last failure of that destination in a running stream of this profile, when it has one and
    /// sends to the same server.
    pub fn check_stream_destination(&mut self, id: &str) -> Result<(), StudioError> {
        let Some(d) = self.profile.stream.destinations.iter().find(|d| d.id == id) else {
            return Err(StudioError::Output(format!("there is no stream destination `{id}`")));
        };
        if d.server.trim().is_empty() {
            return Err(StudioError::Output(format!("destination `{id}` has no server; set one in Settings › Stream")));
        }
        let own = self.stream_profile() == Some(self.profile.name.as_str());
        let live = if own { self.stream_destinations() } else { Vec::new() };
        let last = live.into_iter().find(|s| s.id == id && diagnose::same_server(&s.target, &d.server)).and_then(|s| match s.status {
            DestinationStatus::Failed(f)
            | DestinationStatus::Retrying { failure: f, .. }
            | DestinationStatus::WaitingForNetwork { failure: f, .. } => Some(f),
            _ => None,
        });
        let subject = diagnose::Subject {
            profile: self.profile.name.clone(),
            id: id.to_string(),
            server: d.server.trim().to_string(),
            service: studiocraft_model::services::service(&d.service),
            last,
            service_hosts: true,
            automatic: false,
            checked: None,
            wanted: None,
        };
        diagnose::checks().start(subject).map_err(|e| StudioError::Output(e.to_string()))
    }

    /// The connection check of the profile's stream destination `id`, running or finished; `None`
    /// when there has been none in this session.
    pub fn stream_destination_check(&self, id: &str) -> Option<diagnose::CheckState> {
        let d = self.profile.stream.destinations.iter().find(|d| d.id == id)?;
        let mut names = vec![self.profile.name.as_str()];
        // The automatic checks of this profile's streams are kept under each stream's key, also
        // after the stream ended.
        if let Some(keys) = self.outputs.stream_check_keys.get(&self.profile.name) {
            names.extend(keys.iter().map(String::as_str));
        }
        let service = studiocraft_model::services::service(&d.service).map_or("", |s| s.id);
        diagnose::checks().latest(&names, id, &d.server, service)
    }

    /// Start (`on`) or stop sending to the destination `id` of the running stream; the other
    /// destinations go on. While the stream's own profile is in use, a destination added to it
    /// after the stream started joins the stream, and one that is not sending starts with its
    /// server and key as they are now.
    pub fn set_stream_destination(&mut self, id: &str, on: bool) -> Result<(), StudioError> {
        if on {
            // A destination added to a full stream takes the place of one that ended, so the lost
            // connections of the destinations are seen while each is still listed.
            self.outputs.watch_reconnects();
        }
        let Some(a) = self.outputs.active.get(&OutputKind::Streaming) else {
            return Err(StudioError::Output("the stream is not running".into()));
        };
        if !on {
            return Ok(a.instance.stop_destination(id)?);
        }
        // While the profile the stream started with is in use, a destination starts as the profile
        // has it now: one added since joins the stream, and one that is not sending takes its
        // server, key and service as they are now.
        let own = self.outputs.stream_profile.as_deref() == Some(self.profile.name.as_str());
        if let Some(d) = self.profile.stream.destinations.iter().find(|d| own && d.id == id) {
            let Some(key) = crate::golive::destination_key(&self.profile, &self.outputs.held_keys, d) else {
                // One that is sending keeps its server and key, so it needs none of the profile's.
                // A key that is not held is otherwise an error, also for one asked to stop.
                if a.instance.destination_sending(id) {
                    return Ok(a.instance.start_destination(id, None)?);
                }
                let name = match &d.key {
                    KeyRef::Held(name) => name.as_str(),
                    _ => "",
                };
                return Err(StudioError::Output(format!("the stream key `{name}` for destination `{id}` is not set for this session")));
            };
            let setup =
                DestinationSetup { id: id.to_string(), server: d.server.trim().to_string(), key: key.to_string(), service: d.service.clone() };
            let (source, server, name, print) = (d.key.clone(), d.server.clone(), d.name.trim().to_string(), crate::golive::print(&d.server, key));
            // One that is sending keeps its server and key, and its record. One asked to stop that
            // has not stopped yet still shows as sending, and takes the profile's setup.
            if !a.instance.start_destination_as(&setup)? {
                return Ok(());
            }
            // It took the profile's setup now, and is recorded as it started.
            self.outputs.stream_keys.insert(id.to_string(), source);
            self.outputs.stream_servers.insert(id.to_string(), server);
            self.outputs.stream_names.insert(id.to_string(), name);
            self.outputs.stream_prints.insert(id.to_string(), print);
            return Ok(());
        }
        // A held key is looked up again, so one held after the stream started is used. Where a
        // key comes from is what it was when the stream started.
        let key = match self.outputs.stream_keys.get(id) {
            Some(KeyRef::Held(name)) => match self.outputs.held_keys.get(name) {
                Some(key) => Some(key.as_str()),
                None => return Err(StudioError::Output(format!("the stream key `{name}` for destination `{id}` is not set for this session"))),
            },
            _ => None,
        };
        // A destination that is sending or trying to keeps the key it has.
        let idle =
            a.instance.destinations().iter().any(|d| d.id == id && matches!(d.status, DestinationStatus::Stopped | DestinationStatus::Failed(_)));
        a.instance.start_destination(id, key)?;
        // A destination started again with a held key gets the fingerprint of that key, with the
        // server it had when the stream started.
        if idle
            && let Some(key) = key
            && let Some(server) = self.outputs.stream_servers.get(id)
        {
            let print = crate::golive::print(server, key);
            self.outputs.stream_prints.insert(id.to_string(), print);
        }
        Ok(())
    }

    /// Start the next try of the destination `id` of the running stream at once, when it waits to
    /// try again or waits for the network; whether it was waiting.
    pub fn retry_stream_destination(&self, id: &str) -> Result<bool, StudioError> {
        let Some(a) = self.outputs.active.get(&OutputKind::Streaming) else {
            return Err(StudioError::Output("the stream is not running".into()));
        };
        Ok(a.instance.retry_destination(id)?)
    }

    pub(crate) fn toggle_output(&mut self, kind: OutputKind) -> Result<(), StudioError> {
        if self.outputs.active.contains_key(&kind) || self.outputs.testing.contains_key(&kind) {
            self.stop_output(kind);
            Ok(())
        } else {
            self.start_output(kind)
        }
    }

    /// How many ffmpeg programs were asked for their encoder lists.
    #[cfg(test)]
    pub(crate) fn ffmpeg_lists_read(&self) -> usize {
        self.outputs.encoders.len()
    }

    /// Whether `kind` waits for its hardware encoder tests.
    #[cfg(test)]
    pub(crate) fn testing_hardware(&self, kind: OutputKind) -> bool {
        self.outputs.testing.contains_key(&kind)
    }

    /// Start the hardware encoder tests for `kind` on their own thread, each test with the
    /// arguments the output will run the encoder with.
    /// Test the hardware encoders for `kind`'s start: the `hardware` choice's, or `named` alone.
    fn test_hardware(&mut self, kind: OutputKind, planned: &Plan, program: PathBuf, offered: Vec<String>, named: Option<&'static str>) {
        let v = self.profile.video.sanitized();
        let fps = f64::from(v.fps_num) / f64::from(v.fps_den.max(1));
        let registry = Arc::clone(self.engine.registry());
        let args: HashMap<&'static str, Vec<String>> = craft_hwenc::CANDIDATES
            .iter()
            .filter_map(|c| {
                let encoder = registry.encoder(c.id).ok()?;
                let a = encoder.ffmpeg()?.ffmpeg_args(&encoder_settings(&self.profile, kind, c.id), planned.rate, fps).ok()?;
                Some((c.id, a))
            })
            .collect();
        let ffmpeg =
            FfmpegProber::new(program.clone(), offered).with_timeout(PROBE_TIMEOUT).with_encode_args(Box::new(move |c, _| args.get(c.id).cloned()));
        let adapter = self.gpu.as_ref().and_then(|g| g.adapter());
        let vendor = adapter.map_or(0, |a| a.vendor);
        let stream = kind == OutputKind::Streaming;
        let lossless = planned.rate == RateControl::Lossless;
        let built_in = runs_built_in(&self.profile, kind, planned.rate);
        let prober: Box<dyn Prober + Send> = if built_in {
            Box::new(BuiltInFirst {
                ffmpeg,
                in_process_only: stream,
                size: (v.output_width, v.output_height),
                fps: (v.fps_num, v.fps_den.max(1)),
                rate: planned.rate,
            })
        } else {
            Box::new(ffmpeg)
        };
        let preference = named.map_or_else(|| self.profile.output.hardware_encoder.clone(), String::from);
        // A test passed before another output took an encoder says nothing about a second session.
        let encoding = self.outputs.active.values().any(|a| a.hardware.is_some());
        let cache = if encoding { Arc::new(self.outputs.probes.without_outcomes()) } else { Arc::clone(&self.outputs.probes) };
        // The results kept between sessions are read into, and saved from, the session's cache.
        // Software only tests nothing, so it does not run `ffmpeg -version` for the file either.
        let software = Preference::parse(&preference) == Preference::Software;
        let file = if encoding || software { None } else { self.outputs.saved.path.clone() };
        let disk = file.map(|path| SavedRun {
            path,
            program,
            driver: tested_with(adapter.map(|a| format!("{:04x}:{:04x} {} {} {}", a.vendor, a.device, a.name, a.driver, a.driver_info))),
            read: Arc::clone(&self.outputs.saved.read),
            epoch: self.outputs.saved.epoch.load(Ordering::SeqCst),
            current: Arc::clone(&self.outputs.saved.epoch),
            writing: Arc::clone(&self.outputs.saved.writing),
            last: Arc::clone(&self.outputs.saved.last),
            known: Arc::clone(&self.outputs.saved.known),
        });
        let cancel = Cancel::new();
        let c = cancel.clone();
        let earlier = std::mem::take(&mut self.outputs.canceled);
        let probing = Arc::clone(&self.outputs.probing);
        let handle = std::thread::Builder::new().name("hardware-encoder-tests".into()).spawn(move || {
            for h in earlier {
                let _ = h.join();
            }
            let _one_at_a_time = probing.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let fingerprint = disk.as_ref().map(|d| d.read(&cache, &c));
            let resolved = resolve_hardware(&preference, vendor, stream, lossless, prober.as_ref(), &cache, &c);
            if let (Some(d), Some(f)) = (&disk, &fingerprint)
                && !c.is_canceled()
            {
                d.save(&cache, f);
            }
            if built_in { resolved.map(built_in_twin) } else { resolved }
        });
        match handle {
            Ok(handle) => {
                let device = self.outputs.device_changes;
                let t = Testing { cancel, handle, started: Instant::now(), profile: self.profile.clone(), device, beside_encoder: encoding };
                self.outputs.testing.insert(kind, t);
            }
            Err(e) => self.notices.push(Notice::new(TESTS_FAILED, vec![("error", e.to_string())], true)),
        }
    }

    /// Start each output whose hardware encoder tests ended, or show why it could not start. A
    /// test thread that panicked gives [`TESTS_FAILED`].
    pub(crate) fn finish_hardware_tests(&mut self) {
        self.outputs.canceled.retain(|h| !h.is_finished());
        let done: Vec<OutputKind> = self.outputs.testing.iter().filter(|(_, t)| t.handle.is_finished()).map(|(k, _)| *k).collect();
        for kind in done {
            let Some(t) = self.outputs.testing.remove(&kind) else { continue };
            let canceled = t.cancel.is_canceled();
            let encoding = self.outputs.active.values().any(|a| a.hardware.is_some());
            if !canceled && (t.profile != self.profile || t.device != self.outputs.device_changes || (encoding && !t.beside_encoder)) {
                // The settings or the graphics device changed while the tests ran, or another
                // output took an encoder since: test again.
                if let Err(e) = self.start_output(kind) {
                    self.notices.push(Notice::plain(e.to_string(), true));
                }
                continue;
            }
            match t.handle.join() {
                Ok(Ok(resolved)) if !canceled => {
                    self.outputs.resolved.insert(kind, resolved);
                    if let Err(e) = self.start_output(kind) {
                        self.outputs.resolved.remove(&kind);
                        self.notices.push(Notice::plain(e.to_string(), true));
                    }
                }
                Ok(Ok(_)) => {}
                Ok(Err(e)) => {
                    if !canceled {
                        self.notices.push(Notice::plain(e.to_string(), true));
                    }
                }
                Err(_) => self.notices.push(Notice::new(TESTS_FAILED, vec![("error", "the test thread stopped unexpectedly".to_string())], true)),
            }
        }
    }

    pub(crate) fn stop_output(&mut self, kind: OutputKind) {
        if let Some(t) = self.outputs.testing.remove(&kind) {
            // The thread ends at its next check of the flag; its result is dropped.
            t.cancel.cancel();
            self.outputs.canceled.push(t.handle);
        }
        if kind == OutputKind::Streaming {
            // The go-live check keeps how each destination did on this stream.
            self.record_destinations();
        }
        if let Some(id) = self.outputs.stop(kind, false) {
            self.mixer.send(MixerCmd::RemoveOutput { id });
            self.update_encode_target();
        }
        if kind == OutputKind::Streaming {
            // Lists the connections the stream lost since the last frame.
            self.note_reconnects();
        }
    }

    pub(crate) fn start_output(&mut self, kind: OutputKind) -> Result<(), StudioError> {
        if self.outputs.active.contains_key(&kind) || self.outputs.testing.contains_key(&kind) {
            return Ok(());
        }
        if self.outputs.stopping.iter().any(|s| s.kind == kind) {
            return Err(StudioError::Output(format!("the last {} is still being finished", kind.noun())));
        }
        if self.video.is_none() {
            return Err(StudioError::Output("nothing can be recorded or streamed without the graphics device".into()));
        }
        let mut planned = plan(&self.profile, kind);
        // The key the stream's automatic checks are kept under, recorded once the stream starts.
        let mut check_key = None;
        if kind == OutputKind::Streaming {
            let key = stream_check_key();
            planned.settings = stream_settings(&self.profile, &self.outputs.held_keys)?.with("profile", key.as_str());
            check_key = Some(key);
            self.outputs.stream_keys = self.profile.stream.destinations.iter().map(|d| (d.id.clone(), d.key.clone())).collect();
            self.outputs.stream_prints = crate::golive::prints(&self.profile, &self.outputs.held_keys);
            self.outputs.stream_servers = self.profile.stream.destinations.iter().map(|d| (d.id.clone(), d.server.clone())).collect();
            self.outputs.stream_names = self.profile.stream.destinations.iter().map(|d| (d.id.clone(), d.name.trim().to_string())).collect();
            self.outputs.stream_profile = Some(self.profile.name.clone());
            self.outputs.stream_reconnect = Some(self.profile.stream.reconnect);
        }
        let registry = Arc::clone(self.engine.registry());
        let mut encoder = registry.encoder(&planned.encoder_id)?;
        if kind == OutputKind::Streaming && encoder.ffmpeg().is_some() {
            // Refused before looking for the ffmpeg program, which a stream does not run.
            check_encoder(encoder.as_ref(), &[], kind)?;
        }
        let mut hardware = None;
        let program = if encoder.ffmpeg().is_some() || encoder.id() == HARDWARE {
            // A VA-API encoder chosen by name is tested too, to find the render node it encodes on.
            let vaapi = craft_hwenc::candidate(encoder.id()).filter(|c| c.api == craft_hwenc::Api::Vaapi).map(|c| c.id);
            // The `hardware` choice of an MKV recording can still run NVENC, Media Foundation,
            // VideoToolbox or VA-API without ffmpeg, and a VA-API encoder chosen by name can still
            // run VA-API.
            let tested = encoder.id() == HARDWARE || vaapi.is_some();
            let (program, missing) = if kind == OutputKind::Streaming {
                // A stream runs no ffmpeg, so none is looked for or run.
                (PathBuf::new(), Some(StudioError::Output("a stream runs the built-in encoders only".into())))
            } else {
                match ffmpeg::locate(&self.profile.output.ffmpeg_path).map_err(|e| StudioError::Output(e.to_string())) {
                    Ok(program) => (program, None),
                    Err(e) if tested && runs_built_in(&self.profile, kind, planned.rate) => (PathBuf::new(), Some(e)),
                    Err(e) => return Err(e),
                }
            };
            let offered = match missing {
                Some(_) => Vec::new(),
                None => self.outputs.encoders.entry(program.clone()).or_insert_with(|| ffmpeg::encoders(&program)).clone(),
            };
            if tested {
                let Some((id, run, notice)) = self.outputs.resolved.remove(&kind) else {
                    // The tests can take seconds each, so they run on their own thread, and
                    // `finish_hardware_tests` starts the output when they end.
                    self.test_hardware(kind, &planned, program, offered, vaapi);
                    return Ok(());
                };
                planned.encoder_settings = encoder_settings(&self.profile, kind, &id);
                encoder = registry.encoder(&id)?;
                planned.encoder_id = id;
                hardware = run;
                self.notices.extend(notice);
            }
            if let Some(e) = missing
                && encoder.ffmpeg().is_some()
            {
                return Err(e);
            }
            check_encoder(encoder.as_ref(), &offered, kind)?;
            if hardware.is_none()
                && let Some(candidate) = craft_hwenc::candidate(encoder.id())
            {
                // Chosen by name: a failure while it runs still counts against it.
                hardware = Some(HardwareRun { candidate, program: program.clone(), auto: false });
            }
            program
        } else {
            // A built-in encoder runs without the ffmpeg program.
            check_encoder(encoder.as_ref(), &[], kind)?;
            PathBuf::new()
        };
        let v = self.profile.video.sanitized();
        let stats = Arc::new(EncoderStats::default());
        let video = VideoFormat { width: v.output_width, height: v.output_height, fps_num: v.fps_num, fps_den: v.fps_den, pixel: PixelFormat::Nv12 };
        let start = self.clock.now();
        let mut job = planned.job(encoder, video, program, Arc::clone(&stats), start);
        if kind == OutputKind::Streaming {
            // Phrases live captions finish while the stream runs go out as closed captions.
            job.captions = Some(self.caption_feed());
        }
        let output = registry.output(planned.output_id)?;
        let mut settings = planned.settings;
        let reserved = match kind {
            OutputKind::Recording => {
                let folder = recording_folder(&self.profile.output.folder);
                let path = recording_path(&folder, self.profile.output.format.extension())
                    .map_err(|e| StudioError::Output(format!("no file for the recording could be made in {}: {e}", folder.display())))?;
                settings = settings.with("path", path.to_string_lossy().as_ref());
                Some(path)
            }
            OutputKind::Streaming | OutputKind::Replay => None,
        };
        // The stream's copy: a keyframe interval longer than a recording's, or a file that cannot be
        // made, leaves the stream without one.
        let copy_keyframe_s = match job.rate {
            RateControl::Bitrate { keyframe_s, .. } if keyframe_s > RECORDING_KEYFRAME_S => Some(keyframe_s),
            _ => None,
        };
        let copy = if kind == OutputKind::Streaming && self.profile.stream.save_copy {
            let folder = recording_folder(&self.profile.output.folder);
            if let Some(keyframe_s) = copy_keyframe_s {
                let args = vec![("limit", RECORDING_KEYFRAME_S.to_string()), ("interval", keyframe_s.to_string())];
                self.notices.push(Notice::new(
                    "The stream starts without a copy: a copy needs a keyframe at least every {limit} seconds, not every {interval}; set the Keyframe interval in Settings › Stream",
                    args,
                    true,
                ));
                None
            } else {
                match stream_copy_path(&folder) {
                    Ok(path) => {
                        settings = settings.with("copy", path.to_string_lossy().as_ref());
                        Some(path)
                    }
                    Err(e) => {
                        let args = vec![("folder", folder.display().to_string()), ("error", e.to_string())];
                        self.notices.push(Notice::new("The stream starts without a copy: no file could be made in {folder}: {error}", args, true));
                        None
                    }
                }
            }
        } else {
            None
        };
        // Audio tracks start with the output, so their first sample is the output's. A stream's
        // take is named after its copy, when it has one.
        self.audio_tracks_output_starting(kind, start, reserved.as_deref().or(copy.as_deref()));
        let instance = match output.start(&settings, job) {
            Ok(instance) => instance,
            Err(e) => {
                self.audio_tracks_output_failed(kind);
                for path in reserved.into_iter().chain(copy) {
                    let _ = std::fs::remove_file(path);
                }
                // A destination that started before the failure may have started a check.
                if let Some(key) = &check_key {
                    diagnose::checks().forget_profile(key);
                }
                return Err(e.into());
            }
        };
        self.outputs.next_id += 1;
        let id = self.outputs.next_id;
        self.mixer.send(MixerCmd::AddOutput { id, queue: instance.audio() });
        let target = instance.target();
        if kind == OutputKind::Streaming {
            self.outputs.stream_started();
            self.outputs.copy_path = instance.stream_copy().map(|c| c.path);
        }
        self.outputs.active.insert(kind, Active { id, instance, stats, started: Instant::now(), start, target, hardware });
        if let Some(key) = check_key {
            let keys = self.outputs.stream_check_keys.entry(self.profile.name.clone()).or_default();
            keys.push(key);
            if keys.len() > MAX_CHECK_KEYS {
                diagnose::checks().forget_profile(&keys.remove(0));
            }
        }
        self.update_encode_target();
        Ok(())
    }

    /// Write what the replay buffer holds to a new file in the recording folder, on a thread of
    /// its own; the buffer keeps running. [`Studio::tick`] reports the result as a notice.
    pub(crate) fn save_replay(&mut self) -> Result<(), StudioError> {
        let Some(active) = self.outputs.active.get(&OutputKind::Replay) else {
            return Err(StudioError::Output("the replay buffer is not running; start it first".into()));
        };
        self.outputs.check_save_room()?;
        let folder = recording_folder(&self.profile.output.folder);
        let path =
            replay_path(&folder).map_err(|e| StudioError::Output(format!("no file for the replay could be made in {}: {e}", folder.display())))?;
        let clip = match active.instance.replay() {
            Ok(clip) => clip,
            Err(e) => {
                let _ = std::fs::remove_file(&path);
                return Err(e.into());
            }
        };
        let for_thread = path.clone();
        let handle = std::thread::Builder::new().name("save replay".into()).spawn(move || {
            let r = clip.write(&for_thread);
            if r.is_err() {
                // The empty file reserved for the name, when the write did not get to it.
                let _ = std::fs::remove_file(&for_thread);
            }
            r
        });
        match handle {
            Ok(handle) => {
                self.outputs.saving.push(Saving { path, handle });
                Ok(())
            }
            Err(e) => {
                let _ = std::fs::remove_file(&path);
                Err(StudioError::Output(format!("no thread to save the replay: {e}")))
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use studiocraft_media::audio::PooledBlock;
    use studiocraft_plugin::Plugin;

    use super::*;

    /// A prober that answers every test with one outcome and counts the tests.
    struct FakeProber(craft_hwenc::Outcome, std::sync::atomic::AtomicUsize);

    impl Prober for FakeProber {
        fn program(&self) -> &Path {
            Path::new("fake-ffmpeg")
        }
        fn probe(&self, _: &'static Candidate, _: Input, cancel: &Cancel) -> craft_hwenc::Outcome {
            self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.0 == craft_hwenc::Outcome::Canceled {
                cancel.cancel();
            }
            self.0.clone()
        }
    }

    fn fake(outcome: craft_hwenc::Outcome) -> FakeProber {
        FakeProber(outcome, std::sync::atomic::AtomicUsize::new(0))
    }

    #[test]
    fn the_hardware_choice_takes_a_working_encoder_or_falls_back_to_x264_with_a_notice() {
        let cache = ProbeCache::new();
        let works = fake(craft_hwenc::Outcome::Works);
        let (id, run, notice) = resolve_hardware("auto", 0x10de, false, false, &works, &cache, &Cancel::new()).unwrap();
        let first = craft_hwenc::auto_order(Vendor::Nvidia, Os::current(), craft_hwenc::Codec::H264);
        match first.first() {
            Some(c) => {
                assert_eq!(id, c.id);
                assert_eq!(run, Some(HardwareRun { candidate: c, program: PathBuf::from("fake-ffmpeg"), auto: true }));
                assert!(notice.is_none());
            }
            None => assert_eq!(id, "x264", "no hardware API on this system"),
        }
        let none = fake(craft_hwenc::Outcome::NoDevice("no device".into()));
        let (id, run, notice) = resolve_hardware("auto", 0x10de, false, false, &none, &ProbeCache::new(), &Cancel::new()).unwrap();
        assert_eq!((id.as_str(), run), ("x264", None));
        if !first.is_empty() {
            assert_eq!(notice.map(|n| n.template), Some("No hardware encoder passed its test; using {encoder}"));
        }
        let (id, _, notice) = resolve_hardware("software", 0x10de, false, false, &works, &ProbeCache::new(), &Cancel::new()).unwrap();
        assert_eq!(id, "x264");
        assert!(notice.is_none(), "choosing software is not a fallback");
    }

    #[test]
    fn a_stream_falls_back_to_the_built_in_encoder_and_runs_none_inside_ffmpeg() {
        let none = fake(craft_hwenc::Outcome::NoDevice("no device".into()));
        let (id, run, notice) = resolve_hardware("auto", 0x10de, true, false, &none, &ProbeCache::new(), &Cancel::new()).unwrap();
        assert_eq!((id.as_str(), run), ("h264", None));
        if let Some(n) = notice {
            assert_eq!(n.text(), "No hardware encoder passed its test; using H.264 (built in)");
        }
        let (id, _, _) = resolve_hardware("software", 0x10de, true, false, &none, &ProbeCache::new(), &Cancel::new()).unwrap();
        assert_eq!(id, "h264");
        let prober = BuiltInFirst {
            ffmpeg: FfmpegProber::new(PathBuf::from("no-such-ffmpeg"), Vec::new()),
            in_process_only: true,
            size: (64, 48),
            fps: (30, 1),
            rate: RateControl::Bitrate { kbps: 1000, keyframe_s: 2 },
        };
        for c in craft_hwenc::CANDIDATES.iter().filter(|c| hardware::for_candidate(c.id).is_none()) {
            let outcome = prober.probe(c, Input::Nv12, &Cancel::new());
            assert!(matches!(&outcome, craft_hwenc::Outcome::NoDevice(why) if why.contains("runs only inside ffmpeg")), "{}: {outcome:?}", c.id);
        }
    }

    #[test]
    fn a_stream_sends_each_destination_with_its_key() {
        use studiocraft_model::Destination;
        let mut p = Profile::default();
        let Err(StudioError::Output(e)) = stream_settings(&p, &HashMap::new()) else { panic!("a stream without a server") };
        assert_eq!(e, "set a server in Settings › Stream first");
        p.stream.key = "profile-key".into();
        let d = |id: &str, server: &str, key: KeyRef, enabled: bool| Destination {
            id: id.into(),
            server: server.into(),
            key,
            enabled,
            ..Destination::default()
        };
        p.stream.destinations = vec![
            d("main", " rtmp://a/live ", KeyRef::Profile, true),
            d("b", "rtmp://b/live", KeyRef::Held("b".into()), true),
            d("c", "rtmp://c/live/name", KeyRef::None, true),
            d("d", "rtmp://d/live", KeyRef::Held("d".into()), false),
        ];
        p.stream.destinations[0].service = "twitch".into();
        let Err(StudioError::Output(e)) = stream_settings(&p, &HashMap::new()) else { panic!("a held key that is not set") };
        assert_eq!(e, "the stream key `b` for destination `b` is not set for this session");
        let held = HashMap::from([("b".to_string(), "held-key".to_string())]);
        let settings = stream_settings(&p, &held).unwrap();
        let want = serde_json::json!([
            { "id": "main", "server": "rtmp://a/live", "key": "profile-key", "enabled": true, "service": "twitch" },
            { "id": "b", "server": "rtmp://b/live", "key": "held-key", "enabled": true, "service": "" },
            { "id": "c", "server": "rtmp://c/live/name", "key": "", "enabled": true, "service": "" },
            { "id": "d", "server": "rtmp://d/live", "key": "", "enabled": false, "service": "" },
        ]);
        assert_eq!(settings.get("destinations"), Some(&want));
        assert_eq!(studiocraft_outputs::stream_targets(&settings).map(|t| t.len()), Ok(4));
        assert_eq!(studiocraft_outputs::stream_reconnect(&settings), Ok(p.stream.reconnect), "the reconnect settings go with them");
        let Ok(checks) = studiocraft_outputs::stream_checks(&settings) else { panic!("the check settings are read") };
        let services: Vec<(&str, &str)> = checks.services.iter().map(|(id, s)| (id.as_str(), s.id)).collect();
        assert_eq!((services, checks.service_hosts), (vec![("main", "twitch")], false), "the service hosts are off by default");
        p.stream.check_service = true;
        let on = stream_settings(&p, &held).unwrap();
        assert_eq!(studiocraft_outputs::stream_checks(&on).map(|c| c.service_hosts), Ok(true));
        p.stream.check_service = false;
        let mut slow = p.clone();
        slow.stream.reconnect.max_tries = 0;
        let Err(StudioError::Output(e)) = stream_settings(&slow, &held) else { panic!("reconnect settings out of range") };
        assert!(e.contains("reconnect tries"), "{e}");
        p.stream.destinations[1].id = "main".into();
        let Err(StudioError::Output(e)) = stream_settings(&p, &held) else { panic!("an id used twice") };
        assert!(e.contains("used twice"), "{e}");
        for on in &mut p.stream.destinations {
            on.enabled = false;
        }
        assert!(stream_settings(&p, &held).is_err(), "every destination is off");
    }

    #[test]
    fn a_named_hardware_encoder_that_fails_or_a_canceled_test_stops_the_start_with_a_clear_error() {
        let Some(c) = craft_hwenc::auto_order(Vendor::Nvidia, Os::current(), craft_hwenc::Codec::H264).first().copied() else { return };
        let busy = fake(craft_hwenc::Outcome::Busy("incompatible client key".into()));
        let Err(StudioError::Output(e)) = resolve_hardware(c.id, 0x10de, false, false, &busy, &ProbeCache::new(), &Cancel::new()) else {
            panic!("an error expected")
        };
        assert!(
            e.starts_with(&format!("{} did not pass its test encode", c.name)) && e.ends_with("choose another hardware encoder in Settings › Video"),
            "{e}"
        );
        let canceled = fake(craft_hwenc::Outcome::Canceled);
        let Err(StudioError::Output(e)) = resolve_hardware("auto", 0x10de, false, false, &canceled, &ProbeCache::new(), &Cancel::new()) else {
            panic!("an error expected")
        };
        assert_eq!(e, "the hardware encoder tests were canceled");
        assert_eq!(canceled.1.load(std::sync::atomic::Ordering::SeqCst), 1, "no test runs after the cancel");
    }

    #[test]
    fn lossless_recordings_and_streams_only_take_encoders_that_can_do_them() {
        let works = fake(craft_hwenc::Outcome::Works);
        // An AMD adapter puts AMF or VA-API first, but neither runs losslessly.
        let (id, run, _) = resolve_hardware("auto", 0x1002, false, true, &works, &ProbeCache::new(), &Cancel::new()).unwrap();
        match run {
            Some(run) => assert!(run.candidate.lossless() && run.candidate.id == id, "{id}"),
            None => assert_eq!(id, "x264"),
        }
        let Some(hevc) = craft_hwenc::candidate("nvenc_hevc").filter(|c| c.runs_on(Os::current())) else { return };
        let (id, _, _) = resolve_hardware(hevc.id, 0x10de, false, false, &works, &ProbeCache::new(), &Cancel::new()).unwrap();
        assert_eq!(id, "nvenc_hevc", "a recording takes HEVC");
        let Err(StudioError::Output(e)) = resolve_hardware(hevc.id, 0x10de, true, false, &works, &ProbeCache::new(), &Cancel::new()) else {
            panic!("a stream takes H.264 only")
        };
        assert!(e.starts_with("NVIDIA NVENC HEVC writes another codec"), "{e}");
        let amf = craft_hwenc::candidate("amf_h264").filter(|c| c.runs_on(Os::current()));
        if let Some(amf) = amf {
            let Err(StudioError::Output(e)) = resolve_hardware(amf.id, 0x1002, false, true, &works, &ProbeCache::new(), &Cancel::new()) else {
                panic!("AMF has no lossless mode")
            };
            assert!(e.starts_with("AMD AMF H.264 cannot record losslessly"), "{e}");
        }
    }

    #[test]
    fn only_failures_from_the_encoder_count_against_it() {
        let nvenc = craft_hwenc::candidate("nvenc_h264").unwrap();
        assert!(encoder_failed(nvenc, "ffmpeg stopped: [h264_nvenc @ 0000021] Failed locking bitstream buffer"));
        assert!(encoder_failed(nvenc, "ffmpeg stopped: Cannot load nvcuda.dll"));
        assert!(!encoder_failed(nvenc, "ffmpeg stopped: Connection reset by peer"), "a network error is not the encoder's");
        let busy = "ffmpeg stopped: [h264_nvenc @ 0000021] OpenEncodeSessionEx failed: incompatible client key (21)";
        assert!(!encoder_failed(nvenc, busy), "a full session is tried again at the next start");
        assert!(encoder_failed(nvenc, "NVIDIA NVENC H.264 (built in) stopped: encoding a picture failed (8)"), "the built-in encoder's failure");
    }

    #[test]
    fn an_mkv_recording_tests_nvenc_in_this_process_and_runs_the_built_in_encoder() {
        let run = |id: &'static str| HardwareRun { candidate: craft_hwenc::candidate(id).unwrap(), program: PathBuf::from("ffmpeg"), auto: true };
        assert_eq!(built_in_twin(("nvenc_h264".into(), Some(run("nvenc_h264")), None)).0, "nvenc_native_h264");
        assert_eq!(built_in_twin(("nvenc_hevc".into(), Some(run("nvenc_hevc")), None)).0, "nvenc_native_hevc");
        assert_eq!(built_in_twin(("nvenc_av1".into(), Some(run("nvenc_av1")), None)).0, "nvenc_native_av1");
        assert_eq!(built_in_twin(("mf_h264".into(), Some(run("mf_h264")), None)).0, "mf_native_h264");
        assert_eq!(built_in_twin(("mf_hevc".into(), Some(run("mf_hevc")), None)).0, "mf_native_hevc");
        assert_eq!(built_in_twin(("vt_h264".into(), Some(run("vt_h264")), None)).0, "vt_native_h264");
        assert_eq!(built_in_twin(("vt_hevc".into(), Some(run("vt_hevc")), None)).0, "vt_native_hevc");
        assert_eq!(built_in_twin(("vaapi_h264".into(), Some(run("vaapi_h264")), None)).0, "vaapi_native_h264");
        assert_eq!(built_in_twin(("vaapi_hevc".into(), Some(run("vaapi_hevc")), None)).0, "vaapi_native_hevc");
        assert_eq!(built_in_twin(("vaapi_av1".into(), Some(run("vaapi_av1")), None)).0, "vaapi_native_av1");
        assert_eq!(built_in_twin(("amf_h264".into(), Some(run("amf_h264")), None)).0, "amf_h264", "no built-in twin");
        assert_eq!(built_in_twin(("x264".into(), None, None)).0, "x264");

        // The ffmpeg program does not exist: only the in-process test can say anything else.
        let ffmpeg = FfmpegProber::new("missing-ffmpeg-for-the-test", Vec::new());
        let p = BuiltInFirst { ffmpeg, in_process_only: false, size: (1920, 1080), fps: (30, 1), rate: RateControl::Quality(23) };
        let nvenc = craft_hwenc::candidate("nvenc_h264").unwrap();
        let amf = craft_hwenc::candidate("amf_h264").unwrap();
        match p.probe(nvenc, Input::Nv12, &Cancel::new()) {
            Outcome::Works => {}
            Outcome::NoDevice(why) => assert!(why.starts_with("NVIDIA NVENC H.264 (built in) cannot run on this system"), "{why}"),
            other => panic!("{other:?}"),
        }
        // Media Foundation is tested in this process too.
        let mf = craft_hwenc::candidate("mf_hevc").unwrap();
        match p.probe(mf, Input::Nv12, &Cancel::new()) {
            Outcome::Works => {}
            Outcome::NoDevice(why) => assert!(why.starts_with("Windows Media Foundation HEVC (built in) cannot "), "{why}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(p.runs_in(mf), Path::new(IN_PROCESS));
        // So is VideoToolbox.
        let vt = craft_hwenc::candidate("vt_h264").unwrap();
        match p.probe(vt, Input::Nv12, &Cancel::new()) {
            Outcome::Works => {}
            Outcome::NoDevice(why) => assert!(why.starts_with("Apple VideoToolbox H.264 (built in) cannot "), "{why}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(p.runs_in(vt), Path::new(IN_PROCESS));
        // So is VA-API.
        let vaapi = craft_hwenc::candidate("vaapi_h264").unwrap();
        match p.probe(vaapi, Input::Nv12, &Cancel::new()) {
            Outcome::Works => {}
            Outcome::NoDevice(why) => assert!(why.starts_with("VA-API H.264 (built in) cannot "), "{why}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(p.runs_in(vaapi), Path::new(IN_PROCESS));
        // AMF is tested in ffmpeg; a missing ffmpeg does not stop the tests before NVENC.
        assert!(matches!(p.probe(amf, Input::Nv12, &Cancel::new()), Outcome::NoDevice(why) if why.contains("was not found")));
        let canceled = Cancel::new();
        canceled.cancel();
        assert_eq!(p.probe(nvenc, Input::Nv12, &canceled), Outcome::Canceled);
        assert_ne!(p.args(nvenc, Input::Nv12), p.ffmpeg.args(nvenc, Input::Nv12), "kept apart in the cache");
        assert_eq!(p.args(amf, Input::Nv12), p.ffmpeg.args(amf, Input::Nv12));
        let ffmpeg = FfmpegProber::new("missing-ffmpeg-for-the-test", Vec::new());
        let at_60 = BuiltInFirst { ffmpeg, in_process_only: false, size: (1920, 1080), fps: (60, 1), rate: RateControl::Quality(23) };
        assert_ne!(at_60.args(nvenc, Input::Nv12), p.args(nvenc, Input::Nv12), "another frame rate is tested again");
        let ffmpeg = FfmpegProber::new("missing-ffmpeg-for-the-test", Vec::new());
        let at_720p = BuiltInFirst { ffmpeg, in_process_only: false, size: (1280, 720), fps: (30, 1), rate: RateControl::Quality(23) };
        let hevc = craft_hwenc::candidate("vaapi_hevc").unwrap();
        assert_ne!(at_720p.args(hevc, Input::Nv12), p.args(hevc, Input::Nv12), "another size is tested again");
        // A failure of the built-in encoder while it records does not stop ffmpeg's NVENC.
        assert_eq!((p.runs_in(nvenc), p.runs_in(amf)), (Path::new(IN_PROCESS), p.program()));
        let cache = ProbeCache::new();
        cache.mark_failed(p.runs_in(nvenc), nvenc, "lost its last frames");
        assert!(matches!(cache.get(&p, nvenc, Input::Nv12), Some(Outcome::Failed(_))));
        assert_eq!(cache.get(&p.ffmpeg, nvenc, Input::Nv12), None);
    }

    /// An output that failed while it ran.
    struct FailedOutput;

    impl OutputInstance for FailedOutput {
        fn video(&self) -> Arc<Fifo<Arc<PooledFrame>>> {
            Arc::new(Fifo::new(1))
        }
        fn audio(&self) -> Arc<Fifo<PooledBlock>> {
            Arc::new(Fifo::new(1))
        }
        fn status(&self) -> OutputStatus {
            OutputStatus::Failed("ffmpeg stopped: [h264_mf @ 01] could not set output type".into())
        }
        fn target(&self) -> String {
            String::new()
        }
        fn stop(&mut self) -> Result<(), PluginError> {
            // An error, as from a failed ffmpeg: its notice was shown while it ran.
            Err(PluginError::Failed("ffmpeg stopped".into()))
        }
    }

    #[test]
    fn a_new_graphics_device_forgets_the_hardware_encoder_tests() {
        let mut o = Outputs::default();
        let nvenc = craft_hwenc::candidate("nvenc_h264").unwrap();
        let prober = FfmpegProber::new("ffmpeg", Vec::new());
        o.probes.mark_failed(Path::new("ffmpeg"), nvenc, "no device");
        let old = Arc::clone(&o.probes);
        o.forget_hardware_tests();
        assert_eq!(o.probes.get(&prober, nvenc, craft_hwenc::Input::Nv12), None);
        // A test still running for the old device writes to the old cache.
        old.mark_failed(Path::new("ffmpeg"), nvenc, "old device");
        assert_eq!(o.probes.get(&prober, nvenc, craft_hwenc::Input::Nv12), None);
        assert_eq!(o.device_changes, 1, "tests started before the change are run again");
    }

    /// The [`SavedRun`] a test thread of `o` gets, as [`Studio::test_hardware`] makes it.
    fn saved_run(o: &Outputs, driver: &str) -> SavedRun {
        SavedRun { driver: driver.into(), ..saved_run_at(o, o.saved.path.clone().unwrap()) }
    }

    /// [`saved_run`] for the file at `path`.
    fn saved_run_at(o: &Outputs, path: PathBuf) -> SavedRun {
        SavedRun {
            path,
            program: PathBuf::from("fake-ffmpeg"),
            driver: "10de:2684 GPU 591.44".into(),
            read: Arc::clone(&o.saved.read),
            epoch: o.saved.epoch.load(Ordering::SeqCst),
            current: Arc::clone(&o.saved.epoch),
            writing: Arc::clone(&o.saved.writing),
            last: Arc::clone(&o.saved.last),
            known: Arc::clone(&o.saved.known),
        }
    }

    #[test]
    fn hardware_encoder_tests_are_kept_between_sessions_until_they_are_retested() {
        let dir = std::env::temp_dir().join(format!("studiocraft-hwtests-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join(HARDWARE_TESTS_FILE);
        let nvenc = craft_hwenc::candidate("nvenc_h264").unwrap();
        let works = fake(craft_hwenc::Outcome::Works);
        let cancel = Cancel::new();
        let mut o = Outputs { saved: Saved { path: Some(file.clone()), ..Saved::default() }, ..Outputs::default() };
        let run = saved_run(&o, "10de:2684 GPU 591.44");
        let f = run.read(&o.probes, &cancel);
        assert_eq!(f.also, [PathBuf::from(IN_PROCESS)]);
        o.probes.outcome(&works, nvenc, Input::Nv12, &cancel);
        run.save(&o.probes, &f);
        assert!(file.is_file());
        // The next session reads the file once, before its first tests.
        let next = Outputs { saved: Saved { path: Some(file.clone()), ..Saved::default() }, ..Outputs::default() };
        let run = saved_run(&next, "10de:2684 GPU 591.44");
        run.read(&next.probes, &cancel);
        assert_eq!(next.probes.get(&works, nvenc, Input::Nv12), Some(craft_hwenc::Outcome::Works));
        next.probes.clear();
        run.read(&next.probes, &cancel);
        assert_eq!(next.probes.get(&works, nvenc, Input::Nv12), None, "the file is read once per session");
        // Another driver does not read it.
        let other = Outputs { saved: Saved { path: Some(file.clone()), ..Saved::default() }, ..Outputs::default() };
        saved_run(&other, "10de:2684 GPU 595.02").read(&other.probes, &cancel);
        assert_eq!(other.probes.get(&works, nvenc, Input::Nv12), None);
        // After a device change the encoders are tested again, though the file is for this driver.
        let mut changed = Outputs { saved: Saved { path: Some(file.clone()), ..Saved::default() }, ..Outputs::default() };
        changed.forget_hardware_tests();
        saved_run(&changed, "10de:2684 GPU 591.44").read(&changed.probes, &cancel);
        assert_eq!(changed.probes.get(&works, nvenc, Input::Nv12), None, "a device change is tested again");
        // A test thread started before a retest saves nothing, and the file is gone.
        let stale = saved_run(&o, "10de:2684 GPU 591.44");
        o.retest_hardware().unwrap();
        assert!(!file.exists());
        assert_eq!(o.probes.get(&works, nvenc, Input::Nv12), None);
        stale.save(&o.probes, &f);
        assert!(!file.exists(), "a test from before the retest does not write the file again");
        o.retest_hardware().unwrap();
        // Nor does one from before the file changed.
        let stale = saved_run(&o, "10de:2684 GPU 591.44");
        o.keep_tests_in(None);
        stale.save(&o.probes, &f);
        assert!(!file.exists(), "a test from before the file changed does not write the old file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_encoder_that_fails_while_it_runs_is_taken_out_of_the_saved_tests() {
        let dir = std::env::temp_dir().join(format!("studiocraft-hwfailed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join(HARDWARE_TESTS_FILE);
        let nvenc = craft_hwenc::candidate("nvenc_h264").unwrap();
        let works = fake(craft_hwenc::Outcome::Works);
        let cancel = Cancel::new();
        let o = Outputs { saved: Saved { path: Some(file.clone()), ..Saved::default() }, ..Outputs::default() };
        let run = saved_run(&o, "10de:2684 GPU 591.44");
        let f = run.read(&o.probes, &cancel);
        o.probes.outcome(&works, nvenc, Input::Nv12, &cancel);
        run.save(&o.probes, &f);
        o.probes.mark_failed(works.runs_in(nvenc), nvenc, "lost its device");
        o.saved.save_again(&o.probes);
        let next = ProbeCache::new();
        assert_eq!(next.load(&file, &f), Loaded::Read(0));
        assert_eq!(next.get(&works, nvenc, Input::Nv12), None, "the next session tests it again");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_saved_tests_are_for_one_build_of_the_app() {
        let with = tested_with(Some("10de:2684 GPU 591.44".into()));
        assert_eq!(with, format!("10de:2684 GPU 591.44; StudioCraft {}", env!("CARGO_PKG_VERSION")));
        assert!(tested_with(None).ends_with(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn a_replaced_ffmpeg_program_is_tested_again_in_the_same_session() {
        let dir = std::env::temp_dir().join(format!("studiocraft-hwprogram-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A file that does not run: `-version` gives no line, and its size tells the versions apart.
        let program = dir.join("ffmpeg");
        std::fs::write(&program, b"one").unwrap();
        let nvenc = craft_hwenc::candidate("nvenc_h264").unwrap();
        let works = fake(craft_hwenc::Outcome::Works);
        let cancel = Cancel::new();
        let o = Outputs::default();
        let run = SavedRun { program: program.clone(), ..saved_run_at(&o, dir.join(HARDWARE_TESTS_FILE)) };
        let first = run.read(&o.probes, &cancel);
        o.probes.outcome(&works, nvenc, Input::Nv12, &cancel);
        assert_eq!(run.read(&o.probes, &cancel), first, "the same program gives the same fingerprint");
        assert_eq!(o.probes.get(&works, nvenc, Input::Nv12), Some(Outcome::Works));
        std::fs::write(&program, b"a newer build").unwrap();
        assert_ne!(run.read(&o.probes, &cancel), first);
        assert_eq!(o.probes.get(&works, nvenc, Input::Nv12), None, "the old program's results are forgotten");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A clock that moves only when it is paused.
    struct StepClock(Instant, Mutex<Duration>);

    impl Clock for StepClock {
        fn now(&self) -> Instant {
            self.0 + *self.1.lock().unwrap()
        }
        fn pause(&self, d: Duration) {
            *self.1.lock().unwrap() += d;
        }
    }

    #[test]
    fn an_in_process_encoder_test_that_never_ends_is_left_at_its_limit() {
        let clock = StepClock(Instant::now(), Mutex::new(Duration::ZERO));
        let (release, stuck) = std::sync::mpsc::channel::<()>();
        let work = move || {
            let _ = stuck.recv();
            Outcome::Works
        };
        assert_eq!(wait_on_thread(work, &Cancel::new(), Duration::from_secs(5), &clock), Outcome::TimedOut(Duration::from_secs(5)));
        assert_eq!(*clock.1.lock().unwrap(), Duration::from_secs(5));
        drop(release);
        let canceled = Cancel::new();
        canceled.cancel();
        let (_release, stuck) = std::sync::mpsc::channel::<()>();
        let work = move || {
            let _ = stuck.recv();
            Outcome::Works
        };
        assert_eq!(wait_on_thread(work, &canceled, Duration::from_secs(5), &clock), Outcome::Canceled);
        // Until a test left running ends, the same encoder is not tested again.
        let end = TestEnd { id: "test-only-encoder".into(), done: Arc::default() };
        leave_running("test-only-encoder", &end.done);
        assert!(left_running("test-only-encoder"));
        assert!(!left_running("another-test-only-encoder"));
        drop(end);
        assert!(!left_running("test-only-encoder"));
        // A test that ended before it was left is not recorded.
        let end = TestEnd { id: "test-only-encoder".into(), done: Arc::default() };
        let done = Arc::clone(&end.done);
        drop(end);
        leave_running("test-only-encoder", &done);
        assert!(!left_running("test-only-encoder"));
        assert_eq!(wait_on_thread(|| Outcome::Works, &Cancel::new(), Duration::MAX, &clock), Outcome::Works);
        assert!(matches!(wait_on_thread(|| panic!("a driver fault"), &Cancel::new(), Duration::MAX, &clock), Outcome::Failed(_)));
    }

    #[test]
    fn shutting_down_cancels_and_waits_for_the_hardware_encoder_tests() {
        let mut o = Outputs::default();
        let cancel = Cancel::new();
        let seen = cancel.clone();
        let ended = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done = Arc::clone(&ended);
        let handle = std::thread::spawn(move || {
            // Stands for a test encode that runs until it is canceled.
            for _ in 0..10_000_000 {
                if seen.is_canceled() {
                    done.store(true, std::sync::atomic::Ordering::SeqCst);
                    return Err(StudioError::Output("canceled".into()));
                }
                std::thread::yield_now();
            }
            Err(StudioError::Output("never canceled".into()))
        });
        let t = Testing { cancel, handle, started: Instant::now(), profile: Profile::default(), device: 0, beside_encoder: false };
        o.testing.insert(OutputKind::Recording, t);
        // A test canceled earlier, still finishing.
        let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let f = Arc::clone(&finished);
        o.canceled.push(std::thread::spawn(move || {
            for _ in 0..100_000 {
                std::thread::yield_now();
            }
            f.store(true, std::sync::atomic::Ordering::SeqCst);
            Err(StudioError::Output("canceled".into()))
        }));
        o.stop_all_blocking();
        assert!(ended.load(std::sync::atomic::Ordering::SeqCst), "the test ended through its cancel before the shutdown returned");
        assert!(finished.load(std::sync::atomic::Ordering::SeqCst), "the shutdown waited for the canceled test");
        assert!(o.canceled.is_empty());
        assert!(o.testing.is_empty());
        assert!(o.state(OutputKind::Recording).is_none());
    }

    #[test]
    fn a_hardware_encoder_that_fails_while_recording_is_skipped_for_the_session() {
        let mf = craft_hwenc::candidate("mf_h264").unwrap();
        let mut o = Outputs::default();
        let program = PathBuf::from("fake-ffmpeg");
        let hardware = Some(HardwareRun { candidate: mf, program: program.clone(), auto: true });
        let active = Active {
            id: 1,
            instance: Box::new(FailedOutput),
            stats: Arc::new(EncoderStats::default()),
            started: Instant::now(),
            start: Tick(0),
            target: String::new(),
            hardware,
        };
        o.active.insert(OutputKind::Recording, active);
        let ev = o.poll();
        let templates: Vec<_> = ev.notices.iter().map(|n| n.template).collect();
        assert_eq!(templates, ["The recording stopped: {error}", "{encoder} stopped working; the next start tries another encoder"]);
        let prober = fake(craft_hwenc::Outcome::Works);
        assert_eq!(prober.program(), program);
        assert!(matches!(o.probes.get(&prober, mf, Input::Nv12), Some(craft_hwenc::Outcome::Failed(_))));
        assert!(o.active.is_empty());
    }

    /// A stream with one destination whose lost connections, recoveries and failure the test
    /// sets.
    struct LosingStream {
        reconnects: Arc<std::sync::atomic::AtomicU32>,
        failed: Arc<AtomicBool>,
        recovered: Arc<std::sync::atomic::AtomicU32>,
        /// The destination gave up while the stream runs on.
        gave_up: Arc<AtomicBool>,
    }

    impl OutputInstance for LosingStream {
        fn video(&self) -> Arc<Fifo<Arc<PooledFrame>>> {
            Arc::new(Fifo::new(1))
        }
        fn audio(&self) -> Arc<Fifo<PooledBlock>> {
            Arc::new(Fifo::new(1))
        }
        fn status(&self) -> OutputStatus {
            if self.failed.load(Ordering::SeqCst) { OutputStatus::Failed("every destination failed".into()) } else { OutputStatus::Running }
        }
        fn target(&self) -> String {
            String::new()
        }
        fn stop(&mut self) -> Result<(), PluginError> {
            Ok(())
        }
        fn destinations(&self) -> Vec<studiocraft_plugin::DestinationState> {
            vec![studiocraft_plugin::DestinationState {
                id: "main".into(),
                target: String::new(),
                status: if self.gave_up.load(Ordering::SeqCst) {
                    DestinationStatus::Failed(studiocraft_plugin::Failure::new(studiocraft_plugin::Cause::Lost, "gave up"))
                } else {
                    DestinationStatus::Live
                },
                bytes: 0,
                dropped: 0,
                reconnects: self.reconnects.load(Ordering::SeqCst),
                next_try: None,
                max_tries: 10,
            }]
        }
        fn reconnects(&self) -> u64 {
            u64::from(self.reconnects.load(Ordering::SeqCst))
        }
        fn destination_counts(&self) -> DestinationCounts {
            DestinationCounts { live: 1, retrying: 0 }
        }
        fn recoveries(&self) -> u64 {
            u64::from(self.recovered.load(Ordering::SeqCst))
        }
    }

    /// Runs a [`LosingStream`] as stream `id` of `o`; its lost connections, failure and
    /// recoveries.
    fn losing_stream(o: &mut Outputs, id: u64) -> ([Arc<std::sync::atomic::AtomicU32>; 2], Arc<AtomicBool>) {
        let (reconnects, recovered) = (Arc::new(std::sync::atomic::AtomicU32::new(0)), Arc::new(std::sync::atomic::AtomicU32::new(0)));
        let gave_up = Arc::new(AtomicBool::new(false));
        let instance = Box::new(LosingStream {
            reconnects: Arc::clone(&reconnects),
            failed: Arc::new(AtomicBool::new(false)),
            recovered: Arc::clone(&recovered),
            gave_up: Arc::clone(&gave_up),
        });
        let active = Active {
            id,
            instance,
            stats: Arc::new(EncoderStats::default()),
            started: Instant::now(),
            start: Tick(0),
            target: String::new(),
            hardware: None,
        };
        o.active.insert(OutputKind::Streaming, active);
        ([reconnects, recovered], gave_up)
    }

    #[test]
    fn a_running_stream_shows_its_counts_and_gets_reconnect_notices() {
        let mut o = Outputs { stream_names: HashMap::from([("main".to_string(), "Main".to_string())]), ..Outputs::default() };
        assert_eq!(o.stream_destination_name("main"), "Main");
        assert_eq!(o.stream_destination_name("other"), "other", "a destination with no name is named by its id");
        let ([_, recovered], gave_up) = losing_stream(&mut o, 1);
        let state = o.state(OutputKind::Streaming).unwrap();
        assert_eq!(state.destinations, DestinationCounts { live: 1, retrying: 0 });
        assert!(o.recovered_now().is_empty(), "nothing recovered");
        assert!(o.loss_notice("main", Tick(0)));
        assert!(!o.loss_notice("main", Tick(1)), "a second loss within the minute");
        recovered.store(1, Ordering::SeqCst);
        assert_eq!(o.recovered_now(), ["main"]);
        assert!(o.recovered_now().is_empty(), "each reconnect gets one notice");
        // A destination that gave up does not try again, so its loss gets no notice.
        gave_up.store(true, Ordering::SeqCst);
        assert!(!o.loss_notice("main", Tick(i64::MAX)), "a destination that gave up");
        gave_up.store(false, Ordering::SeqCst);
        assert!(o.stop(OutputKind::Streaming, false).is_some());
        assert!(!o.loss_notice("main", Tick(0)), "no loss notice once the stream has stopped");
        assert_eq!(o.state(OutputKind::Streaming).map(|s| s.destinations), Some(DestinationCounts::default()), "a stopping stream");
        // A new stream starts with no notices shown.
        let _ = losing_stream(&mut o, 2);
        assert!(o.loss_notice("main", Tick(1)));
    }

    #[test]
    fn a_destination_waiting_for_the_network_still_tries() {
        use studiocraft_plugin::{Cause, Failure};
        let failure = || Failure::new(Cause::NoRoute, "no route");
        for s in [
            DestinationStatus::Connecting,
            DestinationStatus::Live,
            DestinationStatus::Retrying { attempt: 2, failure: failure() },
            DestinationStatus::WaitingForNetwork { attempt: 2, failure: failure() },
        ] {
            assert!(still_trying(&s), "{s:?}");
        }
        assert!(!still_trying(&DestinationStatus::Stopped));
        assert!(!still_trying(&DestinationStatus::Failed(failure())));
    }

    #[test]
    fn lost_connections_are_seen_while_a_stream_runs_and_as_it_stops_or_fails() {
        let mut o = Outputs::default();
        let start = |o: &mut Outputs, id: u64| {
            let (reconnects, failed) = (Arc::new(std::sync::atomic::AtomicU32::new(0)), Arc::new(AtomicBool::new(false)));
            let (recovered, gave_up) = (Arc::new(std::sync::atomic::AtomicU32::new(0)), Arc::new(AtomicBool::new(false)));
            let instance = Box::new(LosingStream { reconnects: Arc::clone(&reconnects), failed: Arc::clone(&failed), recovered, gave_up });
            let active = Active {
                id,
                instance,
                stats: Arc::new(EncoderStats::default()),
                started: Instant::now(),
                start: Tick(i64::try_from(id).unwrap_or(0) * 100),
                target: String::new(),
                hardware: None,
            };
            o.active.insert(OutputKind::Streaming, active);
            (reconnects, failed)
        };
        let r = |count, lost, start| (crate::events::Kind::Reconnect { destination: "main".into(), count, lost }, Tick(start));
        let (lost, _) = start(&mut o, 1);
        assert!(o.reconnects_now().is_empty());
        lost.store(1, Ordering::SeqCst);
        assert_eq!(o.reconnects_now(), [r(1, 1, 100)]);
        assert!(o.reconnects_now().is_empty(), "each lost connection is seen once");
        // Lost after the last look and before the stop; the event keeps the stream's start.
        lost.store(2, Ordering::SeqCst);
        assert!(o.stop(OutputKind::Streaming, false).is_some());
        assert_eq!(o.reconnects_now(), [r(2, 1, 100)]);
        // A new stream counts from 0; it loses its connection and fails before the next look.
        let (lost, failed) = start(&mut o, 2);
        lost.store(1, Ordering::SeqCst);
        failed.store(true, Ordering::SeqCst);
        let _ = o.poll();
        assert!(o.active.is_empty(), "the failed stream was stopped");
        assert_eq!(o.reconnects_now(), [r(1, 1, 200)]);
        assert!(o.reconnects_now().is_empty());
    }

    #[test]
    fn recording_names_do_not_overwrite() {
        let dir = std::env::temp_dir().join(format!("studiocraft-names-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let first = recording_path(&dir, "mkv").unwrap();
        assert!(first.exists(), "the name is taken as soon as it is chosen");
        let second = recording_path(&dir, "mkv").unwrap();
        assert_ne!(first, second);
        assert!(second.exists());
        // Both can come from the same second, or the clock can tick between the two calls.
        let name = second.to_string_lossy();
        assert!(name.ends_with(" (2).mkv") || !name.contains(" ("), "{name}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_streams_copy_is_named_for_the_stream_and_its_time() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("Videos");
        let path = stream_copy_path(&dir).unwrap();
        assert!(path.exists(), "the folder is made and the name taken");
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("Stream ") && name.ends_with(".mkv") && name.len() == "Stream 2026-11-23 10-00-00.mkv".len(), "{name}");
    }

    /// A stream with a copy whose state the test sets: `failure` while it runs, `ended` once it
    /// has stopped.
    struct CopyingStream {
        failure: Arc<Mutex<Option<String>>>,
        ended: StreamCopyState,
        stopped: bool,
        /// When set, the stop waits until this gate's sender is dropped.
        gate: Option<std::sync::mpsc::Receiver<()>>,
    }

    impl OutputInstance for CopyingStream {
        fn video(&self) -> Arc<Fifo<Arc<PooledFrame>>> {
            Arc::new(Fifo::new(1))
        }
        fn audio(&self) -> Arc<Fifo<PooledBlock>> {
            Arc::new(Fifo::new(1))
        }
        fn status(&self) -> OutputStatus {
            OutputStatus::Running
        }
        fn target(&self) -> String {
            String::new()
        }
        fn stop(&mut self) -> Result<(), PluginError> {
            if let Some(gate) = self.gate.take() {
                let _ = gate.recv();
            }
            self.stopped = true;
            Ok(())
        }
        fn stream_copy(&self) -> Option<StreamCopyState> {
            if self.stopped {
                return Some(self.ended.clone());
            }
            let failure = self.failure.lock().unwrap().clone();
            Some(StreamCopyState { path: self.ended.path.clone(), failure, ..StreamCopyState::default() })
        }
    }

    /// The notices of `o` until nothing is active or finishing.
    fn notices_until_idle(o: &mut Outputs) -> Vec<(String, bool)> {
        let mut notices = Vec::new();
        while o.any_active() {
            notices.extend(o.poll().notices);
            std::thread::yield_now();
        }
        notices.iter().map(|n| (n.text(), n.error)).collect()
    }

    fn copying(o: &mut Outputs, ended: StreamCopyState) -> Arc<Mutex<Option<String>>> {
        copying_gated(o, ended, None)
    }

    /// [`copying`], with a stop that waits on `gate` when one is given.
    fn copying_gated(o: &mut Outputs, ended: StreamCopyState, gate: Option<std::sync::mpsc::Receiver<()>>) -> Arc<Mutex<Option<String>>> {
        stream_with_copy_gated(o, Tick(0), Arc::new(EncoderStats::default()), ended, gate)
    }

    /// Run a stream that started at `start` with the encoder counters `stats` and a copy, whose
    /// state is `ended` once it has stopped; the copy's failure while it runs, for the test to set.
    pub(crate) fn stream_with_copy(o: &mut Outputs, start: Tick, stats: Arc<EncoderStats>, ended: StreamCopyState) -> Arc<Mutex<Option<String>>> {
        stream_with_copy_gated(o, start, stats, ended, None)
    }

    /// Note that the copy of a stream that a newer stream followed ended as `state`.
    pub(crate) fn earlier_copy_ended(o: &mut Outputs, state: StreamCopyState) {
        o.note_copy_ended(state, false);
    }

    /// [`stream_with_copy`], with a stop that waits on `gate` when one is given.
    fn stream_with_copy_gated(
        o: &mut Outputs,
        start: Tick,
        stats: Arc<EncoderStats>,
        ended: StreamCopyState,
        gate: Option<std::sync::mpsc::Receiver<()>>,
    ) -> Arc<Mutex<Option<String>>> {
        let failure = Arc::new(Mutex::new(None));
        let instance = Box::new(CopyingStream { failure: Arc::clone(&failure), ended, stopped: false, gate });
        let active = Active { id: 1, instance, stats, started: Instant::now(), start, target: String::new(), hardware: None };
        o.stream_started();
        o.copy_path = active.instance.stream_copy().map(|c| c.path);
        o.active.insert(OutputKind::Streaming, active);
        failure
    }

    #[test]
    fn a_streams_copy_is_reported_when_the_stream_ends_and_its_failure_once() {
        let path = PathBuf::from("/videos/Stream 2026-11-23 10-00-00.mkv");
        let mut o = Outputs::default();
        let saved = StreamCopyState { path: path.clone(), bytes: 9, finished: true, ..StreamCopyState::default() };
        let _ = copying(&mut o, saved.clone());
        assert!(o.poll().notices.is_empty(), "nothing while the copy runs");
        assert!(o.stream_copy().is_some_and(|c| !c.finished));
        assert!(o.stop(OutputKind::Streaming, false).is_some());
        let shown = path.display().to_string();
        assert_eq!(notices_until_idle(&mut o), [("The stream ended".to_string(), false), (format!("Stream copy saved to {shown}"), false)]);
        assert_eq!(o.stream_copy(), Some(saved), "the ended stream's copy stays shown");
        // Packets the disk could not take are counted in the notice.
        let _ = copying(&mut o, StreamCopyState { path: path.clone(), lost_packets: 12, finished: true, ..StreamCopyState::default() });
        assert!(o.stop(OutputKind::Streaming, false).is_some());
        let texts = notices_until_idle(&mut o);
        assert_eq!(texts[1], (format!("Stream copy saved to {shown}; 12 packets were left out while the disk fell behind"), true));
        // A copy that fails while the stream runs is shown once, and not again when the stream ends.
        let failed = "the copy of the stream could not be written: the disk is full".to_string();
        let failure = copying(&mut o, StreamCopyState { path: path.clone(), failure: Some(failed.clone()), ..StreamCopyState::default() });
        *failure.lock().unwrap() = Some(failed.clone());
        let first = o.poll().notices;
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].text(), format!("The stream's copy stopped; the stream goes on: {failed}"));
        assert!(o.poll().notices.is_empty(), "shown once");
        assert!(o.stop(OutputKind::Streaming, false).is_some());
        assert_eq!(notices_until_idle(&mut o), [("The stream ended".to_string(), false)]);
        // A copy that fails only as the stream ends is reported then.
        let _ = copying(&mut o, StreamCopyState { path, failure: Some("not finished".into()), ..StreamCopyState::default() });
        assert!(o.stop(OutputKind::Streaming, false).is_some());
        let texts = notices_until_idle(&mut o);
        assert_eq!(texts[1], ("The stream's copy could not be finished: not finished".to_string(), true));
    }

    #[test]
    fn a_streams_copy_stays_shown_while_the_stream_stops() {
        let path = PathBuf::from("/videos/Stream 2026-11-23 10-00-00.mkv");
        let mut o = Outputs::default();
        let saved = StreamCopyState { path: path.clone(), bytes: 9, finished: true, ..StreamCopyState::default() };
        let (release, gate) = std::sync::mpsc::channel::<()>();
        let _ = copying_gated(&mut o, saved.clone(), Some(gate));
        assert!(o.stop(OutputKind::Streaming, false).is_some());
        // The stop waits on the gate, so the copy is still finishing.
        let _ = o.poll();
        let finishing = o.stream_copy();
        assert!(finishing.as_ref().is_some_and(|c| c.path == path && !c.finished), "{finishing:?}");
        drop(release);
        let _ = notices_until_idle(&mut o);
        assert_eq!(o.stream_copy(), Some(saved), "the stop's state replaces it");
    }

    #[test]
    fn a_streams_copy_is_known_as_ended_once_its_stream_has_stopped() {
        let path = PathBuf::from("/videos/Stream 2026-11-23 10-00-00.mkv");
        let other = PathBuf::from("/videos/Stream 2026-11-23 11-00-00.mkv");
        let mut o = Outputs::default();
        assert_eq!(o.copy_ended(&path), Some(false), "a copy no stream writes is not finished");
        let _ = copying(&mut o, StreamCopyState { path: path.clone(), finished: true, ..StreamCopyState::default() });
        assert_eq!(o.copy_ended(&path), None, "the stream runs");
        assert!(o.stop(OutputKind::Streaming, false).is_some());
        let _ = notices_until_idle(&mut o);
        assert_eq!(o.copy_ended(&path), Some(true));
        // The next stream's copy fails; the first copy keeps how it ended.
        let _ = copying(&mut o, StreamCopyState { path: other.clone(), failure: Some("the disk is full".into()), ..StreamCopyState::default() });
        assert_eq!((o.copy_ended(&path), o.copy_ended(&other)), (Some(true), None));
        assert!(o.stop(OutputKind::Streaming, false).is_some());
        let _ = notices_until_idle(&mut o);
        assert_eq!((o.copy_ended(&path), o.copy_ended(&other)), (Some(true), Some(false)));
        // A copy that runs as the app closes is known as ended too.
        let third = PathBuf::from("/videos/Stream 2026-11-23 12-00-00.mkv");
        let _ = copying(&mut o, StreamCopyState { path: third.clone(), finished: true, ..StreamCopyState::default() });
        o.stop_all_blocking();
        assert_eq!(o.copy_ended(&third), Some(true));
        // A new copy with the file name of one that was finished is not taken for it.
        let _ = copying(&mut o, StreamCopyState { path: path.clone(), ..StreamCopyState::default() });
        assert_eq!(o.copy_ended(&path), None, "the new copy runs");
        assert!(o.stop(OutputKind::Streaming, false).is_some());
        let _ = notices_until_idle(&mut o);
        assert_eq!(o.copy_ended(&path), Some(false), "the new copy was not finished");
        // A stream that is still stopping holds back only its own copy.
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let handle = std::thread::spawn(move || {
            let _ = wait.recv();
            Ok(())
        });
        let copy = Some((CopySlot::default(), false, o.streams));
        o.stopping.push(Stopping { kind: OutputKind::Streaming, target: String::new(), failed: false, handle, copy, copy_path: Some(other.clone()) });
        assert_eq!((o.copy_ended(&third), o.copy_ended(&other)), (Some(true), None));
        release.send(()).unwrap();
        let _ = notices_until_idle(&mut o);
        // Only the last ones are kept.
        for n in 0..MAX_UNFINISHED {
            o.note_copy_ended(
                StreamCopyState { path: PathBuf::from(format!("/videos/{n}.mkv")), finished: true, ..StreamCopyState::default() },
                true,
            );
        }
        // Every copy that ended since the last poll is known until the next one.
        assert_eq!(o.copy_ended(&path), Some(false));
        assert_eq!(o.copy_ended(Path::new("/videos/0.mkv")), Some(true), "ended since the last poll");
        let _ = o.poll();
        assert_eq!(o.copy_ended(Path::new("/videos/0.mkv")), Some(true));
        assert_eq!(o.copy_ended(&path), Some(false), "forgotten");
        assert_eq!(o.ended_copies.len(), MAX_UNFINISHED);
    }

    #[test]
    fn copies_that_end_in_one_poll_are_all_known_until_the_next() {
        let mut o = Outputs::default();
        let path = |n: usize| PathBuf::from(format!("/videos/{n}.mkv"));
        for n in 0..=MAX_UNFINISHED {
            o.note_copy_ended(StreamCopyState { path: path(n), finished: true, ..StreamCopyState::default() }, n == MAX_UNFINISHED);
        }
        assert_eq!(o.copy_ended(&path(0)), Some(true), "the first one is still known");
        let _ = o.poll();
        assert_eq!((o.copy_ended(&path(0)), o.copy_ended(&path(1))), (Some(false), Some(true)), "the poll after keeps the last ones");
        assert_eq!(o.ended_copies.len(), MAX_UNFINISHED);
    }

    #[test]
    fn an_older_streams_stop_does_not_show_its_copy_as_the_newer_streams() {
        let path = PathBuf::from("/videos/Stream 2026-11-23 10-00-00.mkv");
        let mut o = Outputs::default();
        let saved = StreamCopyState { path: path.clone(), bytes: 9, finished: true, ..StreamCopyState::default() };
        let (release, gate) = std::sync::mpsc::channel::<()>();
        let _ = copying_gated(&mut o, saved, Some(gate));
        assert!(o.stop(OutputKind::Streaming, false).is_some());
        // A stream without a copy starts and ends while the first one's stop still waits.
        o.stream_started();
        assert_eq!(o.stream_copy(), None);
        drop(release);
        let texts = notices_until_idle(&mut o);
        assert_eq!(texts[1], (format!("Stream copy saved to {}", path.display()), false), "the older copy's notice still shows");
        assert_eq!(o.stream_copy(), None, "the newer stream had no copy");
    }

    #[test]
    fn a_stop_that_cannot_finish_the_file_is_reported_once() {
        let mut o = Outputs::default();
        for (kind, failed) in [(OutputKind::Recording, false), (OutputKind::Streaming, true)] {
            let handle = std::thread::spawn(|| Err(PluginError::Unavailable("the muxer stopped".into())));
            o.stopping.push(Stopping { kind, target: String::new(), failed, handle, copy: None, copy_path: None });
        }
        let mut notices = Vec::new();
        while o.any_active() {
            notices.extend(o.poll().notices);
            std::thread::yield_now();
        }
        let texts: Vec<(String, bool)> = notices.iter().map(|n| (n.text(), n.error)).collect();
        assert_eq!(
            texts,
            [("The recording could not be finished: the muxer stopped".to_string(), true)],
            "a stream whose failure was shown while it ran is not reported again"
        );
    }

    #[test]
    fn the_profile_s_encoder_settings_reach_the_encoder() {
        let mut p = Profile::default();
        p.output.encoder_settings.insert("x264".into(), Settings::new().with("preset", "fast"));
        p.stream.video_encoder = "nvenc_h264".into();
        p.stream.encoder_settings.insert("nvenc_h264".into(), Settings::new().with("preset", "p3"));
        p.stream.encoder_settings.insert("x264".into(), Settings::new().with("preset", "medium"));

        let rec = plan(&p, OutputKind::Recording);
        assert_eq!((rec.output_id, rec.encoder_id.as_str()), ("file", "x264"));
        assert_eq!(rec.encoder_settings, Settings::new().with("preset", "fast"));
        let stream = plan(&p, OutputKind::Streaming);
        assert_eq!(stream.encoder_settings, Settings::new().with("preset", "p3"), "the stream's own encoder's settings");

        let video = VideoFormat { width: 64, height: 48, fps_num: 30, fps_den: 1, pixel: PixelFormat::Nv12 };
        for (plan, preset) in [(rec, "-preset fast"), (stream, "-preset p3")] {
            let Some(encoder) = studiocraft_outputs::encoders::all().into_iter().find(|e| e.id() == plan.encoder_id) else {
                panic!("no encoder {}", plan.encoder_id)
            };
            let stats = Arc::new(EncoderStats::default());
            let job = plan.job(Arc::new(encoder), video, PathBuf::from("ffmpeg"), stats, Tick::from_units(0, 1));
            let Some(ffmpeg) = job.video_encoder.ffmpeg() else { panic!("{} runs in ffmpeg", plan.encoder_id) };
            let args = ffmpeg.ffmpeg_args(&job.video_encoder_settings, job.rate, 30.0).unwrap().join(" ");
            assert!(args.contains(preset), "{args}");
        }
    }

    #[test]
    fn an_encoder_that_cannot_run_names_the_settings_tab_of_its_output() {
        let x264 = studiocraft_outputs::encoders::all().into_iter().next().unwrap();
        let other = ["libx265".to_string()];
        let listed = ["libx264".to_string()];
        assert!(check_encoder(&x264, &listed, OutputKind::Recording).is_ok());
        assert!(check_encoder(&x264, &[], OutputKind::Recording).is_ok(), "an unknown list does not rule it out");
        let Err(StudioError::Output(m)) = check_encoder(&x264, &other, OutputKind::Recording) else { panic!("x264 started without libx264") };
        assert!(m.contains("no libx264 encoder") && m.ends_with("Settings › Output"), "{m}");
        // A stream runs the built-in encoders only, whatever ffmpeg offers.
        for offered in [&listed[..], &other[..], &[]] {
            let Err(StudioError::Output(m)) = check_encoder(&x264, offered, OutputKind::Streaming) else { panic!("a stream started with x264") };
            assert!(m.contains("runs inside ffmpeg") && m.ends_with("Settings › Stream"), "{m}");
        }
    }

    #[test]
    fn a_built_in_encoder_records_and_streams() {
        struct Builtin(studiocraft_plugin::Codec);
        impl Plugin for Builtin {
            fn id(&self) -> &'static str {
                "builtin"
            }
            fn name(&self) -> &'static str {
                "Built-in"
            }
            fn schema(&self) -> studiocraft_model::SettingsSchema {
                studiocraft_model::SettingsSchema::default()
            }
        }
        impl Encoder for Builtin {
            fn codec(&self) -> studiocraft_plugin::Codec {
                self.0
            }
        }
        let video = Builtin(studiocraft_plugin::Codec::H264);
        let listed = ["libx264".to_string()];
        for offered in [&listed[..], &[]] {
            assert!(check_encoder(&video, offered, OutputKind::Streaming).is_ok(), "streams run built-in encoders");
            assert!(check_encoder(&video, offered, OutputKind::Recording).is_ok(), "recordings run built-in encoders");
        }
        let mut registry = studiocraft_plugin::Registry::new();
        studiocraft_outputs::register_all(&mut registry).unwrap();
        let h264 = registry.encoder("h264").unwrap();
        assert!(check_encoder(h264.as_ref(), &[], OutputKind::Recording).is_ok());
        assert!(check_encoder(h264.as_ref(), &[], OutputKind::Streaming).is_ok());
        let audio = Builtin(studiocraft_plugin::Codec::Aac);
        let Err(StudioError::Output(m)) = check_encoder(&audio, &listed, OutputKind::Recording) else { panic!("an audio encoder started as video") };
        assert_eq!(m, "Built-in is not a video encoder; choose another encoder in Settings › Output");
    }

    #[test]
    fn the_replay_buffer_runs_the_built_in_encoder_with_the_profile_s_settings() {
        let mut p = Profile { replay: studiocraft_model::ReplaySettings { seconds: 90, memory_mb: 2048 }, ..Profile::default() };
        p.output.video_encoder = "x264".into();
        p.output.encoder_settings.insert("h264".into(), Settings::new().with("keyframe_s", 1));
        p.output.quality = RecordingQuality::Compact;
        let r = plan(&p, OutputKind::Replay);
        assert_eq!((r.output_id, r.encoder_id.as_str(), r.audio_kbps), ("replay", "h264", 192));
        assert_eq!(r.settings, Settings::new().with("seconds", 90).with("memory_mb", 2048));
        assert_eq!(r.encoder_settings, Settings::new().with("keyframe_s", 1));
        assert_eq!(r.rate, RateControl::Quality(RecordingQuality::Compact.quality_level().unwrap()));
        p.output.quality = RecordingQuality::Lossless;
        assert_eq!(plan(&p, OutputKind::Replay).rate, RateControl::Quality(RecordingQuality::High.quality_level().unwrap()));
        let mut registry = studiocraft_plugin::Registry::new();
        studiocraft_outputs::register_all(&mut registry).unwrap();
        let h264 = registry.encoder("h264").unwrap();
        assert!(check_encoder(h264.as_ref(), &[], OutputKind::Replay).is_ok());
        assert!(registry.output("replay").is_ok());
    }

    #[test]
    fn saved_replays_are_reported_when_their_file_is_written() {
        let dir = std::env::temp_dir().join(format!("studiocraft-replays-{}", std::process::id()));
        let path = replay_path(&dir).unwrap();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("Replay ") && name.ends_with(".mkv"), "{name}");
        assert_ne!(replay_path(&dir).unwrap(), path, "two saves in the same second get two names");
        let mut o = Outputs::default();
        o.saving.push(Saving { path: path.clone(), handle: std::thread::spawn(|| Ok(())) });
        o.saving.push(Saving { path: dir.join("b.mkv"), handle: std::thread::spawn(|| Err(PluginError::Failed("no space left".into()))) });
        let mut notices = Vec::new();
        while o.saving() > 0 {
            notices.extend(o.poll().notices);
            std::thread::yield_now();
        }
        let mut texts: Vec<(String, bool)> = notices.iter().map(|n| (n.text(), n.error)).collect();
        texts.sort();
        assert_eq!(
            texts,
            [(format!("Replay saved to {}", path.display()), false), ("The replay could not be saved: no space left".to_string(), true)]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn replays_being_written_are_limited() {
        let mut o = Outputs::default();
        let (go, wait) = std::sync::mpsc::channel::<()>();
        let wait = Arc::new(Mutex::new(wait));
        for n in 0..MAX_SAVES {
            assert!(o.check_save_room().is_ok(), "save {n}");
            let wait = Arc::clone(&wait);
            let handle = std::thread::spawn(move || {
                let _ = wait.lock().map(|w| w.recv());
                Ok(())
            });
            o.saving.push(Saving { path: PathBuf::from(format!("{n}.mkv")), handle });
        }
        let Err(StudioError::Output(m)) = o.check_save_room() else { panic!("a ninth replay was written at once") };
        assert_eq!(m, "8 replays are already being saved; try again when one is done");
        // One write finishes, and its place is free again once the studio has reported it.
        go.send(()).unwrap();
        while o.saving() == MAX_SAVES {
            let _ = o.poll();
            std::thread::yield_now();
        }
        assert!(o.check_save_room().is_ok());
        for _ in 1..MAX_SAVES {
            go.send(()).unwrap();
        }
        o.stop_all_blocking();
        assert_eq!(o.saving(), 0);
    }

    #[test]
    fn the_configured_folder_wins() {
        assert_eq!(recording_folder(" D:/takes "), PathBuf::from("D:/takes"));
        assert!(!recording_folder("").as_os_str().is_empty());
    }
}
