//! A live stream through the built-in encoders, sent by this process to each of its destinations
//! over RTMP or RTMPS.
//!
//! - The video and audio encoder threads of [`crate::native`] run as for a recording. Its third
//!   thread hands each packet, in decoding order, to [`Publish`], which shares the packet once
//!   ([`craft_packet::PacketPool`]) and offers it to every destination ([`craft_packet::Fanout`]).
//!   Offering a packet never waits for a network.
//! - Each destination that sends has a thread of its own. The thread connects
//!   ([`craft_rtmp::Publisher`]), sends `onMetaData` and the H.264 and AAC sequence headers, and
//!   then sends the packets from the next keyframe on, timed from that keyframe. Its ring holds
//!   [`RING_SECONDS`] seconds of the stream. A destination that falls further behind loses packets
//!   up to the next keyframe, and the other destinations lose nothing.
//! - A destination whose connection cannot be made, or is lost, is tried again as the profile's
//!   [`ReconnectSettings`] say: the first wait, then a wait twice as long each time up to the
//!   longest wait (2, 4, 8, 16, 30, 30... seconds by default), each wait cut by a random 0 to
//!   [`JITTER_PERCENT`] % and never below 1 s by that cut. It gives up after the set number of tries
//!   in a row fail, and at the first failure when automatic reconnect is off; a connection lost
//!   within [`STABLE_AFTER`] of being made counts as a failed try. A server that refuses the
//!   stream, and an address or key that is not valid, are not tried again.
//! - A failure of one destination stops neither the other destinations nor the encoders. The
//!   stream fails when every destination has failed.
//! - A destination can be stopped and started again while the stream runs.
//!
//! No log line, error or status of this module holds a stream key: the keys go to
//! [`craft_rtmp::Config`] and nowhere else, and that crate keeps them out of its errors.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use craft_flv::{Body, MAX_COMPOSITION_TIME, MIN_COMPOSITION_TIME, Metadata};
use craft_media::matroska::TrackSpec;
use craft_packet::{
    Control, Fanout, Info, Kind, MAX_RING_BYTES, MAX_SUBSCRIBERS, PacketPool, Receiver, RecvError, RingConfig, SharedPacket, StreamRates,
};
use craft_rtmp::{Config, Publisher};
use studiocraft_media::audio::{PooledBlock, SAMPLE_RATE};
use studiocraft_media::queue::Fifo;
use studiocraft_media::stats::EncoderStats;
use studiocraft_media::video::PooledFrame;
use studiocraft_model::ReconnectSettings;
use studiocraft_plugin::{
    AddressOutcome, Cause, DestinationCounts, DestinationState, DestinationStatus, EncoderKind, Failure, OutputInstance, OutputJob, OutputStatus,
    PluginError, RateControl,
};

use crate::native::{DISK_WAIT, Item, Native, PacketQueue, QUEUE_BYTES, QUEUE_SPAN_NS, Sink, Track};

/// Seconds of the stream each destination's ring holds.
pub const RING_SECONDS: u32 = 4;
/// The most a wait before a try again is cut by, in percent, so destinations that lost their
/// connections together do not all try again at the same moment.
pub const JITTER_PERCENT: u64 = 20;
/// The shortest wait the random cut leaves.
const MIN_JITTERED_WAIT: Duration = Duration::from_secs(1);
/// How long a connection must last before its loss starts a new count of failed tries.
pub const STABLE_AFTER: Duration = Duration::from_secs(30);
/// How long a destination's thread waits for a packet before it reads what the server sent.
const RECV_WAIT: Duration = Duration::from_millis(100);
/// How long a stop waits for the destinations to send what their rings hold and close.
const CLOSE_WAIT: Duration = Duration::from_secs(5);
/// AAC access units a second at 48 kHz, rounded up: 48 000 / 1024.
const AUDIO_PACKETS_PER_SECOND: u32 = 47;
/// Packets the pool makes up front.
const POOL_PACKETS: usize = 256;
const NS_PER_MS: i64 = 1_000_000;

/// One destination a stream is started with.
#[derive(Clone, PartialEq, Eq)]
pub struct Target {
    pub id: String,
    /// `rtmp://host/app` or `rtmps://host/app`.
    pub server: String,
    /// The stream key; empty when the server's address ends with the stream's name.
    pub key: String,
    /// Whether the stream sends to it from the start.
    pub enabled: bool,
}

impl fmt::Debug for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let key = if self.key.is_empty() { "" } else { "<redacted>" };
        f.debug_struct("Target").field("id", &self.id).field("server", &self.shown()).field("key", &key).field("enabled", &self.enabled).finish()
    }
}

impl Target {
    /// The server's address as status and logs show it: a query is replaced by `?<redacted>`,
    /// and the stream's name is left out when the key is empty and the name ends the address.
    pub fn shown(&self) -> String {
        let server = self.server.trim();
        let (address, query) = server.split_once('?').map_or((server, false), |(a, _)| (a, true));
        let base = if self.key.trim().is_empty() { split_name(address.trim_end_matches('/')).map_or(address, |(b, _)| b) } else { address };
        if query { format!("{base}?<redacted>") } else { base.to_string() }
    }

    /// Whether the server and the key give an address the stream can publish to, as a start
    /// checks it.
    pub fn check(&self) -> Result<(), String> {
        self.config().map(|_| ()).map_err(|e| e.to_string())
    }

    /// What to publish to: the server and the key, or without a key, the server's address up to
    /// its last part and that part as the key, as when the key is written at the end of the
    /// address.
    fn config(&self) -> Result<Config, craft_rtmp::Error> {
        let server = self.server.trim();
        let key = self.key.trim();
        if !key.is_empty() {
            return Config::new(server, key);
        }
        match split_name(server.trim_end_matches('/')) {
            Some((base, name)) => Config::new(base, name),
            // An empty key gives the error that names what is missing.
            None => Config::new(server, ""),
        }
    }
}

/// `url` split before its last part when the host is followed by an application and a name:
/// `rtmp://host/live/name` gives `rtmp://host/live` and `name`. A query stays with the name, as
/// in ffmpeg's RTMP client: `rtmp://host/live/name?auth=t` gives `name?auth=t`.
fn split_name(url: &str) -> Option<(&str, &str)> {
    let start = url.find("://")?.checked_add(3)?;
    let (base, name) = url.rsplit_once('/')?;
    let (_, app) = base.get(start..)?.split_once('/')?;
    (!app.is_empty() && !name.is_empty()).then_some((base, name))
}

/// The time limits of the destinations.
#[derive(Clone, Debug)]
pub(crate) struct Timing {
    /// Whether a failed or lost connection is tried again at all.
    pub(crate) reconnect: bool,
    /// The wait before the first try again.
    pub(crate) first: Duration,
    /// The longest wait before a try again.
    pub(crate) longest: Duration,
    pub(crate) max_tries: u32,
    /// Whether each wait is cut by a random amount; tests turn it off.
    pub(crate) jitter: bool,
    pub(crate) connect: Duration,
    pub(crate) setup: Duration,
    pub(crate) stall: Duration,
    pub(crate) close: Duration,
    pub(crate) stable: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Timing::from(ReconnectSettings::default())
    }
}

impl From<ReconnectSettings> for Timing {
    fn from(r: ReconnectSettings) -> Self {
        Timing {
            reconnect: r.enabled,
            first: Duration::from_secs(r.first_delay_s.into()),
            longest: Duration::from_secs(r.max_delay_s.into()),
            max_tries: r.max_tries,
            jitter: true,
            connect: craft_rtmp::DEFAULT_CONNECT_TIMEOUT,
            setup: craft_rtmp::DEFAULT_SETUP_TIMEOUT,
            stall: craft_rtmp::DEFAULT_STALL_TIMEOUT,
            close: CLOSE_WAIT,
            stable: STABLE_AFTER,
        }
    }
}

impl Timing {
    /// The wait before the try that follows `failures` failed tries in a row (at least 1): the
    /// first wait doubled for each failure after the first, at most the longest wait.
    fn delay(&self, failures: u32) -> Duration {
        let doublings = failures.saturating_sub(1).min(31);
        self.first.checked_mul(1 << doublings).map_or(self.longest, |d| d.min(self.longest))
    }

    /// Whether a destination gives up after `failures` failed tries in a row.
    fn gives_up(&self, failures: u32) -> bool {
        !self.reconnect || failures >= self.max_tries
    }
}

/// `wait` cut by `random` (taken modulo [`JITTER_PERCENT`] + 1) percent, in tenths of a percent so
/// the cut is not a whole second; a wait the cut would bring below [`MIN_JITTERED_WAIT`] stops
/// there, and a wait already below it is kept.
fn jittered(wait: Duration, random: u64) -> Duration {
    let per_mille = u128::from(random % (JITTER_PERCENT * 10 + 1));
    let nanos = wait.as_nanos();
    let cut = nanos.saturating_mul(per_mille) / 1000;
    let left = Duration::from_nanos(u64::try_from(nanos.saturating_sub(cut)).unwrap_or(u64::MAX));
    left.max(wait.min(MIN_JITTERED_WAIT))
}

/// The next number of a destination's xorshift sequence.
fn next_random(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

/// A nonzero seed for a destination's sequence, from its id and the time it starts.
fn seed(id: &str) -> u64 {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() ^ u64::from(d.subsec_nanos()) << 32);
    // FNV-1a over the id, then the time mixed in.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in id.bytes() {
        h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
    }
    let s = h ^ now;
    if s == 0 { 0x9e37_79b9_7f4a_7c15 } else { s }
}

/// What each connection sends before the packets.
struct Headers {
    /// `onMetaData`, encoded.
    script: Vec<u8>,
    /// The H.264 encoder's `avcC` record.
    avcc: Vec<u8>,
    /// The AAC encoder's AudioSpecificConfig.
    asc: Vec<u8>,
}

impl Headers {
    fn new(job: &OutputJob, kbps: u32, tracks: &[TrackSpec]) -> Result<Headers, PluginError> {
        let private = |i: usize| tracks.get(i).map(|t| t.codec_private.clone()).unwrap_or_default();
        let (avcc, asc) = (private(Track::Video as usize), private(Track::Audio as usize));
        Body::AvcSequenceHeader(&avcc)
            .check()
            .map_err(|e| PluginError::Failed(format!("the video encoder's configuration cannot be streamed: {e}")))?;
        Body::AacSequenceHeader(&asc)
            .check()
            .map_err(|e| PluginError::Failed(format!("the audio encoder's configuration cannot be streamed: {e}")))?;
        let v = job.video;
        let fps = f64::from(v.fps_num) / f64::from(v.fps_den.max(1));
        let mut script = Vec::new();
        Metadata::h264_aac(v.width, v.height, fps, kbps, job.audio_bitrate_kbps, SAMPLE_RATE, true)
            .encode(&mut script)
            .map_err(|e| PluginError::Failed(format!("the stream's metadata could not be written: {e}")))?;
        Ok(Headers { script, avcc, asc })
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// How many destinations of a stream are live and how many wait to try again, kept as each
/// destination's status is set.
#[derive(Default)]
struct Counts {
    live: AtomicU32,
    retrying: AtomicU32,
}

impl Counts {
    /// The count a destination with status `s` is in.
    fn of(&self, s: &DestinationStatus) -> Option<&AtomicU32> {
        match s {
            DestinationStatus::Live => Some(&self.live),
            DestinationStatus::Retrying { .. } => Some(&self.retrying),
            _ => None,
        }
    }
}

/// A destination and what its thread reports.
struct Dest {
    target: Target,
    /// The stream's counts, shared by its destinations.
    counts: Arc<Counts>,
    /// The stream key it sends with: the target's, or one given when it was started again.
    key: Mutex<String>,
    status: Mutex<DestinationStatus>,
    /// How many times the status was set, with a signal for each.
    changes: Mutex<u64>,
    changed: Condvar,
    /// Bytes sent over every connection so far.
    bytes: AtomicU64,
    /// Packets lost over every connection so far.
    dropped: AtomicU64,
    reconnects: AtomicU32,
    /// The last connection was lost, and no connection has gone live since.
    lost: AtomicBool,
    /// Times it went live again after a lost connection.
    recovered: AtomicU32,
    /// Asks the thread to close the connection and end.
    stop: AtomicBool,
    /// The thread runs and has not decided to end. It is changed with `thread` locked, so a start
    /// either reaches a thread that goes on or finds none.
    alive: AtomicBool,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Dest {
    fn new(target: Target, counts: Arc<Counts>) -> Dest {
        Dest {
            key: Mutex::new(target.key.clone()),
            target,
            counts,
            status: Mutex::new(DestinationStatus::Stopped),
            changes: Mutex::new(0),
            changed: Condvar::new(),
            bytes: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            reconnects: AtomicU32::new(0),
            lost: AtomicBool::new(false),
            recovered: AtomicU32::new(0),
            stop: AtomicBool::new(false),
            alive: AtomicBool::new(false),
            thread: Mutex::new(None),
        }
    }

    /// The target with `key` as its stream key.
    fn with_key(&self, key: &str) -> Target {
        Target { key: key.to_string(), ..self.target.clone() }
    }

    fn config(&self) -> Result<Config, craft_rtmp::Error> {
        let key = lock(&self.key).clone();
        self.with_key(&key).config()
    }

    fn shown(&self) -> String {
        let key = lock(&self.key).clone();
        self.with_key(&key).shown()
    }

    fn set(&self, s: DestinationStatus) {
        let mut status = lock(&self.status);
        // The counts change with the status held, so each destination is in at most one count.
        if let Some(c) = self.counts.of(&status) {
            c.fetch_sub(1, Ordering::Relaxed);
        }
        if let Some(c) = self.counts.of(&s) {
            c.fetch_add(1, Ordering::Relaxed);
        }
        *status = s;
        // Counted with the status held, so a reader that sees the count and then reads the
        // status sees it live.
        if *status == DestinationStatus::Live && self.lost.swap(false, Ordering::Relaxed) {
            self.recovered.fetch_add(1, Ordering::SeqCst);
        }
        drop(status);
        let mut changes = lock(&self.changes);
        *changes = changes.wrapping_add(1);
        self.changed.notify_all();
    }

    fn status(&self) -> DestinationStatus {
        lock(&self.status).clone()
    }

    fn stopping(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    fn running(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// Asks the thread to stop, and wakes it from a wait before a try again.
    fn ask_stop(&self) {
        let slot = lock(&self.thread);
        self.stop.store(true, Ordering::SeqCst);
        match slot.as_ref() {
            Some(t) if self.running() => t.thread().unpark(),
            _ => self.set(DestinationStatus::Stopped),
        }
    }

    /// Wakes the thread from a wait before a try again.
    fn wake(&self) {
        if let Some(t) = lock(&self.thread).as_ref() {
            t.thread().unpark();
        }
    }

    /// Whether the thread ends now that it was stopped or the stream ended: not when it was
    /// started again meanwhile.
    fn ends(&self, hub: &Hub) -> bool {
        let _slot = lock(&self.thread);
        if !self.stopping() && !hub.control.is_ended() {
            return false;
        }
        self.set(DestinationStatus::Stopped);
        self.alive.store(false, Ordering::SeqCst);
        true
    }

    /// The thread gives up.
    fn fail(&self, failure: Failure) {
        let _slot = lock(&self.thread);
        self.set(DestinationStatus::Failed(failure));
        self.alive.store(false, Ordering::SeqCst);
    }

    fn state(&self) -> DestinationState {
        DestinationState {
            id: self.target.id.clone(),
            target: self.shown(),
            status: self.status(),
            bytes: self.bytes.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            reconnects: self.reconnects.load(Ordering::Relaxed),
        }
    }
}

/// Marks a destination as failed when its thread ends by a panic.
struct Watch<'a>(&'a Dest);

impl Drop for Watch<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.set(DestinationStatus::Failed(Failure::new(Cause::Other, "the destination stopped unexpectedly")));
            self.0.alive.store(false, Ordering::SeqCst);
        }
    }
}

/// What the destinations share.
struct Hub {
    control: Control,
    headers: Headers,
    ring: RingConfig,
    timing: Timing,
    dests: Vec<Arc<Dest>>,
    counts: Arc<Counts>,
}

impl Hub {
    fn find(&self, id: &str) -> Result<&Arc<Dest>, PluginError> {
        self.dests.iter().find(|d| d.target.id == id).ok_or_else(|| PluginError::Failed(format!("the stream has no destination `{id}`")))
    }
}

/// Start the thread of `d`, with `key` as its stream key when given, unless it is running; a
/// running thread keeps its key.
fn spawn(hub: &Arc<Hub>, d: &Arc<Dest>, key: Option<&str>) -> Result<(), PluginError> {
    let mut slot = lock(&d.thread);
    if d.running() {
        // Asked to stop and started again before the thread decided to end: it goes on.
        d.stop.store(false, Ordering::SeqCst);
        return Ok(());
    }
    if let Some(t) = slot.take() {
        // The thread has decided to end, so it ends without waiting for anything.
        let _ = t.join();
    }
    if let Some(key) = key {
        *lock(&d.key) = key.to_string();
    }
    d.stop.store(false, Ordering::SeqCst);
    d.alive.store(true, Ordering::SeqCst);
    d.set(DestinationStatus::Connecting);
    let (h, dd) = (Arc::clone(hub), Arc::clone(d));
    match std::thread::Builder::new().name("stream destination".into()).spawn(move || run(&h, &dd)) {
        Ok(t) => {
            *slot = Some(t);
            Ok(())
        }
        Err(e) => {
            let message = format!("no thread for the destination: {e}");
            d.alive.store(false, Ordering::SeqCst);
            d.set(DestinationStatus::Failed(Failure::new(Cause::Other, message.clone())));
            Err(PluginError::Failed(message))
        }
    }
}

/// Errors that a try again cannot fix: the address, the key or a setting is not valid, the
/// server refused the stream, or a packet cannot be sent.
fn is_final(e: &craft_rtmp::Error) -> bool {
    use craft_rtmp::Error as E;
    matches!(e, E::BadUrl(_) | E::BadKey(_) | E::BadConfig(_) | E::BadInput(_) | E::Rejected { .. })
}

/// The failure `e` describes. `live` is whether the connection had been made and the stream's
/// headers had started to go out, which makes a closed, reset or stalled connection a lost one.
pub(crate) fn failure(e: &craft_rtmp::Error, live: bool) -> Failure {
    use craft_rtmp::{Error as E, TlsKind};
    let detail = e.to_string();
    let cause = match e {
        E::BadUrl(_) => Cause::BadAddress,
        E::BadKey(_) => Cause::BadKey,
        E::Resolve(io) => lookup_cause(io),
        E::Connect(tried) => {
            let addresses: Vec<AddressOutcome> =
                tried.iter().map(|(a, io)| AddressOutcome { address: a.to_string(), cause: connect_cause(io), detail: io.to_string() }).collect();
            let has = |c: Cause| addresses.iter().any(|a| a.cause == c);
            let cause = if addresses.is_empty() {
                // The name resolved to no address.
                Cause::LookupFailed
            } else if has(Cause::Refused) {
                Cause::Refused
            } else if has(Cause::TimedOut) {
                Cause::TimedOut
            } else if addresses.iter().all(|a| a.cause == Cause::NoRoute) {
                Cause::NoRoute
            } else {
                Cause::Other
            };
            return Failure { cause, detail, addresses };
        }
        E::Tls(t) => match t.kind {
            TlsKind::UntrustedCertificate => Cause::CertificateUntrusted,
            TlsKind::NameMismatch => Cause::CertificateNameMismatch,
            TlsKind::Expired => Cause::CertificateExpired,
            TlsKind::CertificateNotAccepted => Cause::CertificateNotAccepted,
            TlsKind::HandshakeAlert | TlsKind::Other => Cause::TlsFailed,
        },
        E::Rejected { command: "connect", .. } => Cause::RejectedConnect,
        E::Rejected { command: "publish", .. } => Cause::RejectedPublish,
        E::Closed | E::Io(_) | E::Stalled(_) | E::Broken if live => Cause::Lost,
        E::Closed | E::Io(_) => Cause::ClosedDuringSetup,
        E::Timeout(_) => Cause::SetupTimedOut,
        _ => Cause::Other,
    };
    Failure::new(cause, detail)
}

/// The cause a failed name lookup gives. Only Windows reports the resolver's answer as an error
/// code (`WSAHOST_NOT_FOUND` 11001, `WSATRY_AGAIN` 11002); other systems report text, which is
/// kept as the detail and not read.
fn lookup_cause(e: &std::io::Error) -> Cause {
    match e.raw_os_error() {
        Some(11001) if cfg!(windows) => Cause::LookupNotFound,
        Some(11002) if cfg!(windows) => Cause::LookupNoAnswer,
        _ => Cause::LookupFailed,
    }
}

/// The cause a failed TCP connection to one address gives.
fn connect_cause(e: &std::io::Error) -> Cause {
    use std::io::ErrorKind as K;
    match e.kind() {
        K::ConnectionRefused => Cause::Refused,
        K::TimedOut | K::WouldBlock => Cause::TimedOut,
        K::NetworkUnreachable | K::HostUnreachable | K::NetworkDown => Cause::NoRoute,
        _ => Cause::Other,
    }
}

/// `f` with "gave up after `tries` tries", or "automatic reconnect is off", added to its detail.
fn gave_up(timing: &Timing, f: Failure, tries: u32) -> Failure {
    if !timing.reconnect {
        return Failure { detail: format!("{}; automatic reconnect is off", f.detail), ..f };
    }
    Failure { detail: format!("{}; gave up after {tries} tries", f.detail), ..f }
}

/// Waits `delay`, or less when the destination is stopped or the stream ends.
fn wait(hub: &Hub, d: &Dest, delay: Duration) {
    let deadline = Instant::now() + delay;
    loop {
        let now = Instant::now();
        if d.stopping() || hub.control.is_ended() || now >= deadline {
            return;
        }
        std::thread::park_timeout(deadline - now);
    }
}

/// How sending over one connection ended.
enum End {
    /// The destination was stopped, or the stream ended and every packet was sent.
    Done,
    /// The connection was lost: try again.
    Lost(Failure),
    /// Give up.
    Failed(Failure),
}

fn end_for(e: craft_rtmp::Error) -> End {
    let f = failure(&e, true);
    if is_final(&e) { End::Failed(f) } else { End::Lost(f) }
}

fn connect(hub: &Hub, d: &Dest) -> Result<Publisher, craft_rtmp::Error> {
    let mut c = d.config()?;
    c.connect_timeout = hub.timing.connect;
    c.setup_timeout = hub.timing.setup;
    c.stall_timeout = hub.timing.stall;
    Publisher::connect(&c)
}

/// The thread of one destination: connect, send, and try again when the connection is lost.
fn run(hub: &Hub, d: &Dest) {
    let _watch = Watch(d);
    let mut failures = 0u32;
    let mut last_error: Option<Failure> = None;
    let mut random = seed(&d.target.id);
    loop {
        if let Some(failure) = last_error.take() {
            d.set(DestinationStatus::Retrying { attempt: failures.saturating_add(1), failure });
            let delay = hub.timing.delay(failures.max(1));
            wait(hub, d, if hub.timing.jitter { jittered(delay, next_random(&mut random)) } else { delay });
        }
        if d.ends(hub) {
            return;
        }
        if !matches!(d.status(), DestinationStatus::Retrying { .. }) {
            d.set(DestinationStatus::Connecting);
        }
        let mut publisher = match connect(hub, d) {
            Ok(p) => p,
            // Stopped while it connected: the next turn ends the thread.
            Err(_) if d.stopping() || hub.control.is_ended() => continue,
            Err(e) if is_final(&e) => return d.fail(failure(&e, false)),
            Err(e) => {
                failures = failures.saturating_add(1);
                if hub.timing.gives_up(failures) {
                    return d.fail(gave_up(&hub.timing, failure(&e, false), failures));
                }
                last_error = Some(failure(&e, false));
                continue;
            }
        };
        // Stopped while it connected: the next turn ends the thread without sending.
        if d.stopping() || hub.control.is_ended() {
            let _ = publisher.close();
            continue;
        }
        let started = Instant::now();
        let end = match hub.control.subscribe(hub.ring) {
            Ok(mut rx) => send_all(hub, d, &mut publisher, &mut rx),
            Err(e) => End::Failed(Failure::new(Cause::Other, format!("no room for the destination's packets: {e}"))),
        };
        // The connection ends whichever way sending ended; a lost one may not take the close.
        let _ = publisher.close();
        match end {
            // Unless it was started again meanwhile, the next turn ends the thread.
            End::Done => {}
            End::Failed(e) => return d.fail(e),
            End::Lost(e) => {
                d.lost.store(true, Ordering::Relaxed);
                d.reconnects.fetch_add(1, Ordering::Relaxed);
                // A connection lost soon after it was made counts as another failed try.
                failures = if started.elapsed() >= hub.timing.stable { 1 } else { failures.saturating_add(1) };
                if hub.timing.gives_up(failures) {
                    return d.fail(gave_up(&hub.timing, e, failures));
                }
                last_error = Some(e);
            }
        }
    }
}

/// Packets a receiver lost: dropped when its ring was full, skipped as stale, or discarded.
fn lost(rx: &Receiver) -> u64 {
    let s = rx.stats();
    s.dropped_packets.saturating_add(s.skipped_packets).saturating_add(s.discarded_packets)
}

/// Send the headers, then every packet `rx` delivers, until the destination is stopped, the
/// stream ends or the connection fails.
fn send_all(hub: &Hub, d: &Dest, p: &mut Publisher, rx: &mut Receiver) -> End {
    let (bytes, dropped) = (d.bytes.load(Ordering::Relaxed), d.dropped.load(Ordering::Relaxed));
    let h = &hub.headers;
    let mut headers = || -> Result<(), craft_rtmp::Error> {
        let bad = |_| craft_rtmp::Error::BadInput("the encoder's configuration cannot be streamed");
        p.set_data_frame(&h.script)?;
        p.send_flv(0, &Body::AvcSequenceHeader(&h.avcc).check().map_err(bad)?)?;
        p.send_flv(0, &Body::AacSequenceHeader(&h.asc).check().map_err(bad)?)
    };
    if let Err(e) = headers() {
        return end_for(e);
    }
    d.set(DestinationStatus::Live);
    let mut zero = None;
    loop {
        if d.stopping() {
            return End::Done;
        }
        let sent = match rx.recv_timeout(RECV_WAIT) {
            Ok(packet) => send_packet(p, &packet, *zero.get_or_insert(packet.dts())),
            Err(RecvError::Closed) => return End::Done,
            // Nothing to send: read what the server sent, which also notices a closed connection.
            Err(RecvError::Timeout | RecvError::Empty) => p.poll(),
        };
        d.bytes.store(bytes.saturating_add(p.stats().bytes_sent), Ordering::Relaxed);
        d.dropped.store(dropped.saturating_add(lost(rx)), Ordering::Relaxed);
        if let Err(e) = sent {
            return end_for(e);
        }
    }
}

/// Milliseconds from `zero` to `t`, both in nanoseconds.
fn ms_since(zero: i64, t: i64) -> i64 {
    t.saturating_sub(zero).div_euclid(NS_PER_MS)
}

/// The FLV body of `packet` and its timestamp in milliseconds from `zero`; `None` for a packet
/// that is neither video nor audio. A timestamp before `zero` is sent as 0.
fn body(packet: &SharedPacket, zero: i64) -> Option<(u32, Body<'_>)> {
    let dts = ms_since(zero, packet.dts()).max(0);
    let timestamp = u32::try_from(dts).unwrap_or(u32::MAX);
    let body = match packet.kind() {
        Kind::Video => {
            let cts = ms_since(zero, packet.pts()).saturating_sub(dts).clamp(i64::from(MIN_COMPOSITION_TIME), i64::from(MAX_COMPOSITION_TIME));
            Body::AvcNalus { key: packet.keyframe(), composition_time: i32::try_from(cts).unwrap_or(0), data: packet.data() }
        }
        Kind::Audio => Body::AacRaw(packet.data()),
        Kind::Data => return None,
    };
    Some((timestamp, body))
}

fn send_packet(p: &mut Publisher, packet: &SharedPacket, zero: i64) -> Result<(), craft_rtmp::Error> {
    let Some((timestamp, body)) = body(packet, zero) else { return Ok(()) };
    let checked = body.check().map_err(|_| craft_rtmp::Error::BadInput("the encoder gave a packet that a stream cannot carry"))?;
    p.send_flv(timestamp, &checked)
}

/// The third thread's sink: each packet is shared and offered to every destination.
struct Publish {
    fanout: Fanout,
    pool: PacketPool,
    /// Packets no destination got because they could not be shared, and the video packets left
    /// out after one until the next keyframe.
    refused: Arc<AtomicU64>,
    /// A video packet could not be shared, so video waits for the next keyframe.
    video_gap: bool,
}

impl Sink for Publish {
    fn write(&mut self, item: Item) -> Result<(), String> {
        let video = item.track == Track::Video;
        if video && self.video_gap && !item.keyframe {
            // The frames up to the next keyframe may refer to the one left out.
            self.refused.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        let info = if video { Info::video(item.pts_ns, item.dts_ns, item.keyframe) } else { Info::audio(item.pts_ns) };
        let mut data = item.data;
        match self.fanout.share(&mut self.pool, &mut data, info) {
            Ok(packet) => {
                self.video_gap &= !video;
                self.fanout.publish(&packet);
            }
            Err(_) => {
                self.refused.fetch_add(1, Ordering::Relaxed);
                self.video_gap |= video;
            }
        }
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<(), String> {
        // Dropping the fan-out ends every subscription once its ring is sent.
        Ok(())
    }
}

/// A stream through the built-in encoders to its destinations.
pub struct Streaming {
    native: Native,
    hub: Arc<Hub>,
    stats: Arc<EncoderStats>,
    refused: Arc<AtomicU64>,
    stopped: bool,
}

impl Streaming {
    /// Open the encoders for `job` and start sending to each enabled destination of `targets`.
    /// Starts the stream to `targets`, each trying again as `reconnect` says.
    pub fn start(job: &OutputJob, targets: Vec<Target>, reconnect: ReconnectSettings) -> Result<Streaming, PluginError> {
        reconnect.check().map_err(PluginError::Failed)?;
        Self::start_timed(job, targets, Timing::from(reconnect))
    }

    pub(crate) fn start_timed(job: &OutputJob, targets: Vec<Target>, timing: Timing) -> Result<Streaming, PluginError> {
        let RateControl::Bitrate { kbps, .. } = job.rate else {
            return Err(PluginError::Failed("streams need a bitrate".into()));
        };
        let encoder = &job.video_encoder;
        if encoder.kind() != EncoderKind::Video || !encoder.streams() {
            return Err(PluginError::Failed(format!("streams need an H.264 encoder, and {} is not one", encoder.name())));
        }
        if encoder.ffmpeg().is_some() {
            return Err(PluginError::Failed(format!(
                "streams run the built-in encoders, and {} runs inside ffmpeg; choose another encoder in Settings › Stream",
                encoder.name()
            )));
        }
        check_targets(&targets)?;
        let v = job.video;
        let fps = f64::from(v.fps_num) / f64::from(v.fps_den.max(1));
        let rates = StreamRates {
            video_packets_per_second: (fps.ceil() as u32).max(1),
            audio_packets_per_second: AUDIO_PACKETS_PER_SECOND,
            bits_per_second: (u64::from(kbps) + u64::from(job.audio_bitrate_kbps)) * 1000,
            ticks_per_second: 1_000_000_000,
        };
        let mut ring = RingConfig::for_stream(&rates, RING_SECONDS);
        // At the highest bitrates the ring holds fewer seconds.
        ring.bytes = ring.bytes.min(MAX_RING_BYTES);
        ring.check().map_err(|e| PluginError::Failed(format!("the stream's packets do not fit in memory: {e}")))?;
        let pool = PacketPool::new(POOL_PACKETS, 0).map_err(|e| PluginError::Failed(format!("no memory for the stream's packets: {e}")))?;
        let fanout = Fanout::new();
        let control = fanout.control();
        let refused = Arc::new(AtomicU64::new(0));
        let mut headers = None;
        let sink_refused = Arc::clone(&refused);
        let open = |tracks: Vec<TrackSpec>| -> Result<Box<dyn Sink>, PluginError> {
            headers = Some(Headers::new(job, kbps, &tracks)?);
            Ok(Box::new(Publish { fanout, pool, refused: sink_refused, video_gap: false }))
        };
        let native = Native::start_with(job, open, PacketQueue::new(QUEUE_BYTES, QUEUE_SPAN_NS), DISK_WAIT)?;
        let Some(headers) = headers else {
            return Err(PluginError::Failed("the stream's encoders gave no configuration".into()));
        };
        let counts = Arc::new(Counts::default());
        let dests = targets.into_iter().map(|t| Arc::new(Dest::new(t, Arc::clone(&counts)))).collect();
        let hub = Arc::new(Hub { control, headers, ring, timing, dests, counts });
        let mut s = Streaming { native, hub, stats: Arc::clone(&job.stats), refused, stopped: false };
        for d in s.hub.dests.iter().filter(|d| d.target.enabled) {
            if let Err(e) = spawn(&s.hub, d, None) {
                let _ = s.stop();
                return Err(e);
            }
        }
        Ok(s)
    }

    /// Packets no destination got because the shared pool was full.
    pub fn refused(&self) -> u64 {
        self.refused.load(Ordering::Relaxed)
    }

    /// Wait for the destinations to send what their rings hold and close, then ask those still
    /// running to end.
    fn close_destinations(&self) {
        for d in &self.hub.dests {
            d.wake();
        }
        let deadline = Instant::now() + self.hub.timing.close;
        while self.hub.dests.iter().any(|d| d.running()) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        for d in &self.hub.dests {
            if d.running() {
                // A thread in the middle of a connect ends when the connect returns.
                d.ask_stop();
            } else if let Some(t) = lock(&d.thread).take() {
                let _ = t.join();
            }
        }
    }
}

/// The first problem with `targets`: none, too many, an empty id, an id used twice, none enabled,
/// or an address or key of an enabled one that is not valid.
fn check_targets(targets: &[Target]) -> Result<(), PluginError> {
    if targets.is_empty() {
        return Err(PluginError::Failed("the stream has no destination; add a server in Settings › Stream".into()));
    }
    if targets.len() > MAX_SUBSCRIBERS {
        return Err(PluginError::Failed(format!("a stream can have at most {MAX_SUBSCRIBERS} destinations, not {}", targets.len())));
    }
    for (i, t) in targets.iter().enumerate() {
        if t.id.is_empty() {
            return Err(PluginError::Failed(format!("destination {} has no id", i + 1)));
        }
        if targets[..i].iter().any(|o| o.id == t.id) {
            return Err(PluginError::Failed(format!("the destination id `{}` is used twice", t.id)));
        }
        // One that is off is checked when it is started.
        if t.enabled
            && let Err(e) = t.config()
        {
            return Err(PluginError::Failed(format!("destination `{}`: {e}", t.id)));
        }
    }
    if !targets.iter().any(|t| t.enabled) {
        return Err(PluginError::Failed("every destination is off; turn one on in Settings › Stream".into()));
    }
    Ok(())
}

impl OutputInstance for Streaming {
    fn video(&self) -> Arc<Fifo<Arc<PooledFrame>>> {
        self.native.video()
    }

    fn audio(&self) -> Arc<Fifo<PooledBlock>> {
        self.native.audio()
    }

    fn status(&self) -> OutputStatus {
        let total = self.hub.dests.iter().map(|d| d.bytes.load(Ordering::Relaxed)).fold(0u64, u64::saturating_add);
        self.stats.bytes.store(total, Ordering::Relaxed);
        let s = self.native.status();
        if !matches!(s, OutputStatus::Starting | OutputStatus::Running) {
            return s;
        }
        // The stream fails once a destination has failed and none is sending or trying to. While
        // one sends or tries, the check reads each status in place, without copying.
        let quiet = |st: &DestinationStatus| matches!(st, DestinationStatus::Failed(_) | DestinationStatus::Stopped);
        if self.hub.dests.iter().any(|d| !quiet(&lock(&d.status))) {
            return s;
        }
        let states: Vec<DestinationStatus> = self.hub.dests.iter().map(|d| d.status()).collect();
        let active = states.iter().any(|st| !matches!(st, DestinationStatus::Failed(_) | DestinationStatus::Stopped));
        let failed: Vec<(&str, &str)> = self
            .hub
            .dests
            .iter()
            .zip(&states)
            .filter_map(|(d, st)| match st {
                DestinationStatus::Failed(f) => Some((d.target.id.as_str(), f.detail.as_str())),
                _ => None,
            })
            .collect();
        if active || failed.is_empty() {
            return s;
        }
        if let ([(_, e)], 1) = (failed.as_slice(), self.hub.dests.len()) {
            return OutputStatus::Failed((*e).to_string());
        }
        let each: Vec<String> = failed.iter().map(|(id, e)| format!("{id}: {e}")).collect();
        let lead = if failed.len() == self.hub.dests.len() { "every destination failed" } else { "no destination is sending" };
        OutputStatus::Failed(format!("{lead} ({})", each.join("; ")))
    }

    /// The addresses of the destinations that send or try to, as status shows them.
    fn target(&self) -> String {
        let quiet = |d: &Dest| matches!(*lock(&d.status), DestinationStatus::Stopped | DestinationStatus::Failed(_));
        let on: Vec<String> = self.hub.dests.iter().filter(|d| !quiet(d)).map(|d| d.shown()).collect();
        on.join(", ")
    }

    fn stop(&mut self) -> Result<(), PluginError> {
        if self.stopped {
            return Ok(());
        }
        // A stream whose destinations all failed ends with that failure.
        let failed = match self.status() {
            OutputStatus::Failed(e) => Some(e),
            _ => None,
        };
        self.stopped = true;
        // The encoders finish, and the last packets go to the rings; then the fan-out ends.
        let r = self.native.stop();
        self.close_destinations();
        r?;
        failed.map_or(Ok(()), |e| Err(PluginError::Failed(e)))
    }

    fn destinations(&self) -> Vec<DestinationState> {
        self.hub.dests.iter().map(|d| d.state()).collect()
    }

    fn reconnects(&self) -> u64 {
        self.hub.dests.iter().map(|d| u64::from(d.reconnects.load(Ordering::Relaxed))).sum()
    }

    fn destination_counts(&self) -> DestinationCounts {
        let c = &self.hub.counts;
        DestinationCounts { live: c.live.load(Ordering::Relaxed), retrying: c.retrying.load(Ordering::Relaxed) }
    }

    fn recoveries(&self) -> u64 {
        self.hub.dests.iter().map(|d| u64::from(d.recovered.load(Ordering::SeqCst))).sum()
    }

    fn start_destination(&self, id: &str, key: Option<&str>) -> Result<(), PluginError> {
        let d = self.hub.find(id)?;
        if self.stopped || self.hub.control.is_ended() {
            return Err(PluginError::Failed("the stream is ending".into()));
        }
        let bad = |e: craft_rtmp::Error| PluginError::Failed(format!("destination `{id}`: {e}"));
        match key {
            Some(key) => d.with_key(key).config().map_err(bad)?,
            None => d.config().map_err(bad)?,
        };
        spawn(&self.hub, d, key)
    }

    fn stop_destination(&self, id: &str) -> Result<(), PluginError> {
        self.hub.find(id)?.ask_stop();
        Ok(())
    }
}

impl Drop for Streaming {
    fn drop(&mut self) {
        if !self.stopped {
            let _ = self.stop();
        }
    }
}

#[cfg(test)]
mod tests;
