//! A live stream through the built-in encoders, sent by this process to each of its destinations
//! over RTMP or RTMPS.
//!
//! - The video and audio encoder threads of [`crate::native`] run as for a recording. Its third
//!   thread hands each packet, in decoding order, to [`Publish`], which shares the packet once
//!   ([`craft_packet::PacketPool`]) and offers it to every destination ([`craft_packet::Fanout`]).
//!   Offering a packet never waits for a network.
//! - When the job has a caption feed, [`Publish`] takes the phrases waiting in it before each
//!   video packet, without waiting, and puts the CEA-608 roll-up caption pairs due for that
//!   picture into the packet as an SEI message ([`craft_closed_captions`]), before its first
//!   slice. Platforms that read captions embedded in H.264 show them as closed captions.
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
//!   stream, and an address or key that is not valid, are not tried again, with one exception: a
//!   destination whose connection was lost after the server took the stream, and that is then
//!   refused `publish` on a try again, is tried up to [`REFUSED_RETRIES`] more times, waiting at
//!   least [`REFUSED_WAIT`]. A destination stopped and started again does not count as one whose
//!   connection was lost.
//! - Each try looks the server's name up again, on a thread of its own, and waits at most
//!   [`LOOKUP_WAIT`] for the answer; a stop does not wait for it. A lookup the system never answers
//!   keeps its thread until the system returns, so each destination keeps at most [`MAX_LOOKUPS`]
//!   lookups outstanding and the process at most [`MAX_ALL_LOOKUPS`], and an answer that comes
//!   after its try gave up is not used.
//! - Before a try, a destination asks the system whether it has a network route to the server's
//!   last known addresses ([`studiocraft_platform::net::route_to`], which sends nothing), or, when
//!   none is known and the last try failed for a network cause, to any address. With no route it
//!   waits without connecting ([`DestinationStatus::WaitingForNetwork`]). One thread per stream
//!   checks the route of every waiting destination each [`NET_POLL`], exists only while a
//!   destination waits, and wakes each one whose route returns, which then tries at once. Each
//!   longest wait spent without a route counts as a failed try.
//! - When [`CHECK_AFTER_FAILURES`] tries of a destination fail in a row, or it gives up, its
//!   connection is checked once ([`diagnose`]) until it goes live again or is started again.
//! - A failure of one destination stops neither the other destinations nor the encoders. The
//!   stream fails when every destination has failed.
//! - A destination can be stopped and started again while the stream runs. A destination the
//!   stream was not started with can be added and started while it runs, and one that is not
//!   sending can be started with another server, key or service; it sends from the next keyframe.
//! - A destination can run a video encoder of its own ([`OwnEncoder`]), with its own encoder and
//!   bitrate, in place of sending what the stream's encoder makes. Destinations that name the same
//!   encoder, settings and bitrate share one. Each such encoder encodes the same frames at the same
//!   size and keyframe interval, with its own sound encoder beside it, and has its own fan-out,
//!   threads and ring size; a thread of the stream (`split.rs`) hands it the stream's input. One that fails, or cannot
//!   start, fails its destinations and stops neither the others nor the stream. The stream's
//!   encoder runs for as long as the stream does, and an encoder a destination started runs until
//!   the stream stops.
//! - A stream started with a copy also writes the packets the destinations get to a Matroska
//!   file ([`copy`]): what the stream's encoder makes. The copy takes one of the
//!   [`MAX_SUBSCRIBERS`] subscriptions, and its failure stops neither the destinations nor the
//!   encoders.
//!
//! No log line, error or status of this module holds a stream key: the keys go to
//! [`craft_rtmp::Config`] and nowhere else, and that crate keeps them out of its errors.

use std::collections::VecDeque;
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use craft_closed_captions::sei::{self, Codec};
use craft_closed_captions::{Config as CaptionConfig, MAX_PAIRS_PER_FRAME, RollUp};
use craft_flv::{Body, MAX_COMPOSITION_TIME, MIN_COMPOSITION_TIME, Metadata};
use craft_media::matroska::TrackSpec;
use craft_packet::{
    Control, Fanout, Info, Kind, MAX_RING_BYTES, MAX_SUBSCRIBERS, PacketPool, Receiver, RecvError, RingConfig, SharedPacket, StreamRates,
};
use craft_rtmp::{Config, Publisher};
use studiocraft_encode::h264::RECORDING_KEYFRAME_S;
use studiocraft_media::Tick;
use studiocraft_media::audio::{PooledBlock, SAMPLE_RATE};
use studiocraft_media::queue::Fifo;
use studiocraft_media::stats::EncoderStats;
use studiocraft_media::video::PooledFrame;
use studiocraft_model::services::Service;
use studiocraft_model::{ReconnectSettings, VIDEO_BITRATE_KBPS as VIDEO_KBPS};
use studiocraft_platform::net::Route;
use studiocraft_plugin::{
    AddressOutcome, Cause, DestinationCounts, DestinationSetup, DestinationState, DestinationStatus, EncoderKind, Failure, OutputInstance, OutputJob,
    OutputStatus, OwnEncoder, PluginError, RateControl, StreamCopyState,
};

use crate::native::{DISK_WAIT, FailureWatch, Item, Native, PacketQueue, QUEUE_BYTES, QUEUE_SPAN_NS, Sink, Track};
use crate::writer::FINISH_TIMEOUT;
use copy::StreamCopy;
use split::{Input, Split};

pub mod copy;
pub mod diagnose;
pub mod ingests;
mod split;

/// Seconds of the stream each destination's ring holds.
pub const RING_SECONDS: u32 = 4;
/// The most a wait before a try again is cut by, in percent, so destinations that lost their
/// connections together do not all try again at the same moment.
pub const JITTER_PERCENT: u64 = 20;
/// The shortest wait the random cut leaves.
const MIN_JITTERED_WAIT: Duration = Duration::from_secs(1);
/// How long a connection must last before its loss starts a new count of failed tries.
pub const STABLE_AFTER: Duration = Duration::from_secs(30);
/// Tries again after a refused `publish` that follows a lost connection. Each one counts toward
/// the failed tries in a row that [`ReconnectSettings::max_tries`] allows.
pub const REFUSED_RETRIES: u32 = 3;
/// The shortest wait before a try again after such a refusal.
pub const REFUSED_WAIT: Duration = Duration::from_secs(5);
/// How long a try waits for the server's name to be looked up.
pub const LOOKUP_WAIT: Duration = Duration::from_secs(10);
/// The most lookups of one destination that may be outstanding: the one of the current try, and
/// one an earlier try gave up on that the system has not answered yet.
pub const MAX_LOOKUPS: u32 = 2;
/// The most lookups that may be outstanding in the process, across every stream started and
/// stopped: a lookup the system never answers outlives its stream.
pub const MAX_ALL_LOOKUPS: u32 = 32;
/// Lookups outstanding in the process.
static ALL_LOOKUPS: AtomicU32 = AtomicU32::new(0);
/// How often a wait for a lookup looks at the clock again when nothing wakes it.
const LOOKUP_POLL: Duration = Duration::from_millis(50);
/// How often a stream's route thread checks the route of the destinations waiting for the
/// network, StudioCraft's own value.
pub const NET_POLL: Duration = Duration::from_secs(1);
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
#[derive(Clone, PartialEq)]
pub struct Target {
    pub id: String,
    /// `rtmp://host/app` or `rtmps://host/app`.
    pub server: String,
    /// The stream key; empty when the server's address ends with the stream's name.
    pub key: String,
    /// Whether the stream sends to it from the start.
    pub enabled: bool,
    /// The video encoder it runs for itself; `None` to send what the stream's encoder makes.
    pub encoder: Option<OwnEncoder>,
}

impl fmt::Debug for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let key = if self.key.is_empty() { "" } else { "<redacted>" };
        f.debug_struct("Target")
            .field("id", &self.id)
            .field("server", &self.shown())
            .field("key", &key)
            .field("enabled", &self.enabled)
            .field("encoder", &self.encoder)
            .finish()
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
    /// The shortest wait before a try again after a refused `publish`.
    pub(crate) refused: Duration,
    /// Whether each wait is cut by a random amount; tests turn it off.
    pub(crate) jitter: bool,
    pub(crate) connect: Duration,
    pub(crate) setup: Duration,
    pub(crate) stall: Duration,
    pub(crate) close: Duration,
    pub(crate) stable: Duration,
    /// How long a try waits for the server's name to be looked up.
    pub(crate) lookup: Duration,
    /// How often the route of the destinations waiting for the network is checked.
    pub(crate) net_poll: Duration,
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
            refused: REFUSED_WAIT,
            jitter: true,
            connect: craft_rtmp::DEFAULT_CONNECT_TIMEOUT,
            setup: craft_rtmp::DEFAULT_SETUP_TIMEOUT,
            stall: craft_rtmp::DEFAULT_STALL_TIMEOUT,
            close: CLOSE_WAIT,
            stable: STABLE_AFTER,
            lookup: LOOKUP_WAIT,
            net_poll: NET_POLL,
        }
    }
}

/// Where the destinations read the time for the lookup wait and the wait for the network. Tests
/// pass a clock they move on.
pub(crate) trait Clock: Send + Sync {
    fn now(&self) -> Instant;
}

/// The system's clock.
struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// Looks a host name and port up. Tests pass one that answers as they choose.
pub(crate) type Resolve = dyn Fn(&str, u16) -> std::io::Result<Vec<std::net::SocketAddr>> + Send + Sync;

/// The system resolver.
fn system_resolve(host: &str, port: u16) -> std::io::Result<Vec<std::net::SocketAddr>> {
    use std::net::ToSocketAddrs;
    Ok((host, port).to_socket_addrs()?.collect())
}

/// Whether the system has a route to any of the addresses, or to any address when there are
/// none. Tests pass one whose answer they set.
pub(crate) type RouteCheck = dyn Fn(&[std::net::SocketAddr]) -> Route + Send + Sync;

/// What a destination's thread uses from outside: the clock, the resolver and the route check.
#[derive(Clone)]
pub(crate) struct Seams {
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) resolve: Arc<Resolve>,
    pub(crate) route: Arc<RouteCheck>,
}

impl Default for Seams {
    fn default() -> Self {
        Seams { clock: Arc::new(SystemClock), resolve: Arc::new(system_resolve), route: Arc::new(studiocraft_platform::net::route_to) }
    }
}

/// How many tries in a row fail before a destination's connection is checked
/// ([`diagnose`]), once until it goes live again or is started again. A destination that gives up
/// sooner is checked when it gives up.
pub const CHECK_AFTER_FAILURES: u32 = 3;

/// What a stream's automatic connection checks need: the service of each destination set up for
/// one, and whether the checks also contact the service's other hosts.
#[derive(Clone, Debug, Default)]
pub struct AutoCheck {
    /// The key the stream's checks are kept under ([`diagnose::Subject::profile`]).
    pub profile: String,
    /// Destination ids and their services.
    pub services: Vec<(String, &'static Service)>,
    /// Whether the checks also read the service's list of ingest servers and status summary and
    /// connect to its other ingest servers.
    pub service_hosts: bool,
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

/// How many destinations of a stream are live and how many wait to try again, waiting for the
/// network included, kept as each destination's status is set.
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
            DestinationStatus::Retrying { .. } | DestinationStatus::WaitingForNetwork { .. } => Some(&self.retrying),
            _ => None,
        }
    }
}

/// What a destination sends with. A change replaces the whole setup at once, so a connection
/// never pairs the key of one setup with the server of another.
#[derive(Clone)]
struct Setup {
    /// The server it sends to: the target's, or one given when it was started again.
    server: String,
    /// The stream key it sends with: the target's, or one given when it was started again.
    key: String,
    /// The streaming service it is set up for, for its automatic connection checks.
    service: Option<&'static Service>,
}

/// A destination and what its thread reports.
struct Dest {
    target: Target,
    /// The stream's counts, shared by its destinations.
    counts: Arc<Counts>,
    /// The server, stream key and service it sends with, read without a lock and replaced
    /// together.
    setup: ArcSwap<Setup>,
    /// The encoder whose packets it sends: the stream's, or one it runs for itself.
    feed: Mutex<Arc<EncoderFeed>>,
    /// The encoder it is set up to run for itself; `None` to send the stream's. A start gives it
    /// the encoder this names.
    own: Mutex<Option<OwnEncoder>>,
    status: Mutex<DestinationStatus>,
    /// How many times the status was set, with a signal for each.
    changes: Mutex<u64>,
    changed: Condvar,
    /// Bytes sent over every connection so far.
    bytes: AtomicU64,
    /// Packets lost over every connection so far.
    dropped: AtomicU64,
    reconnects: AtomicU32,
    /// It was started again while its thread still ran; the thread's next try is a first one, and
    /// its failed tries are counted from none.
    restarted: AtomicBool,
    /// It was started again with another server or key while its thread still ran: the
    /// connection made or being made before that closes, and the next one uses the new setup.
    renew: AtomicBool,
    /// Lookups of the server's name whose threads have not ended.
    lookups: AtomicU32,
    /// The server's addresses from the last lookup that answered; empty before one.
    addrs: Mutex<Vec<std::net::SocketAddr>>,
    /// The server is given as an IP address, so a lookup cannot give it another address.
    fixed: AtomicBool,
    /// Set by the route thread when a route to the server returned while the destination waited.
    net_up: AtomicBool,
    /// The last connection was lost, and no connection has gone live since.
    lost: AtomicBool,
    /// Times it went live again after a lost connection.
    recovered: AtomicU32,
    /// Its connection was checked after failures since it last went live or was started. Shared
    /// with a check that waits for a place, which clears it when it cannot run.
    checked: Arc<AtomicBool>,
    /// Whether its latest check is still wanted, shared with that check while it waits for a
    /// place; cleared with `checked`.
    wanted: Mutex<Arc<Mutex<bool>>>,
    /// Asks the thread to close the connection and end.
    stop: AtomicBool,
    /// The thread runs and has not decided to end. It is changed with `thread` locked, so a start
    /// either reaches a thread that goes on or finds none.
    alive: AtomicBool,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Dest {
    fn new(target: Target, counts: Arc<Counts>, service: Option<&'static Service>, feed: Arc<EncoderFeed>) -> Dest {
        Dest {
            setup: ArcSwap::from_pointee(Setup { server: target.server.clone(), key: target.key.clone(), service }),
            feed: Mutex::new(feed),
            own: Mutex::new(target.encoder.clone()),
            target,
            counts,
            status: Mutex::new(DestinationStatus::Stopped),
            changes: Mutex::new(0),
            changed: Condvar::new(),
            bytes: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            reconnects: AtomicU32::new(0),
            restarted: AtomicBool::new(false),
            renew: AtomicBool::new(false),
            lookups: AtomicU32::new(0),
            addrs: Mutex::new(Vec::new()),
            fixed: AtomicBool::new(false),
            net_up: AtomicBool::new(false),
            lost: AtomicBool::new(false),
            recovered: AtomicU32::new(0),
            checked: Arc::new(AtomicBool::new(false)),
            wanted: Mutex::new(Arc::new(Mutex::new(false))),
            stop: AtomicBool::new(false),
            alive: AtomicBool::new(false),
            thread: Mutex::new(None),
        }
    }

    /// The target with its server now and `key` as its stream key.
    fn with_key(&self, key: &str) -> Target {
        self.with(&self.setup.load().server, key)
    }

    /// The target with `server` and `key`, for its address: without its encoder.
    fn with(&self, server: &str, key: &str) -> Target {
        Target { id: self.target.id.clone(), server: server.to_string(), key: key.to_string(), enabled: self.target.enabled, encoder: None }
    }

    /// The encoder whose packets it sends.
    fn feed(&self) -> Arc<EncoderFeed> {
        Arc::clone(&lock(&self.feed))
    }

    /// Why it cannot send: the encoder it runs for itself ended while the stream goes on. `None`
    /// while that encoder runs, and for one that sends what the stream's encoder makes.
    fn feed_ended(&self, hub: &Hub) -> Option<Failure> {
        let feed = self.feed();
        if feed.own.is_none() || !feed.control.is_ended() || hub.ending.load(Ordering::SeqCst) || hub.control.is_ended() {
            return None;
        }
        let detail = match feed.failure.get() {
            Some(e) => format!("its video encoder stopped: {e}"),
            None => "its video encoder stopped".to_string(),
        };
        Some(Failure::new(Cause::Other, detail))
    }

    /// Its setup now: the server, key and service from one change.
    fn setup(&self) -> Arc<Setup> {
        self.setup.load_full()
    }

    /// The target with its server and key now, both from one setup.
    fn current(&self) -> Target {
        let s = self.setup();
        self.with(&s.server, &s.key)
    }

    fn config(&self) -> Result<Config, craft_rtmp::Error> {
        self.current().config()
    }

    fn shown(&self) -> String {
        self.current().shown()
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
        if *status == DestinationStatus::Live {
            self.uncheck();
        }
        drop(status);
        self.touch();
    }

    /// Its connection may be checked again after failures, and a check of it that waits for a
    /// place is dropped. A check that is starting at that moment finishes starting first.
    fn uncheck(&self) {
        // Held while `checked` changes, as in `check_after`.
        let wanted = lock(&self.wanted);
        self.checked.store(false, Ordering::SeqCst);
        *lock(&wanted) = false;
    }

    /// Counts a change and signals it.
    fn touch(&self) {
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

/// One video encoder of a stream, with the sound encoded beside it, and what a destination needs
/// to send what it makes.
struct EncoderFeed {
    /// The encoder a destination runs for itself; `None` for the stream's encoder.
    own: Option<OwnEncoder>,
    control: Control,
    headers: Headers,
    ring: RingConfig,
    /// Why its encoders failed, when they did.
    failure: FailureWatch,
}

/// What the destinations share.
struct Hub {
    /// The fan-out of the stream's encoder: once it has ended, the stream is ending.
    control: Control,
    /// The stream's encoder.
    main: Arc<EncoderFeed>,
    /// The stream was asked to stop: an encoder's end is not a failure of its destinations.
    ending: AtomicBool,
    timing: Timing,
    seams: Seams,
    /// The destinations it was started with, in order, then those added while it runs. At most
    /// [`MAX_SUBSCRIBERS`].
    dests: Mutex<Vec<Arc<Dest>>>,
    /// What the destinations that gave their place to added ones had sent and made again.
    left: Mutex<Left>,
    counts: Arc<Counts>,
    net: NetWatch,
    /// Where the automatic connection checks run, and what they need; `None` turns them off.
    auto: Option<(Arc<diagnose::Checks>, AutoCheck)>,
}

/// The counts of destinations no longer in a stream's list, so neither the stream's sums nor the
/// counts of one added again under its id go down.
#[derive(Default)]
struct Left {
    /// The last [`MAX_SUBSCRIBERS`] that left, oldest first, each by id.
    each: VecDeque<(String, Totals)>,
    /// Those that left before them, summed.
    older: Totals,
}

/// What a destination sent and lost, and how often it made its connection again.
#[derive(Clone, Copy, Default)]
struct Totals {
    bytes: u64,
    dropped: u64,
    reconnects: u64,
    recovered: u64,
}

impl Totals {
    fn of(d: &Dest) -> Totals {
        Totals {
            bytes: d.bytes.load(Ordering::Relaxed),
            dropped: d.dropped.load(Ordering::Relaxed),
            reconnects: u64::from(d.reconnects.load(Ordering::Relaxed)),
            recovered: u64::from(d.recovered.load(Ordering::SeqCst)),
        }
    }

    fn add(&mut self, o: Totals) {
        self.bytes = self.bytes.saturating_add(o.bytes);
        self.dropped = self.dropped.saturating_add(o.dropped);
        self.reconnects = self.reconnects.saturating_add(o.reconnects);
        self.recovered = self.recovered.saturating_add(o.recovered);
    }
}

impl Left {
    /// Everything that left, summed.
    fn sum(&self) -> Totals {
        let mut t = self.older;
        for (_, each) in &self.each {
            t.add(*each);
        }
        t
    }
}

/// The destinations of a stream that wait for the network, and its route thread.
#[derive(Default)]
struct NetWatch {
    /// The waiting destinations, and whether the route thread runs.
    state: Mutex<(Vec<Arc<Dest>>, bool)>,
    /// Route threads started, for the tests.
    threads: AtomicU32,
}

impl Hub {
    fn find(&self, id: &str) -> Result<Arc<Dest>, PluginError> {
        lock(&self.dests)
            .iter()
            .find(|d| d.target.id == id)
            .cloned()
            .ok_or_else(|| PluginError::Failed(format!("the stream has no destination `{id}`")))
    }

    /// The destination `id`, added when the stream has none of that id. When the stream already
    /// has [`MAX_SUBSCRIBERS`] destinations, an added one takes the place of one whose thread has
    /// ended, a stopped one before a failed one; with none, it is an error.
    fn find_or_add(&self, id: &str, service: Option<&'static Service>, feed: &Arc<EncoderFeed>) -> Result<Arc<Dest>, PluginError> {
        let mut dests = lock(&self.dests);
        if let Some(d) = dests.iter().find(|d| d.target.id == id) {
            return Ok(Arc::clone(d));
        }
        let ended = |stopped: bool| move |o: &Arc<Dest>| !o.running() && (o.status() == DestinationStatus::Stopped) == stopped;
        let place = if dests.len() < MAX_SUBSCRIBERS {
            None
        } else {
            match dests.iter().position(ended(true)).or_else(|| dests.iter().position(ended(false))) {
                Some(i) => Some(i),
                None => return Err(PluginError::Failed(format!("a stream can have at most {MAX_SUBSCRIBERS} destinations"))),
            }
        };
        let target = Target { id: id.to_string(), server: String::new(), key: String::new(), enabled: true, encoder: feed.own.clone() };
        let d = Arc::new(Dest::new(target, Arc::clone(&self.counts), service, Arc::clone(feed)));
        // Changed with the list held, so a sum read with it held counts each destination once.
        let mut left = lock(&self.left);
        // One that left under this id goes on from its counts.
        if let Some(i) = left.each.iter().position(|(i, _)| i == id)
            && let Some((_, t)) = left.each.remove(i)
        {
            d.bytes.store(t.bytes, Ordering::Relaxed);
            d.dropped.store(t.dropped, Ordering::Relaxed);
            d.reconnects.store(u32::try_from(t.reconnects).unwrap_or(u32::MAX), Ordering::Relaxed);
            d.recovered.store(u32::try_from(t.recovered).unwrap_or(u32::MAX), Ordering::SeqCst);
        }
        let Some(i) = place else {
            dests.push(Arc::clone(&d));
            return Ok(d);
        };
        let old = std::mem::replace(&mut dests[i], Arc::clone(&d));
        left.each.push_back((old.target.id.clone(), Totals::of(&old)));
        if left.each.len() > MAX_SUBSCRIBERS
            && let Some((_, t)) = left.each.pop_front()
        {
            left.older.add(t);
        }
        drop(left);
        drop(dests);
        if let Some(t) = lock(&old.thread).take() {
            let _ = t.join();
        }
        Ok(d)
    }
}

/// The parts of a destination's setup a start may give it: its stream key, its server and service,
/// and the encoder whose packets it sends. `None` keeps what it has.
#[derive(Default)]
struct Given<'a> {
    key: Option<&'a str>,
    server: Option<(&'a str, Option<&'static Service>)>,
    feed: Option<&'a Arc<EncoderFeed>>,
}

/// Start the thread of `d`, unless it is running, with the parts of `given`. A running thread
/// keeps its setup, unless it was asked to stop and has not ended yet. Says whether `d` took
/// `given`: not when its thread was sending.
fn spawn(hub: &Arc<Hub>, d: &Arc<Dest>, given: Given<'_>) -> Result<bool, PluginError> {
    let mut slot = lock(&d.thread);
    if d.running() {
        // Asked to stop and started again before the thread decided to end: it goes on, and
        // tries at once, with the new setup when it has one. A start of one that was not asked
        // to stop changes nothing. Only `spawn` and `ask_stop` change `stop`, both with `thread`
        // locked, so it stays set until it is cleared below.
        if !d.stopping() {
            return Ok(false);
        }
        // The new setup and `renew` are in place before `stop` is cleared, so the thread does not
        // go on to connect before the setup changes. A connection it made or is making with the
        // old setup closes before it sends.
        if set_up(d, given) {
            d.renew.store(true, Ordering::SeqCst);
        }
        d.stop.store(false, Ordering::SeqCst);
        d.restarted.store(true, Ordering::SeqCst);
        // A check that waits for a place from before the stop is dropped.
        d.uncheck();
        if let Some(t) = slot.as_ref() {
            t.thread().unpark();
        }
        return Ok(true);
    }
    if let Some(t) = slot.take() {
        // The thread has decided to end, so it ends without waiting for anything.
        let _ = t.join();
    }
    set_up(d, given);
    d.renew.store(false, Ordering::SeqCst);
    d.stop.store(false, Ordering::SeqCst);
    d.uncheck();
    d.alive.store(true, Ordering::SeqCst);
    d.set(DestinationStatus::Connecting);
    let (h, dd) = (Arc::clone(hub), Arc::clone(d));
    match std::thread::Builder::new().name("stream destination".into()).spawn(move || run(&h, &dd)) {
        Ok(t) => {
            *slot = Some(t);
            Ok(true)
        }
        Err(e) => {
            let message = format!("no thread for the destination: {e}");
            d.alive.store(false, Ordering::SeqCst);
            d.set(DestinationStatus::Failed(Failure::new(Cause::Other, message.clone())));
            Err(PluginError::Failed(message))
        }
    }
}

/// Gives `d` the parts of `given`: `key` as its stream key, `server` as its server and service,
/// and `feed` as the encoder whose packets it sends. Says whether its server, key or encoder
/// changed; the service is used only by its connection checks, so a connection does not close
/// for it. The key, server and service are published as one new setup, so no reader sees some of
/// them changed and not the others. Only `spawn` calls it, with `thread` locked, so no other
/// change comes between the read and the publish.
fn set_up(d: &Dest, given: Given<'_>) -> bool {
    let Given { key, server, feed } = given;
    let mut now = Setup::clone(&d.setup.load());
    let mut changed = false;
    let mut moved = false;
    if let Some(feed) = feed {
        let mut held = lock(&d.feed);
        if !Arc::ptr_eq(&held, feed) {
            // Another encoder sends other sequence headers, so the connection starts again.
            *held = Arc::clone(feed);
            changed = true;
        }
    }
    if let Some(key) = key {
        changed |= now.key != key;
        now.key = key.to_string();
    }
    if let Some((server, service)) = server {
        if now.server != server {
            now.server = server.to_string();
            moved = true;
        }
        now.service = service;
    }
    d.setup.store(Arc::new(now));
    if moved {
        // The addresses of the server it sent to before are not this one's. They are cleared
        // after the new server is published, so a lookup of the old server that `connect` keeps
        // either sees the new server and drops its addresses, or is cleared here.
        let mut addrs = lock(&d.addrs);
        addrs.clear();
        d.fixed.store(false, Ordering::SeqCst);
    }
    changed | moved
}

/// Errors that a try again cannot fix: the address, the key or a setting is not valid, the
/// server refused the stream, or a packet cannot be sent.
fn is_final(e: &craft_rtmp::Error) -> bool {
    use craft_rtmp::Error as E;
    matches!(e, E::BadUrl(_) | E::BadKey(_) | E::BadConfig(_) | E::BadInput(_) | E::Rejected { .. })
}

/// Whether the server refused the `publish` command.
fn is_publish_refusal(e: &craft_rtmp::Error) -> bool {
    matches!(e, craft_rtmp::Error::Rejected { command: "publish", .. })
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

/// Starts the automatic connection check of `d` after `f`, unless one ran since it last went live
/// or was started, or `f` is an address or key that is not valid.
fn check_after(hub: &Hub, d: &Dest, f: &Failure) {
    let Some((checks, auto)) = &hub.auto else { return };
    // `checked` and this check's own flag change under the same lock as in `uncheck`, so a
    // restart or a recovery meanwhile either happens before both are set or clears this check's
    // flag. A check from before that still waits for a place is dropped.
    let wanted = {
        let mut held = lock(&d.wanted);
        if matches!(f.cause, Cause::BadAddress | Cause::BadKey) || d.checked.swap(true, Ordering::SeqCst) {
            return;
        }
        let wanted = Arc::new(Mutex::new(true));
        let before: Arc<Mutex<bool>> = std::mem::replace(&mut *held, Arc::clone(&wanted));
        *lock(&before) = false;
        wanted
    };
    let id = &d.target.id;
    let setup = d.setup();
    let subject = diagnose::Subject {
        profile: auto.profile.clone(),
        id: id.clone(),
        server: setup.server.clone(),
        service: setup.service,
        last: Some(f.clone()),
        service_hosts: auto.service_hosts,
        automatic: true,
        checked: Some(Arc::clone(&d.checked)),
        wanted: Some(wanted),
    };
    if let Err(e) = checks.start(subject) {
        log::info!("stream destination `{id}`: the connection was not checked: {e}");
        // A later failure tries again when another check of it ran, no thread was free, or too
        // many checks waited for a place. With fewer waiting, the check waits for one instead.
        if !matches!(e, diagnose::CheckError::BadAddress(_)) {
            d.checked.store(false, Ordering::SeqCst);
        }
    }
}

/// The thread gives up with `f`, and the connection is checked.
fn fail(hub: &Hub, d: &Dest, f: Failure) {
    check_after(hub, d, &f);
    d.fail(f);
}

/// Waits `delay`, or less when the destination is stopped or started again or the stream ends.
fn wait(hub: &Hub, d: &Dest, delay: Duration) {
    let deadline = Instant::now() + delay;
    loop {
        let now = Instant::now();
        if d.stopping() || d.restarted.load(Ordering::SeqCst) || hub.control.is_ended() || now >= deadline {
            return;
        }
        std::thread::park_timeout(deadline - now);
    }
}

/// What is left at `now` of the wait after a `publish` refused at `at`; nothing when the last try
/// was not refused.
fn refusal_left(timing: &Timing, at: Option<Instant>, now: Instant) -> Duration {
    at.map_or(Duration::ZERO, |at| timing.refused.saturating_sub(now.saturating_duration_since(at)))
}

/// Whether a failure of `cause` may come from this computer having no network: a missing route,
/// a lookup that failed or had no answer, or a connection that timed out.
fn network_cause(cause: Cause) -> bool {
    matches!(cause, Cause::NoRoute | Cause::LookupFailed | Cause::LookupNoAnswer | Cause::TimedOut)
}

/// Whether the system has a route to `d`'s server, checked before a try after `last`: to its last
/// known addresses, and, when they have none or none is known after a failure of a
/// [`network_cause`], to any address.
fn gate(hub: &Hub, d: &Dest, last: &Failure) -> Gate {
    let addrs = lock(&d.addrs).clone();
    if !addrs.is_empty() && (hub.seams.route)(&addrs) != Route::None {
        return Gate::Open;
    }
    if !addrs.is_empty() && d.fixed.load(Ordering::SeqCst) {
        return Gate::Closed;
    }
    // A lookup may give a named server an address that has a route, and a network may reach the
    // server without a default route, so the probes never stop the try of a named server.
    if (!addrs.is_empty() || network_cause(last.cause)) && (hub.seams.route)(&[]) == Route::None {
        return Gate::NoDefault;
    }
    Gate::Open
}

/// What the route check says about the next try.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Gate {
    /// Try at the usual time.
    Open,
    /// The server is given as an IP address with no route: wait until one returns.
    Closed,
    /// The server is named, its known addresses (if any) have no route, and the system has no
    /// default route: show the wait, and try at the usual time or as soon as a route returns.
    NoDefault,
}

/// How a wait for the network ended.
enum NetWait {
    /// A route returned: try at once.
    Up,
    /// The destination was started again: try at once, as a first try.
    Restarted,
    /// The destination was stopped or the stream ended.
    Stopped,
    /// The longest waits spent without a route used up the tries.
    GaveUp(Failure),
    /// The encoder the destination runs for itself ended while the stream goes on.
    EncoderEnded(Failure),
}

/// Waits until the system has a route to `d`'s server again or `d` is started again, or, with
/// `until`, at most that long, showing [`DestinationStatus::WaitingForNetwork`] with `last`. Each
/// longest wait spent without a route counts as a failed try in `failures`.
fn wait_for_network(hub: &Arc<Hub>, d: &Arc<Dest>, failures: &mut u32, last: &Failure, until: Option<Duration>) -> NetWait {
    // The wait starts before the status shows it, so a reader of the status never sees a later start.
    let start = hub.seams.clock.now();
    let mut since = start;
    d.net_up.store(false, Ordering::SeqCst);
    let watched = watch_network(hub, d);
    d.set(DestinationStatus::WaitingForNetwork { attempt: failures.saturating_add(1), failure: last.clone() });
    let ended = loop {
        if d.stopping() || hub.control.is_ended() {
            break NetWait::Stopped;
        }
        // Started again while it waits: it tries at once, as after the wait before a try.
        if d.restarted.load(Ordering::SeqCst) {
            break NetWait::Restarted;
        }
        // No route would let it send anything.
        if let Some(f) = d.feed_ended(hub) {
            break NetWait::EncoderEnded(f);
        }
        // Without a route thread, the destination checks for itself.
        if d.net_up.load(Ordering::SeqCst) || (!watched && gate(hub, d, last) == Gate::Open) {
            break NetWait::Up;
        }
        let now = hub.seams.clock.now();
        if until.is_some_and(|u| now.saturating_duration_since(start) >= u) {
            break NetWait::Up;
        }
        if now.saturating_duration_since(since) >= hub.timing.longest {
            *failures = failures.saturating_add(1);
            if hub.timing.gives_up(*failures) {
                break NetWait::GaveUp(gave_up(&hub.timing, last.clone(), *failures));
            }
            since = now;
            d.set(DestinationStatus::WaitingForNetwork { attempt: failures.saturating_add(1), failure: last.clone() });
        }
        std::thread::park_timeout(hub.timing.net_poll);
    };
    let mut state = lock(&hub.net.state);
    state.0.retain(|w| !Arc::ptr_eq(w, d));
    ended
}

/// Adds `d` to the destinations waiting for the network, and starts the route thread when none
/// runs. Whether a route thread watches.
fn watch_network(hub: &Arc<Hub>, d: &Arc<Dest>) -> bool {
    let mut state = lock(&hub.net.state);
    if !state.0.iter().any(|w| Arc::ptr_eq(w, d)) {
        state.0.push(Arc::clone(d));
    }
    if state.1 {
        return true;
    }
    let h = Arc::clone(hub);
    let started = std::thread::Builder::new().name("stream route".into()).spawn(move || route_thread(&h)).is_ok();
    if started {
        hub.net.threads.fetch_add(1, Ordering::SeqCst);
    }
    state.1 = started;
    started
}

/// Each [`Timing::net_poll`], checks the route of every destination waiting for the network and
/// wakes each one that has a route again. Ends when none waits or the stream ends.
fn route_thread(hub: &Hub) {
    loop {
        std::thread::park_timeout(hub.timing.net_poll);
        let waiting = {
            let mut state = lock(&hub.net.state);
            if state.0.is_empty() || hub.control.is_ended() {
                state.1 = false;
                return;
            }
            state.0.clone()
        };
        for d in waiting {
            let addrs = lock(&d.addrs).clone();
            // A named server is tried again once the system has any route, as `gate` decides.
            let named = !d.fixed.load(Ordering::SeqCst) && !addrs.is_empty();
            if (hub.seams.route)(&addrs) != Route::None || (named && (hub.seams.route)(&[]) != Route::None) {
                d.net_up.store(true, Ordering::SeqCst);
                d.wake();
            }
        }
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

/// Why a try did not connect.
enum Missed {
    /// The address, the key, the lookup or the connection failed.
    Rtmp(craft_rtmp::Error),
    /// The lookup gave no answer in time, or an earlier one has not finished.
    Lookup(Failure),
    /// The destination was stopped, or the stream ended, while the lookup ran.
    Stopped,
}

fn connect(hub: &Hub, d: &Arc<Dest>) -> Result<Publisher, Missed> {
    let setup = d.setup();
    let mut c = d.with(&setup.server, &setup.key).config().map_err(Missed::Rtmp)?;
    c.connect_timeout = hub.timing.connect;
    c.setup_timeout = hub.timing.setup;
    c.stall_timeout = hub.timing.stall;
    let addrs = lookup(hub, d, c.endpoint.host(), c.endpoint.port())?;
    {
        // Kept only while the server is the one looked up: a server given during the lookup has
        // its addresses cleared by `set_up`, and they stay clear until its own lookup. The server
        // is read with the addresses locked, and `set_up` publishes a new one before it clears
        // them with them locked, so a clearing is never undone here.
        let mut kept = lock(&d.addrs);
        if d.setup.load().server == setup.server {
            kept.clone_from(&addrs);
            d.fixed.store(c.endpoint.host().parse::<std::net::IpAddr>().is_ok(), Ordering::SeqCst);
        }
    }
    // A stop that came as the lookup answered does not wait for a TCP connect.
    if d.stopping() || hub.control.is_ended() {
        return Err(Missed::Stopped);
    }
    Publisher::connect_to(&c, &addrs).map_err(Missed::Rtmp)
}

/// Looks `host` up on a thread of its own and waits for the answer until the lookup time runs
/// out, the destination is stopped or the stream ends. The thread ends when the resolver returns,
/// whether or not the try still waits; an answer nobody waits for is dropped. An IP address is
/// used as it is, with no lookup, so the lookup limits do not apply to it.
fn lookup(hub: &Hub, d: &Arc<Dest>, host: &str, port: u16) -> Result<Vec<std::net::SocketAddr>, Missed> {
    use std::sync::mpsc::{TryRecvError, sync_channel};
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return Ok(vec![std::net::SocketAddr::new(ip, port)]);
    }
    // Only this destination's thread adds lookups, so the count cannot pass the limit between the
    // check and the add.
    if d.lookups.load(Ordering::SeqCst) >= MAX_LOOKUPS {
        return Err(Missed::Lookup(Failure::new(Cause::LookupsPending, "an earlier lookup of the server's name has not finished")));
    }
    if !reserve(&ALL_LOOKUPS, MAX_ALL_LOOKUPS) {
        return Err(Missed::Lookup(Failure::new(Cause::LookupsPending, "too many lookups of server names have not finished")));
    }
    let (wait, start) = (hub.timing.lookup, hub.seams.clock.now());
    let (tx, rx) = sync_channel(1);
    let waiter = std::thread::current();
    let (resolve, owner, name) = (Arc::clone(&hub.seams.resolve), Arc::clone(d), host.to_string());
    let clock = Arc::clone(&hub.seams.clock);
    d.lookups.fetch_add(1, Ordering::SeqCst);
    let spawned = std::thread::Builder::new().name("stream lookup".into()).spawn(move || {
        let answer = resolve(&name, port);
        // The answer carries when it came, so a try that wakes late still uses an answer in time.
        // Fails when the try stopped waiting, which drops the answer.
        let _ = tx.send((answer, clock.now()));
        ALL_LOOKUPS.fetch_sub(1, Ordering::SeqCst);
        owner.lookups.fetch_sub(1, Ordering::SeqCst);
        owner.touch();
        waiter.unpark();
    });
    if let Err(e) = spawned {
        ALL_LOOKUPS.fetch_sub(1, Ordering::SeqCst);
        d.lookups.fetch_sub(1, Ordering::SeqCst);
        return Err(Missed::Lookup(Failure::new(Cause::Other, format!("no thread for the lookup: {e}"))));
    }
    let late = || Missed::Lookup(Failure::new(Cause::LookupNoAnswer, format!("the server's name was not looked up within {} s", wait.as_secs())));
    // An answer counts when it came before the deadline, however late the try wakes to take it.
    let take = |(answer, at): (std::io::Result<Vec<std::net::SocketAddr>>, Instant)| {
        if at.saturating_duration_since(start) >= wait {
            return Err(late());
        }
        answer.map_err(|e| Missed::Rtmp(craft_rtmp::Error::Resolve(e)))
    };
    loop {
        let received = rx.try_recv();
        if d.stopping() || hub.control.is_ended() {
            return Err(Missed::Stopped);
        }
        match received {
            Ok(answer) => return take(answer),
            Err(TryRecvError::Disconnected) => {
                return Err(Missed::Lookup(Failure::new(Cause::LookupFailed, "the lookup of the server's name ended without an answer")));
            }
            Err(TryRecvError::Empty) => {}
        }
        let waited = hub.seams.clock.now().saturating_duration_since(start);
        if waited >= wait {
            // An answer that came while the clock was read may still be in time.
            return rx.try_recv().map_or_else(|_| Err(late()), take);
        }
        std::thread::park_timeout((wait - waited).min(LOOKUP_POLL));
    }
}

/// Adds one to `count` unless it already holds `max`, and says whether it added.
fn reserve(count: &AtomicU32, max: u32) -> bool {
    let mut n = count.load(Ordering::SeqCst);
    while n < max {
        match count.compare_exchange_weak(n, n + 1, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return true,
            Err(now) => n = now,
        }
    }
    false
}

/// The thread of one destination: connect, send, and try again when the connection is lost.
fn run(hub: &Arc<Hub>, d: &Arc<Dest>) {
    let _watch = Watch(d);
    let mut failures = 0u32;
    let mut last_error: Option<Failure> = None;
    // Whether the tries follow a connection the server took and that was then lost, the refused
    // `publish` answers since, and whether the last try was one.
    let mut after_loss = false;
    let mut refusals = 0u32;
    let mut refused = false;
    let mut random = seed(&d.target.id);
    loop {
        if let Some(failure) = last_error.take() {
            if failures >= CHECK_AFTER_FAILURES {
                check_after(hub, d, &failure);
            }
            let delay = hub.timing.delay(failures.max(1));
            let delay = if hub.timing.jitter { jittered(delay, next_random(&mut random)) } else { delay };
            let refused_at = std::mem::take(&mut refused).then(Instant::now);
            let delay = if refused_at.is_some() { delay.max(hub.timing.refused) } else { delay };
            // With no route, the wait for the network takes the place of the wait before the try.
            let mut route = gate(hub, d, &failure);
            if route == Gate::Open {
                d.set(DestinationStatus::Retrying { attempt: failures.saturating_add(1), failure: failure.clone() });
                wait(hub, d, delay);
                // Checked again right before the try: the route may have gone during the wait.
                route = match gate(hub, d, &failure) {
                    Gate::Closed => Gate::Closed,
                    Gate::Open | Gate::NoDefault => Gate::Open,
                };
            }
            if route != Gate::Open && !d.stopping() && !d.restarted.load(Ordering::SeqCst) && !hub.control.is_ended() {
                let until = (route == Gate::NoDefault).then_some(delay);
                match wait_for_network(hub, d, &mut failures, &failure, until) {
                    NetWait::Up => {
                        d.set(DestinationStatus::Retrying { attempt: failures.saturating_add(1), failure });
                        // A route that returns soon after a refused `publish` does not cut its wait.
                        wait(hub, d, refusal_left(&hub.timing, refused_at, Instant::now()));
                    }
                    NetWait::Restarted | NetWait::Stopped => {}
                    NetWait::GaveUp(f) => return fail(hub, d, f),
                    // The connection is not at fault, so it is not checked.
                    NetWait::EncoderEnded(f) => return d.fail(f),
                }
            }
        }
        if d.ends(hub) {
            return;
        }
        // The encoder it runs for itself ended: no try can send anything. The connection is not
        // at fault, so it is not checked.
        if let Some(f) = d.feed_ended(hub) {
            return d.fail(f);
        }
        // Started again while it waited: the try is shown as a first one.
        if d.restarted.load(Ordering::SeqCst) || !matches!(d.status(), DestinationStatus::Retrying { .. }) {
            d.set(DestinationStatus::Connecting);
        }
        // Cleared before the connect reads the setup, so a new setup given from here on closes
        // this connection.
        d.renew.store(false, Ordering::SeqCst);
        let connected = connect(hub, d);
        // Started again while it waited or connected: this try's answer is that of a first try, and
        // the tries after it are counted from none, and a later failure may be checked again.
        if d.restarted.swap(false, Ordering::SeqCst) {
            (after_loss, refusals, failures) = (false, 0, 0);
            d.uncheck();
        }
        let tried = match connected {
            Ok(p) => Ok(p),
            // Stopped while it connected: the next turn ends the thread.
            Err(_) if d.stopping() || hub.control.is_ended() => continue,
            Err(Missed::Stopped) => continue,
            // Given a new setup while it connected: the old one's error is not this one's, and the
            // next turn connects with the new one at once.
            Err(_) if d.renew.load(Ordering::SeqCst) => continue,
            Err(Missed::Rtmp(e)) if is_final(&e) && !(is_publish_refusal(&e) && after_loss && refusals < REFUSED_RETRIES) => {
                return fail(hub, d, failure(&e, false));
            }
            Err(Missed::Rtmp(e)) => {
                if is_publish_refusal(&e) {
                    refusals = refusals.saturating_add(1);
                    refused = true;
                }
                Err(failure(&e, false))
            }
            Err(Missed::Lookup(f)) => Err(f),
        };
        let mut publisher = match tried {
            Ok(p) => p,
            Err(f) => {
                failures = failures.saturating_add(1);
                if hub.timing.gives_up(failures) {
                    return fail(hub, d, gave_up(&hub.timing, f, failures));
                }
                last_error = Some(f);
                continue;
            }
        };
        // The server took `publish`.
        (after_loss, refusals) = (false, 0);
        // Stopped while it connected: the next turn ends the thread without sending. Given a new
        // setup while it connected: the next turn connects with it.
        if d.stopping() || hub.control.is_ended() || d.renew.load(Ordering::SeqCst) {
            let _ = publisher.close();
            continue;
        }
        let started = Instant::now();
        let feed = d.feed();
        let end = match feed.control.subscribe(feed.ring) {
            Ok(mut rx) => send_all(d, &feed.headers, &mut publisher, &mut rx),
            Err(e) => End::Failed(Failure::new(Cause::Other, format!("no room for the destination's packets: {e}"))),
        };
        drop(feed);
        // The connection ends whichever way sending ended; a lost one may not take the close.
        let _ = publisher.close();
        match end {
            // Given a new setup while it sent: however the old connection ended, the next turn
            // connects with the new one.
            _ if d.renew.load(Ordering::SeqCst) => {}
            // Unless it was started again meanwhile, the next turn ends the thread. When the
            // encoder it runs for itself ended while the stream goes on, it fails.
            End::Done => {
                if !d.stopping()
                    && let Some(f) = d.feed_ended(hub)
                {
                    return d.fail(f);
                }
            }
            End::Failed(e) => return fail(hub, d, e),
            End::Lost(e) => {
                // A stop and start that came while it sent or as the connection closed stays set,
                // so the next try is at once and is that of a first try.
                after_loss = true;
                d.lost.store(true, Ordering::Relaxed);
                d.reconnects.fetch_add(1, Ordering::Relaxed);
                failures = failures_after_loss(&hub.timing, failures, started.elapsed(), d.restarted.load(Ordering::SeqCst));
                if hub.timing.gives_up(failures) {
                    return fail(hub, d, gave_up(&hub.timing, e, failures));
                }
                last_error = Some(e);
            }
        }
    }
}

/// The failed tries in a row after a connection that lasted `lasted` was lost: none when the
/// destination was started again meanwhile, one when the connection lasted [`Timing::stable`], and
/// one more than `failures` when it was lost sooner.
fn failures_after_loss(timing: &Timing, failures: u32, lasted: Duration, restarted: bool) -> u32 {
    if restarted {
        0
    } else if lasted >= timing.stable {
        1
    } else {
        failures.saturating_add(1)
    }
}

/// Packets a receiver lost: dropped when its ring was full, skipped as stale, or discarded.
fn lost(rx: &Receiver) -> u64 {
    let s = rx.stats();
    s.dropped_packets.saturating_add(s.skipped_packets).saturating_add(s.discarded_packets)
}

/// Send the headers, then every packet `rx` delivers, until the destination is stopped, the
/// stream ends or the connection fails.
fn send_all(d: &Dest, h: &Headers, p: &mut Publisher, rx: &mut Receiver) -> End {
    let (bytes, dropped) = (d.bytes.load(Ordering::Relaxed), d.dropped.load(Ordering::Relaxed));
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
        // Stopped, or given a new setup: the next turn ends the thread or connects with it.
        if d.stopping() || d.renew.load(Ordering::SeqCst) {
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
        // A stop and start this loop did not see left the connection sending, so it is not a
        // restart for the next try; one that comes as the connection closes is kept.
        d.restarted.store(false, Ordering::SeqCst);
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

/// Closed captions for the stream's pictures: finished phrases from the job's feed, as CEA-608
/// roll-up captions in an SEI message on each H.264 access unit that has pairs due. Streams have
/// no B-frames, so packets come in the order their pictures are shown.
struct Captions {
    feed: Arc<Fifo<String>>,
    roll: RollUp,
    pairs: Vec<[u8; 2]>,
    nal: Vec<u8>,
}

impl Captions {
    /// Captions for a stream at `fps_num / fps_den`; the phrases already waiting are left out.
    fn new(feed: Arc<Fifo<String>>, fps_num: u32, fps_den: u32) -> Result<Captions, PluginError> {
        while feed.pop().is_some() {}
        let roll = RollUp::new(fps_num, fps_den, CaptionConfig::default()).map_err(|e| PluginError::Failed(format!("closed captions: {e}")))?;
        Ok(Captions { feed, roll, pairs: Vec::with_capacity(MAX_PAIRS_PER_FRAME), nal: Vec::new() })
    }

    /// Put this picture's caption pairs into `au`, a length-prefixed access unit. An access unit
    /// with no picture is sent as it was, and its pairs are lost.
    fn add(&mut self, au: &mut Vec<u8>) {
        while let Some(text) = self.feed.pop() {
            self.roll.push(&text);
        }
        self.roll.frame(&mut self.pairs);
        if self.pairs.is_empty() {
            return;
        }
        self.nal.clear();
        sei::sei_nal(Codec::H264, &self.pairs, &mut self.nal);
        let _ = sei::insert_sei(Codec::H264, au, &self.nal);
    }
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
    captions: Option<Captions>,
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
        if video && let Some(c) = &mut self.captions {
            c.add(&mut data);
        }
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
    /// The copy written to a file, when the stream was started with one.
    copy: Option<StreamCopy>,
    /// What hands the input to every encoder; `None` when the stream was started with no
    /// destination that runs an encoder of its own.
    split: Option<Split>,
    /// The encoders destinations run for themselves, in the order they started.
    own: Mutex<Vec<(Arc<EncoderFeed>, Native)>>,
    /// The job the stream was started with, for the encoders destinations start while it runs.
    job: OutputJob,
    stopped: bool,
}

/// The job of the encoder `own` names, beside the stream's `job`: the same frames, size, rate,
/// keyframe interval and sound, from `start` on. `None` gives `job` itself.
fn encoder_job(job: &OutputJob, own: Option<&OwnEncoder>, start: Tick) -> Result<OutputJob, PluginError> {
    let Some(own) = own else { return Ok(job.clone()) };
    let RateControl::Bitrate { keyframe_s, .. } = job.rate else {
        return Err(PluginError::Failed("streams need a bitrate".into()));
    };
    if !VIDEO_KBPS.contains(&own.kbps) {
        return Err(PluginError::Failed(format!(
            "a destination's video bitrate must be {} to {} kbps, not {}",
            VIDEO_KBPS.start(),
            VIDEO_KBPS.end(),
            own.kbps
        )));
    }
    let encoder = if job.video_encoder.id() == own.encoder {
        Arc::clone(&job.video_encoder)
    } else {
        let found = job.other_encoders.iter().find(|e| e.id() == own.encoder).cloned();
        found.ok_or_else(|| PluginError::Failed(format!("there is no video encoder `{}` for a destination to run", own.encoder)))?
    };
    Ok(OutputJob {
        video_encoder: encoder,
        video_encoder_settings: own.settings.clone(),
        rate: RateControl::Bitrate { kbps: own.kbps, keyframe_s },
        // Its frame counts are not the stream's.
        stats: Arc::new(EncoderStats::default()),
        start,
        other_encoders: Vec::new(),
        // The stream's encoder takes every phrase from the caption feed, so this one sends none.
        captions: None,
        ..job.clone()
    })
}

/// The first reason `job`'s video encoder cannot run a stream; else its video bitrate in kbps and
/// its keyframe interval in seconds.
fn check_stream_encoder(job: &OutputJob) -> Result<(u32, u32), PluginError> {
    let RateControl::Bitrate { kbps, keyframe_s } = job.rate else {
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
    Ok((kbps, keyframe_s))
}

/// The packets a stream's subscriber copies from as it starts: the tracks and its receiver.
type CopyFeed = (Vec<TrackSpec>, Result<Receiver, String>);

/// Whether `own` names the stream's encoder: its encoder, settings and bitrate are those of `job`.
fn is_main_encoder(job: &OutputJob, own: &OwnEncoder) -> bool {
    own.encoder == job.video_encoder.id()
        && own.settings == job.video_encoder_settings
        && matches!(job.rate, RateControl::Bitrate { kbps, .. } if kbps == own.kbps)
}

/// Open the encoders of `job` and the fan-out that shares what they make with the destinations,
/// as the encoder `own` names (`None` for the stream's encoder). With `copy`, a subscriber for
/// the copy is made before the encoders start, so it gets the first keyframe.
fn open_encoder(
    job: &OutputJob,
    own: Option<OwnEncoder>,
    refused: &Arc<AtomicU64>,
    copy: bool,
) -> Result<(Native, Arc<EncoderFeed>, Option<CopyFeed>), PluginError> {
    let (kbps, _) = check_stream_encoder(job)?;
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
    let mut headers = None;
    let mut copy_feed: Option<CopyFeed> = None;
    let sink_refused = Arc::clone(refused);
    let captions = match &job.captions {
        Some(feed) => Some(Captions::new(Arc::clone(feed), v.fps_num, v.fps_den)?),
        None => None,
    };
    let open = |tracks: Vec<TrackSpec>| -> Result<Box<dyn Sink>, PluginError> {
        headers = Some(Headers::new(job, kbps, &tracks)?);
        if copy {
            let rx = fanout.subscribe(ring).map_err(|e| format!("the copy of the stream could not be started: {e}"));
            copy_feed = Some((tracks, rx));
        }
        Ok(Box::new(Publish { fanout, pool, refused: sink_refused, video_gap: false, captions }))
    };
    let native = Native::start_with(job, open, PacketQueue::new(QUEUE_BYTES, QUEUE_SPAN_NS), DISK_WAIT)?;
    let Some(headers) = headers else {
        return Err(PluginError::Failed("the stream's encoders gave no configuration".into()));
    };
    let failure = native.failure_watch();
    Ok((native, Arc::new(EncoderFeed { own, control, headers, ring, failure }), copy_feed))
}

impl Streaming {
    /// Open the encoders for `job` and start sending to each enabled destination of `targets`.
    /// Starts the stream to `targets`, each trying again as `reconnect` says, with the automatic
    /// connection checks of `checks` in the process's [`diagnose::checks`]. With `copy`, the
    /// stream also writes what it sends to that Matroska file ([`copy`]).
    pub fn start(
        job: &OutputJob,
        targets: Vec<Target>,
        reconnect: ReconnectSettings,
        checks: AutoCheck,
        copy: Option<PathBuf>,
    ) -> Result<Streaming, PluginError> {
        reconnect.check().map_err(PluginError::Failed)?;
        let copy = copy.map(|path| {
            let open = copy::create(&path);
            (path, open)
        });
        Self::start_all(job, targets, Timing::from(reconnect), Seams::default(), Some((diagnose::checks(), checks)), copy)
    }

    #[cfg(test)]
    pub(crate) fn start_timed(job: &OutputJob, targets: Vec<Target>, timing: Timing) -> Result<Streaming, PluginError> {
        Self::start_with(job, targets, timing, Seams::default())
    }

    #[cfg(test)]
    pub(crate) fn start_with(job: &OutputJob, targets: Vec<Target>, timing: Timing, seams: Seams) -> Result<Streaming, PluginError> {
        Self::start_full(job, targets, timing, seams, None)
    }

    /// [`Streaming::start_timed`] with a copy shown as `path`, written to the file `open` makes.
    #[cfg(test)]
    pub(crate) fn start_copying(
        job: &OutputJob,
        targets: Vec<Target>,
        timing: Timing,
        path: PathBuf,
        open: copy::Open,
    ) -> Result<Streaming, PluginError> {
        Self::start_all(job, targets, timing, Seams::default(), None, Some((path, open)))
    }

    #[cfg(test)]
    pub(crate) fn start_full(
        job: &OutputJob,
        targets: Vec<Target>,
        timing: Timing,
        seams: Seams,
        auto: Option<(Arc<diagnose::Checks>, AutoCheck)>,
    ) -> Result<Streaming, PluginError> {
        Self::start_all(job, targets, timing, seams, auto, None)
    }

    fn start_all(
        job: &OutputJob,
        targets: Vec<Target>,
        timing: Timing,
        seams: Seams,
        auto: Option<(Arc<diagnose::Checks>, AutoCheck)>,
        copy: Option<(PathBuf, copy::Open)>,
    ) -> Result<Streaming, PluginError> {
        let (_, keyframe_s) = check_stream_encoder(job)?;
        check_targets(&targets)?;
        // The copy's writer holds each cluster in memory until the next keyframe, so the copy takes
        // the keyframe interval a recording takes; with a longer one only the copy is refused.
        let copy_refused = (copy.is_some() && keyframe_s > RECORDING_KEYFRAME_S).then(|| {
            format!(
                "the copy of the stream needs a keyframe at least every {RECORDING_KEYFRAME_S} seconds, not every {keyframe_s}; set the keyframe interval in Settings › Stream"
            )
        });
        let wants_copy = copy.is_some() && copy_refused.is_none();
        if wants_copy && targets.len() >= MAX_SUBSCRIBERS {
            return Err(PluginError::Failed(format!(
                "a stream that saves a copy can have at most {} destinations, not {}",
                MAX_SUBSCRIBERS - 1,
                targets.len()
            )));
        }
        let refused = Arc::new(AtomicU64::new(0));
        // The copy holds what the stream's encoder makes.
        let (native, main, copy_feed) = open_encoder(job, None, &refused, wants_copy)?;
        let control = main.control.clone();
        // Only a stream with a destination that runs an encoder other than the stream's splits its
        // input.
        let split = if targets.iter().any(|t| t.encoder.as_ref().is_some_and(|own| !is_main_encoder(job, own))) {
            Some(Split::start(&Input { video: native.video(), audio: native.audio() })?)
        } else {
            None
        };
        let copy = match (copy, copy_feed, copy_refused) {
            (Some((path, _)), _, Some(e)) => Some(StreamCopy::failed(path, e)),
            (Some((path, open)), Some((tracks, Ok(rx))), None) => Some(StreamCopy::start(path, open, tracks, rx)),
            (Some((path, _)), Some((_, Err(e))), None) => Some(StreamCopy::failed(path, e)),
            (Some((path, _)), None, None) => Some(StreamCopy::failed(path, "the copy of the stream could not be started".into())),
            (None, _, _) => None,
        };
        let counts = Arc::new(Counts::default());
        let service = |id: &str| auto.as_ref().and_then(|(_, a)| a.services.iter().find(|(i, _)| i == id).map(|(_, s)| *s));
        // Each destination is given its encoder as it starts.
        let dests: Vec<Arc<Dest>> = targets
            .into_iter()
            .map(|t| {
                let service = service(&t.id);
                Arc::new(Dest::new(t, Arc::clone(&counts), service, Arc::clone(&main)))
            })
            .collect();
        let hub = Arc::new(Hub {
            control,
            main,
            ending: AtomicBool::new(false),
            timing,
            seams,
            dests: Mutex::new(dests.clone()),
            left: Mutex::default(),
            counts,
            net: NetWatch::default(),
            auto,
        });
        let mut s = Streaming {
            native,
            hub,
            stats: Arc::clone(&job.stats),
            refused,
            copy,
            split,
            own: Mutex::new(Vec::new()),
            job: job.clone(),
            stopped: false,
        };
        for d in dests.iter().filter(|d| d.target.enabled) {
            // An encoder of its own that cannot start fails that destination alone.
            let feed = match s.feed_for(d.target.encoder.as_ref()) {
                Ok(feed) => feed,
                Err(e) => {
                    d.fail(Failure::new(Cause::Other, format!("its video encoder could not start: {e}")));
                    continue;
                }
            };
            if let Err(e) = spawn(&s.hub, d, Given { feed: Some(&feed), ..Given::default() }) {
                let _ = s.stop();
                return Err(e);
            }
        }
        Ok(s)
    }

    /// The encoder whose packets a destination with `own` sends: the stream's for `None`, or for
    /// one whose encoder, settings and bitrate are the stream's; else the running encoder another
    /// destination started with `own`; else a new one, started from the newest frame.
    fn feed_for(&self, own: Option<&OwnEncoder>) -> Result<Arc<EncoderFeed>, PluginError> {
        let Some(own) = own else { return Ok(Arc::clone(&self.hub.main)) };
        let job = &self.job;
        if is_main_encoder(job, own) {
            return Ok(Arc::clone(&self.hub.main));
        }
        // Held while one starts, so two starts of the same encoder start it once.
        let mut groups = lock(&self.own);
        // An encoder that ended is not used again. It is dropped, so it no longer counts toward
        // the encoders the stream can run; a destination still holding its packets keeps them.
        let ended: Vec<(Arc<EncoderFeed>, Native)> = {
            let (ended, running) = std::mem::take(&mut *groups).into_iter().partition(|(f, _)| f.control.is_ended());
            *groups = running;
            ended
        };
        drop(ended);
        if let Some((feed, _)) = groups.iter().find(|(f, _)| f.own.as_ref() == Some(own)) {
            return Ok(Arc::clone(feed));
        }
        let Some(split) = &self.split else {
            return Err(PluginError::Failed(
                "the stream started with no destination that runs a video encoder of its own; stop the stream and start it again for this destination to run one"
                    .into(),
            ));
        };
        if groups.len() >= MAX_SUBSCRIBERS {
            return Err(PluginError::Failed(format!("a stream can run at most {MAX_SUBSCRIBERS} encoders for its destinations")));
        }
        let start = split.newest().unwrap_or(job.start);
        let (native, feed, _) = open_encoder(&encoder_job(job, Some(own), start)?, Some(own.clone()), &self.refused, false)?;
        split.add(&Input { video: native.video(), audio: native.audio() })?;
        groups.push((Arc::clone(&feed), native));
        Ok(feed)
    }

    /// Whether one more destination can send: each destination that sends or tries to, and the
    /// copy, takes one of the [`MAX_SUBSCRIBERS`] subscriptions.
    fn room(&self) -> Result<(), PluginError> {
        let copy = usize::from(self.copy.is_some());
        let taken = lock(&self.hub.dests).iter().filter(|o| o.running()).count() + copy;
        if taken >= MAX_SUBSCRIBERS {
            let most = MAX_SUBSCRIBERS - copy;
            return Err(PluginError::Failed(format!("a stream can send to at most {most} destinations at once")));
        }
        Ok(())
    }

    /// Packets no destination got because the shared pool was full.
    pub fn refused(&self) -> u64 {
        self.refused.load(Ordering::Relaxed)
    }

    /// Wait for the destinations to send what their rings hold and close, then ask those still
    /// running to end.
    fn close_destinations(&self) {
        // The stream is ending, so no destination is added from here on.
        let dests = lock(&self.hub.dests).clone();
        for d in &dests {
            d.wake();
        }
        let deadline = Instant::now() + self.hub.timing.close;
        while dests.iter().any(|d| d.running()) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        for d in &dests {
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
        self.split.as_ref().map_or_else(|| self.native.video(), Split::video)
    }

    fn audio(&self) -> Arc<Fifo<PooledBlock>> {
        self.split.as_ref().map_or_else(|| self.native.audio(), Split::audio)
    }

    fn status(&self) -> OutputStatus {
        let dests = lock(&self.hub.dests);
        let left = lock(&self.hub.left).sum().bytes;
        let total = dests.iter().map(|d| d.bytes.load(Ordering::Relaxed)).fold(left, u64::saturating_add);
        self.stats.bytes.store(total, Ordering::Relaxed);
        let s = self.native.status();
        if !matches!(s, OutputStatus::Starting | OutputStatus::Running) {
            return s;
        }
        // The stream fails once a destination has failed and none is sending or trying to. While
        // one sends or tries, the check reads each status in place, without copying.
        let quiet = |st: &DestinationStatus| matches!(st, DestinationStatus::Failed(_) | DestinationStatus::Stopped);
        if dests.iter().any(|d| !quiet(&lock(&d.status))) {
            return s;
        }
        let states: Vec<DestinationStatus> = dests.iter().map(|d| d.status()).collect();
        let active = states.iter().any(|st| !matches!(st, DestinationStatus::Failed(_) | DestinationStatus::Stopped));
        let failed: Vec<(&str, &str)> = dests
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
        if let ([(_, e)], 1) = (failed.as_slice(), dests.len()) {
            return OutputStatus::Failed((*e).to_string());
        }
        let each: Vec<String> = failed.iter().map(|(id, e)| format!("{id}: {e}")).collect();
        let lead = if failed.len() == dests.len() { "every destination failed" } else { "no destination is sending" };
        OutputStatus::Failed(format!("{lead} ({})", each.join("; ")))
    }

    /// The addresses of the destinations that send or try to, as status shows them.
    fn target(&self) -> String {
        let quiet = |d: &Dest| matches!(*lock(&d.status), DestinationStatus::Stopped | DestinationStatus::Failed(_));
        let on: Vec<String> = lock(&self.hub.dests).iter().filter(|d| !quiet(d)).map(|d| d.shown()).collect();
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
        self.hub.ending.store(true, Ordering::SeqCst);
        // The input queued so far goes to every encoder.
        if let Some(split) = &mut self.split {
            split.stop();
        }
        // The encoders finish, and the last packets go to the rings; then the fan-outs end. An
        // encoder a destination runs for itself does not fail the stream.
        let r = self.native.stop();
        for (_, native) in lock(&self.own).iter_mut() {
            let _ = native.stop();
        }
        self.close_destinations();
        // The copy writes what its ring holds and finishes its file; its failure is in its state,
        // not in the stream's result.
        if let Some(c) = &mut self.copy {
            c.finish(FINISH_TIMEOUT);
        }
        // The check of a destination that failed for good outlives the stream; the others end.
        // A thread asked to stop starts no check, so one started while the stream stopped is
        // canceled here too.
        if let Some((checks, auto)) = &self.hub.auto {
            for d in lock(&self.hub.dests).iter().filter(|d| !matches!(d.status(), DestinationStatus::Failed(_))) {
                checks.cancel(&auto.profile, &d.target.id);
            }
        }
        r?;
        failed.map_or(Ok(()), |e| Err(PluginError::Failed(e)))
    }

    fn destinations(&self) -> Vec<DestinationState> {
        lock(&self.hub.dests).iter().map(|d| d.state()).collect()
    }

    fn stream_copy(&self) -> Option<StreamCopyState> {
        self.copy.as_ref().map(StreamCopy::state)
    }

    fn reconnects(&self) -> u64 {
        let dests = lock(&self.hub.dests);
        let left = lock(&self.hub.left).sum().reconnects;
        dests.iter().map(|d| u64::from(d.reconnects.load(Ordering::Relaxed))).fold(left, u64::saturating_add)
    }

    fn destination_counts(&self) -> DestinationCounts {
        let c = &self.hub.counts;
        DestinationCounts { live: c.live.load(Ordering::Relaxed), retrying: c.retrying.load(Ordering::Relaxed) }
    }

    fn recoveries(&self) -> u64 {
        let dests = lock(&self.hub.dests);
        let left = lock(&self.hub.left).sum().recovered;
        dests.iter().map(|d| u64::from(d.recovered.load(Ordering::SeqCst))).fold(left, u64::saturating_add)
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
        if d.running() && !d.stopping() {
            // It keeps what it sends with.
            return spawn(&self.hub, &d, Given { key, ..Given::default() }).map(|_| ());
        }
        if !d.running() {
            self.room()?;
        }
        // It sends with the encoder it was set up with, started again when that one ended.
        let own = lock(&d.own).clone();
        let feed = self.feed_for(own.as_ref())?;
        spawn(&self.hub, &d, Given { key, feed: Some(&feed), ..Given::default() }).map(|_| ())
    }

    fn start_destination_as(&self, setup: &DestinationSetup) -> Result<bool, PluginError> {
        let id = setup.id.as_str();
        if self.stopped || self.hub.control.is_ended() {
            return Err(PluginError::Failed("the stream is ending".into()));
        }
        if id.is_empty() {
            return Err(PluginError::Failed("the destination has no id".into()));
        }
        let found = self.hub.find(id).ok();
        if let Some(d) = found.as_ref().filter(|d| d.running() && !d.stopping()) {
            // It keeps the server and key it sends with, so `setup` is not used. A stop asked for
            // since is canceled, and it goes on with them too.
            return spawn(&self.hub, d, Given::default()).map(|_| false);
        }
        let target =
            Target { id: id.to_string(), server: setup.server.clone(), key: setup.key.clone(), enabled: true, encoder: setup.encoder.clone() };
        target.config().map_err(|e| PluginError::Failed(format!("destination `{id}`: {e}")))?;
        let service = studiocraft_model::services::service(&setup.service);
        // One asked to stop whose thread has not ended still holds its subscription, and starts
        // again with `setup`.
        if !found.as_ref().is_some_and(|d| d.running()) {
            self.room()?;
        }
        // The encoder is found or started before the destination is added, so one that cannot
        // start adds nothing.
        let feed = self.feed_for(setup.encoder.as_ref())?;
        let d = match found {
            Some(d) => d,
            None => self.hub.find_or_add(id, service, &feed)?,
        };
        *lock(&d.own) = setup.encoder.clone();
        let given = Given { key: Some(&target.key), server: Some((&target.server, service)), feed: Some(&feed) };
        spawn(&self.hub, &d, given)
    }

    fn stop_destination(&self, id: &str) -> Result<(), PluginError> {
        self.hub.find(id)?.ask_stop();
        if let Some((checks, auto)) = &self.hub.auto {
            checks.cancel(&auto.profile, id);
        }
        Ok(())
    }

    fn destination_sending(&self, id: &str) -> bool {
        self.hub.find(id).is_ok_and(|d| d.running() && !d.stopping())
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
