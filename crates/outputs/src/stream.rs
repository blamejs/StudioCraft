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
//!   lookups of one name outstanding and the process at most [`MAX_ALL_LOOKUPS`] of any name, and
//!   lookups of a name the destination no longer uses do not hold back its new name. An answer that comes
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
//! - A destination that waits to try again, or waits for the network, reports when its next try
//!   starts ([`DestinationState::next_try`]), and Try Now ([`OutputInstance::retry_destination`])
//!   starts that try at once, without a route too. Unlike a stop and start, the try counts toward
//!   the failed tries in a row.
//! - A stream started with a copy also writes the packets the destinations get to a Matroska
//!   file ([`copy`]). The copy takes one of the [`MAX_SUBSCRIBERS`] subscriptions, and its
//!   failure stops neither the destinations nor the encoders.
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
use studiocraft_media::audio::{PooledBlock, SAMPLE_RATE};
use studiocraft_media::queue::Fifo;
use studiocraft_media::stats::EncoderStats;
use studiocraft_media::video::PooledFrame;
use studiocraft_model::ReconnectSettings;
use studiocraft_model::services::Service;
use studiocraft_platform::net::Route;
use studiocraft_plugin::{
    AddressOutcome, Cause, DestinationCounts, DestinationSetup, DestinationState, DestinationStatus, EncoderKind, Failure, OutputInstance, OutputJob,
    OutputStatus, PluginError, RateControl, StreamCopyState,
};

use crate::native::{DISK_WAIT, Item, Native, PacketQueue, QUEUE_BYTES, QUEUE_SPAN_NS, Sink, Track};
use crate::writer::FINISH_TIMEOUT;
use copy::StreamCopy;

pub mod copy;
pub mod diagnose;
pub mod ingests;

/// Seconds of the stream each destination's ring holds.
pub const RING_SECONDS: u32 = 4;
/// The most destinations a stream lists, and the most of those that gave their place to added
/// ones whose counts it keeps by id.
pub const MAX_DESTINATIONS: usize = MAX_SUBSCRIBERS;
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
/// The most lookups of one name by one destination that may be outstanding: the one of the
/// current try, and one an earlier try gave up on that the system has not answered yet.
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

/// When a destination's next try starts while it waits to try again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NextTry {
    /// It does not wait to try again.
    None,
    /// At this time of the system's clock.
    At(Instant),
    /// At this time of the stream's clock ([`Seams::clock`]), while it waits for the network.
    Clocked(Instant),
    /// When a route to the server returns, while it waits for the network.
    Route,
}

/// The bits of [`Dest::next`] that hold the kind of the next try.
const NEXT_KIND: u64 = 0b11;
const NEXT_NONE: u64 = 0;
const NEXT_AT: u64 = 1;
const NEXT_CLOCKED: u64 = 2;
const NEXT_ROUTE: u64 = 3;
/// The bit of [`Dest::next`] that holds Try Now's request.
const NEXT_TRY_NOW: u64 = 0b100;
/// Where the deadline starts in [`Dest::next`].
const NEXT_SHIFT: u32 = 3;

/// A destination and what its thread reports.
struct Dest {
    target: Target,
    /// The stream's counts, shared by its destinations.
    counts: Arc<Counts>,
    /// The server, stream key and service it sends with, read without a lock and replaced
    /// together.
    setup: ArcSwap<Setup>,
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
    /// The name of each lookup counted in `lookups`, one entry per lookup.
    looking_up: Mutex<Vec<String>>,
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
    /// When its next try starts while it waits to try again, with Try Now's request, in one word
    /// ([`Dest::pack`]). The request is set only while there is a next try, and cleared as the try
    /// starts.
    next: AtomicU64,
    /// The time the deadlines in `next` count from.
    epoch: Instant,
    /// Asks the thread to close the connection and end.
    stop: AtomicBool,
    /// The thread runs and has not decided to end. It is changed with `thread` locked, so a start
    /// either reaches a thread that goes on or finds none.
    alive: AtomicBool,
    thread: Mutex<Option<JoinHandle<()>>>,
    /// Times [`Dest::sending`] found `thread` locked and waited for it.
    #[cfg(test)]
    waits: AtomicU32,
    /// Where [`send_all`] tells a test it reached a [`Pause`], then waits until the test lets it
    /// go on; `None`, or a test that hung up, lets it go on at once.
    #[cfg(test)]
    pause: Mutex<Option<(std::sync::mpsc::Sender<Pause>, std::sync::mpsc::Receiver<()>)>>,
}

/// A point in [`send_all`] where a test can hold the sending thread.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pause {
    /// Past the check for a stop or a new setup, before the wait for a packet.
    Waiting,
    /// A packet was sent or the connection was read without an error.
    Sent,
}

impl Dest {
    fn new(target: Target, counts: Arc<Counts>, service: Option<&'static Service>) -> Dest {
        Dest {
            setup: ArcSwap::from_pointee(Setup { server: target.server.clone(), key: target.key.clone(), service }),
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
            looking_up: Mutex::new(Vec::new()),
            addrs: Mutex::new(Vec::new()),
            fixed: AtomicBool::new(false),
            net_up: AtomicBool::new(false),
            lost: AtomicBool::new(false),
            recovered: AtomicU32::new(0),
            checked: Arc::new(AtomicBool::new(false)),
            wanted: Mutex::new(Arc::new(Mutex::new(false))),
            next: AtomicU64::new(0),
            epoch: Instant::now(),
            stop: AtomicBool::new(false),
            alive: AtomicBool::new(false),
            thread: Mutex::new(None),
            #[cfg(test)]
            waits: AtomicU32::new(0),
            #[cfg(test)]
            pause: Mutex::new(None),
        }
    }

    /// Tells the test holding [`Dest::pause`] that the thread reached `at`, and waits until it
    /// lets the thread go on.
    #[cfg(test)]
    fn pause_at(&self, at: Pause) {
        let held = lock(&self.pause);
        if let Some((tell, go)) = held.as_ref()
            && tell.send(at).is_ok()
        {
            let _ = go.recv();
        }
    }

    /// The target with its server now and `key` as its stream key.
    fn with_key(&self, key: &str) -> Target {
        self.with(&self.setup.load().server, key)
    }

    /// The target with `server` and `key`.
    fn with(&self, server: &str, key: &str) -> Target {
        Target { id: self.target.id.clone(), server: server.to_string(), key: key.to_string(), enabled: self.target.enabled }
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
        // Only a destination that waits to try again has a next try.
        if !matches!(*status, DestinationStatus::Retrying { .. } | DestinationStatus::WaitingForNetwork { .. }) {
            self.wait_until(NextTry::None);
        }
        drop(status);
        self.touch();
    }

    /// `next` as one word: the kind in bits 0 and 1, Try Now's request (bit 2) left clear, and the
    /// deadline in the bits above, in nanoseconds from `epoch`, signed. A deadline more than about
    /// 36 years from `epoch` is kept at that limit.
    fn pack(&self, next: NextTry) -> u64 {
        let (kind, at) = match next {
            NextTry::None => (NEXT_NONE, None),
            NextTry::At(at) => (NEXT_AT, Some(at)),
            NextTry::Clocked(at) => (NEXT_CLOCKED, Some(at)),
            NextTry::Route => (NEXT_ROUTE, None),
        };
        let limit = i64::MAX >> NEXT_SHIFT;
        let nanos = at.map_or(0, |at| match at.checked_duration_since(self.epoch) {
            Some(after) => i64::try_from(after.as_nanos()).unwrap_or(limit).min(limit),
            None => -i64::try_from(self.epoch.duration_since(at).as_nanos()).unwrap_or(limit).min(limit),
        });
        ((nanos << NEXT_SHIFT) as u64) | kind
    }

    /// The next try held in `word`.
    fn unpack(&self, word: u64) -> NextTry {
        let nanos = (word as i64) >> NEXT_SHIFT;
        let at = if nanos >= 0 {
            self.epoch.checked_add(Duration::from_nanos(nanos.unsigned_abs()))
        } else {
            self.epoch.checked_sub(Duration::from_nanos(nanos.unsigned_abs()))
        }
        .unwrap_or(self.epoch);
        match word & NEXT_KIND {
            NEXT_AT => NextTry::At(at),
            NEXT_CLOCKED => NextTry::Clocked(at),
            NEXT_ROUTE => NextTry::Route,
            _ => NextTry::None,
        }
    }

    /// When the next try starts.
    fn next_try(&self) -> NextTry {
        self.unpack(self.next.load(Ordering::SeqCst))
    }

    /// Sets when the next try starts, before the status that shows the wait is set. A Try Now
    /// request stays until the try starts.
    fn wait_until(&self, next: NextTry) {
        let word = self.pack(next);
        // The closure always gives a value, so the update cannot fail.
        let _ = self.next.try_update(Ordering::SeqCst, Ordering::SeqCst, |old| Some(word | (old & NEXT_TRY_NOW)));
    }

    /// Ends the wait before the next try, so the try starts at once; whether it was waiting.
    fn try_now(&self) -> bool {
        let asked =
            self.next.try_update(Ordering::SeqCst, Ordering::SeqCst, |old| (old & NEXT_KIND != NEXT_NONE).then_some(old | NEXT_TRY_NOW)).is_ok();
        if asked {
            self.wake();
        }
        asked
    }

    /// Whether Try Now asked the thread to end its wait.
    fn trying_now(&self) -> bool {
        self.next.load(Ordering::SeqCst) & NEXT_TRY_NOW != 0
    }

    /// The wait is over and the try starts: there is no next try until it fails.
    fn begin_try(&self) {
        self.next.store(self.pack(NextTry::None), Ordering::SeqCst);
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

    /// Whether its thread runs and was not asked to stop, read with `thread` locked. The thread
    /// clears `alive` with `thread` locked when it decides to end, so a thread that has not decided
    /// by then goes on sending, and the start that follows finds one that has decided ended.
    fn sending(&self) -> bool {
        let _slot = self.slot();
        self.running() && !self.stopping()
    }

    /// `thread`, locked.
    #[cfg(not(test))]
    fn slot(&self) -> MutexGuard<'_, Option<JoinHandle<()>>> {
        lock(&self.thread)
    }

    /// `thread`, locked, counted in `waits` when it was locked already.
    #[cfg(test)]
    fn slot(&self) -> MutexGuard<'_, Option<JoinHandle<()>>> {
        match self.thread.try_lock() {
            Ok(slot) => slot,
            Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {
                self.waits.fetch_add(1, Ordering::SeqCst);
                lock(&self.thread)
            }
        }
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

    fn state(&self, hub: &Hub) -> DestinationState {
        let status = self.status();
        // Read after the status, which is set after the next try, so a waiting status has its time.
        let next_try = match self.next_try() {
            NextTry::At(at) => Some(at.saturating_duration_since(Instant::now())),
            NextTry::Clocked(at) => Some(at.saturating_duration_since(hub.seams.clock.now())),
            NextTry::None | NextTry::Route => None,
        };
        let waiting = matches!(status, DestinationStatus::Retrying { .. } | DestinationStatus::WaitingForNetwork { .. });
        DestinationState {
            id: self.target.id.clone(),
            target: self.shown(),
            status,
            bytes: self.bytes.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            reconnects: self.reconnects.load(Ordering::Relaxed),
            next_try: next_try.filter(|_| waiting),
            max_tries: hub.timing.max_tries,
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
    seams: Seams,
    /// The destinations it was started with, in order, then those added while it runs. One added
    /// to a full list takes the place of the one it replaces, so it can come before destinations
    /// the stream was started with. At most [`MAX_SUBSCRIBERS`].
    dests: Mutex<Vec<Arc<Dest>>>,
    /// What the destinations that gave their place to added ones had sent and made again.
    left: Mutex<Left>,
    counts: Arc<Counts>,
    net: NetWatch,
    /// Where the automatic connection checks run, and what they need; `None` turns them off.
    auto: Option<(Arc<diagnose::Checks>, AutoCheck)>,
}

/// The counts of destinations no longer in a stream's list, so the stream's sums never go down.
/// One added again under its id goes on from its counts when it is among the last
/// [`MAX_SUBSCRIBERS`] that left; one that left before them starts from zero, and the stream's
/// sums still hold its counts. The ids come from control requests, so the record stays bounded.
#[derive(Default)]
struct Left {
    /// The last [`MAX_SUBSCRIBERS`] that left, oldest first, each by id.
    each: VecDeque<(String, Totals)>,
    /// Those that left before them, summed without their ids.
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
    fn find_or_add(&self, id: &str, service: Option<&'static Service>) -> Result<Arc<Dest>, PluginError> {
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
        let target = Target { id: id.to_string(), server: String::new(), key: String::new(), enabled: true };
        let d = Arc::new(Dest::new(target, Arc::clone(&self.counts), service));
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
        // Its thread has ended and starts no more checks. As in `stop`, the check of one that
        // failed for good goes on, and that of a stopped one ends.
        if let Some((checks, auto)) = &self.auto
            && !matches!(old.status(), DestinationStatus::Failed(_))
        {
            checks.cancel(&auto.profile, &old.target.id);
        }
        Ok(d)
    }
}

/// Start the thread of `d`, unless it is running, with `key` as its stream key when given, and
/// with `server` as its server and service when given. A running thread keeps its server and key,
/// unless it was asked to stop and has not ended yet. Says whether `d` took `key` and `server`:
/// not when its thread was sending.
fn spawn(hub: &Arc<Hub>, d: &Arc<Dest>, key: Option<&str>, server: Option<(&str, Option<&'static Service>)>) -> Result<bool, PluginError> {
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
        if set_up(d, key, server) {
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
    set_up(d, key, server);
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

/// Gives `d` `key` as its stream key when given, and `server` as its server and service when
/// given. Says whether its server or key changed; the service is used only by its connection
/// checks, so a connection does not close for it. The key, server and service are published as
/// one new setup, so no reader sees some of them changed and not the others. Only `spawn` calls
/// it, with `thread` locked, so no other change comes between the read and the publish.
fn set_up(d: &Dest, key: Option<&str>, server: Option<(&str, Option<&'static Service>)>) -> bool {
    let mut now = Setup::clone(&d.setup.load());
    let mut changed = false;
    let mut moved = false;
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

/// Starts the automatic connection check of `d` after `f`, a failure of a try with `setup`, unless
/// one ran since it last went live or was started, `d` was started again since that try, or `f` is
/// an address or key that is not valid.
fn check_after(hub: &Hub, d: &Dest, f: &Failure, setup: &Setup) {
    let Some((checks, auto)) = &hub.auto else { return };
    // `checked` and this check's own flag change under the same lock as in `uncheck`, so a
    // restart or a recovery meanwhile either happens before both are set or clears this check's
    // flag. A check from before that still waits for a place is dropped. `spawn` sets `restarted`
    // before it calls `uncheck`, so a restart this check does not see clears its flag.
    let wanted = {
        let mut held = lock(&d.wanted);
        if matches!(f.cause, Cause::BadAddress | Cause::BadKey) || d.restarted.load(Ordering::SeqCst) || d.checked.swap(true, Ordering::SeqCst) {
            return;
        }
        let wanted = Arc::new(Mutex::new(true));
        let before: Arc<Mutex<bool>> = std::mem::replace(&mut *held, Arc::clone(&wanted));
        *lock(&before) = false;
        wanted
    };
    let id = &d.target.id;
    // The server and service of the try that failed, not of a setup given since.
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

/// The thread gives up with `f`, a failure of a try with `setup`, and the connection is checked;
/// says whether it gave up. It does not when it was started again since that try: `spawn` sets
/// `restarted` with `thread` locked, as this reads it, so the start either comes first and the
/// thread goes on with the setup it was given, or comes after and starts a new thread.
fn fail(hub: &Hub, d: &Dest, f: Failure, setup: &Setup) -> bool {
    let _slot = lock(&d.thread);
    if d.restarted.load(Ordering::SeqCst) {
        return false;
    }
    check_after(hub, d, &f, setup);
    d.set(DestinationStatus::Failed(f));
    d.alive.store(false, Ordering::SeqCst);
    true
}

/// Waits until `deadline`, or less when the destination is stopped or started again, Try Now ends
/// the wait, or the stream ends.
fn wait(hub: &Hub, d: &Dest, deadline: Instant) {
    loop {
        let now = Instant::now();
        if d.stopping() || d.restarted.load(Ordering::SeqCst) || d.trying_now() || hub.control.is_ended() || now >= deadline {
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
    d.wait_until(until.and_then(|u| start.checked_add(u)).map_or(NextTry::Route, NextTry::Clocked));
    d.set(DestinationStatus::WaitingForNetwork { attempt: failures.saturating_add(1), failure: last.clone() });
    let ended = loop {
        if d.stopping() || hub.control.is_ended() {
            break NetWait::Stopped;
        }
        // Started again while it waits: it tries at once, as after the wait before a try.
        if d.restarted.load(Ordering::SeqCst) {
            break NetWait::Restarted;
        }
        // Try Now: it tries at once, without a route, as the next of its tries.
        if d.trying_now() {
            break NetWait::Up;
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
    /// The destination was stopped, or the stream ended, while the lookup ran; or it was given a
    /// new setup, which the next try uses.
    Stopped,
}

/// Connects with `setup`, the setup the try read.
fn connect(hub: &Hub, d: &Arc<Dest>, setup: &Setup) -> Result<Publisher, Missed> {
    let mut c = connect_config(d, setup).map_err(Missed::Rtmp)?;
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
    // A stop, or a new setup, that came as the lookup answered does not wait for a TCP connect.
    if d.stopping() || hub.control.is_ended() || d.renew.load(Ordering::SeqCst) {
        return Err(Missed::Stopped);
    }
    Publisher::connect_to(&c, &addrs).map_err(Missed::Rtmp)
}

/// The connection settings of `d` with the server and key of `setup`, whatever setup `d` holds now.
fn connect_config(d: &Dest, setup: &Setup) -> Result<Config, craft_rtmp::Error> {
    d.with(&setup.server, &setup.key).config()
}

/// Looks `host` up on a thread of its own and waits for the answer until the lookup time runs
/// out, the destination is stopped or given a new setup, or the stream ends. The thread ends when
/// the resolver returns, whether or not the try still waits; an answer nobody waits for is
/// dropped. An IP address is used as it is, with no lookup, so the lookup limits do not apply to
/// it.
fn lookup(hub: &Hub, d: &Arc<Dest>, host: &str, port: u16) -> Result<Vec<std::net::SocketAddr>, Missed> {
    use std::sync::mpsc::{TryRecvError, sync_channel};
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return Ok(vec![std::net::SocketAddr::new(ip, port)]);
    }
    // Only this destination's thread adds lookups, so the count cannot pass the limit between the
    // check and the add. Lookups of other names, such as a server given before a stop and start,
    // count only toward the process's limit.
    let pending = lock(&d.looking_up).iter().filter(|n| n.as_str() == host).count();
    if pending >= MAX_LOOKUPS as usize {
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
    lock(&d.looking_up).push(host.to_string());
    d.lookups.fetch_add(1, Ordering::SeqCst);
    let spawned = std::thread::Builder::new().name("stream lookup".into()).spawn(move || {
        let answer = resolve(&name, port);
        // The answer carries when it came, so a try that wakes late still uses an answer in time.
        // Fails when the try stopped waiting, which drops the answer.
        let _ = tx.send((answer, clock.now()));
        ALL_LOOKUPS.fetch_sub(1, Ordering::SeqCst);
        unlist(&owner.looking_up, &name);
        owner.lookups.fetch_sub(1, Ordering::SeqCst);
        owner.touch();
        waiter.unpark();
    });
    if let Err(e) = spawned {
        ALL_LOOKUPS.fetch_sub(1, Ordering::SeqCst);
        unlist(&d.looking_up, host);
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
        // `spawn` wakes the thread after it gives a new setup, so the try ends at once.
        if d.stopping() || hub.control.is_ended() || d.renew.load(Ordering::SeqCst) {
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

/// Removes one entry of `name` from `names`.
fn unlist(names: &Mutex<Vec<String>>, name: &str) {
    let mut names = lock(names);
    if let Some(i) = names.iter().position(|n| n == name) {
        names.swap_remove(i);
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
    // The last try's failure, with the setup that try used.
    let mut last_error: Option<(Failure, Arc<Setup>)> = None;
    // Whether the tries follow a connection the server took and that was then lost, the refused
    // `publish` answers since, and whether the last try was one.
    let mut after_loss = false;
    let mut refusals = 0u32;
    let mut refused = false;
    let mut random = seed(&d.target.id);
    loop {
        if let Some((failure, setup)) = last_error.take() {
            if failures >= CHECK_AFTER_FAILURES {
                check_after(hub, d, &failure, &setup);
            }
            let delay = hub.timing.delay(failures.max(1));
            let delay = if hub.timing.jitter { jittered(delay, next_random(&mut random)) } else { delay };
            let refused_at = std::mem::take(&mut refused).then(Instant::now);
            let delay = if refused_at.is_some() { delay.max(hub.timing.refused) } else { delay };
            // With no route, the wait for the network takes the place of the wait before the try.
            let mut route = gate(hub, d, &failure);
            if route == Gate::Open {
                // The next try is set before the status that shows the wait.
                let at = Instant::now() + delay;
                d.wait_until(NextTry::At(at));
                d.set(DestinationStatus::Retrying { attempt: failures.saturating_add(1), failure: failure.clone() });
                wait(hub, d, at);
                // Checked again right before the try: the route may have gone during the wait.
                route = match gate(hub, d, &failure) {
                    Gate::Closed => Gate::Closed,
                    Gate::Open | Gate::NoDefault => Gate::Open,
                };
            }
            // Try Now tries at once, also without a route.
            if route != Gate::Open && !d.stopping() && !d.restarted.load(Ordering::SeqCst) && !d.trying_now() && !hub.control.is_ended() {
                let until = (route == Gate::NoDefault).then_some(delay);
                match wait_for_network(hub, d, &mut failures, &failure, until) {
                    NetWait::Up => {
                        let at = Instant::now() + refusal_left(&hub.timing, refused_at, Instant::now());
                        d.wait_until(NextTry::At(at));
                        d.set(DestinationStatus::Retrying { attempt: failures.saturating_add(1), failure });
                        // A route that returns soon after a refused `publish` does not cut its wait;
                        // Try Now does.
                        wait(hub, d, at);
                    }
                    NetWait::Restarted | NetWait::Stopped => {}
                    // Started again as it gave up: the next try uses the setup it was given.
                    NetWait::GaveUp(f) => {
                        if fail(hub, d, f, &setup) {
                            return;
                        }
                    }
                }
            }
        }
        if d.ends(hub) {
            return;
        }
        d.begin_try();
        // Started again while it waited: the try is shown as a first one.
        if d.restarted.load(Ordering::SeqCst) || !matches!(d.status(), DestinationStatus::Retrying { .. }) {
            d.set(DestinationStatus::Connecting);
        }
        // Cleared before the connect reads the setup, so a new setup given from here on closes
        // this connection.
        d.renew.store(false, Ordering::SeqCst);
        let setup = d.setup();
        let connected = connect(hub, d, &setup);
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
            // Unless it was started again since, which the next turn tries at once.
            Err(Missed::Rtmp(e)) if is_final(&e) && !(is_publish_refusal(&e) && after_loss && refusals < REFUSED_RETRIES) => {
                if fail(hub, d, failure(&e, false), &setup) {
                    return;
                }
                continue;
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
                if !hub.timing.gives_up(failures) {
                    last_error = Some((f, setup));
                } else if fail(hub, d, gave_up(&hub.timing, f, failures), &setup) {
                    return;
                }
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
        let end = match hub.control.subscribe(hub.ring) {
            Ok(mut rx) => send_all(hub, d, &mut publisher, &mut rx),
            Err(e) => End::Failed(Failure::new(Cause::Other, format!("no room for the destination's packets: {e}"))),
        };
        // The connection ends whichever way sending ended; a lost one may not take the close.
        let _ = publisher.close();
        match end {
            // Given a new setup while it sent: however the old connection ended, the next turn
            // connects with the new one.
            _ if d.renew.load(Ordering::SeqCst) => {}
            // Unless it was started again meanwhile, the next turn ends the thread.
            End::Done => {}
            End::Failed(e) => {
                if fail(hub, d, e, &setup) {
                    return;
                }
            }
            End::Lost(e) => {
                // A stop and start that came while it sent or as the connection closed stays set,
                // so the next try is at once and is that of a first try.
                after_loss = true;
                d.lost.store(true, Ordering::Relaxed);
                d.reconnects.fetch_add(1, Ordering::Relaxed);
                failures = failures_after_loss(&hub.timing, failures, started.elapsed(), d.restarted.load(Ordering::SeqCst));
                if !hub.timing.gives_up(failures) {
                    last_error = Some((e, setup));
                } else if fail(hub, d, gave_up(&hub.timing, e, failures), &setup) {
                    return;
                }
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
        // Stopped, or given a new setup: the next turn ends the thread or connects with it.
        if d.stopping() || d.renew.load(Ordering::SeqCst) {
            return End::Done;
        }
        #[cfg(test)]
        d.pause_at(Pause::Waiting);
        let sent = match rx.recv_timeout(RECV_WAIT) {
            // Stopped or given a new setup during the wait: the packet is not sent with the old one.
            Ok(_) if d.stopping() || d.renew.load(Ordering::SeqCst) => return End::Done,
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
        #[cfg(test)]
        d.pause_at(Pause::Sent);
        // A stop and start this loop did not see left the connection sending, so it is not a
        // restart for the next try; one that comes as the connection closes is kept. One with a
        // new setup closes this connection, so it stays a restart: `spawn` sets `renew` before
        // `restarted`, so one that `restarted` was cleared after shows here as `renew`.
        d.restarted.store(false, Ordering::SeqCst);
        if d.renew.load(Ordering::SeqCst) {
            d.restarted.store(true, Ordering::SeqCst);
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
    stopped: bool,
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
        check_targets(&targets)?;
        // The copy's writer holds each cluster in memory until the next keyframe, so the copy takes
        // the keyframe interval a recording takes; with a longer one only the copy is refused.
        let copy_refused = (copy.is_some() && keyframe_s > RECORDING_KEYFRAME_S).then(|| {
            format!(
                "the copy of the stream needs a keyframe at least every {RECORDING_KEYFRAME_S} seconds, not every {keyframe_s}; set the keyframe interval in Settings › Stream"
            )
        });
        let wants_copy = copy.is_some() && copy_refused.is_none();
        // Only the destinations that are on take a subscription.
        let on = targets.iter().filter(|t| t.enabled).count();
        if wants_copy && on >= MAX_SUBSCRIBERS {
            return Err(PluginError::Failed(format!(
                "a stream that saves a copy can have at most {} destinations turned on, not {on}",
                MAX_SUBSCRIBERS - 1
            )));
        }
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
        // The copy subscribes before the encoders start, so it gets the first keyframe.
        let mut copy_feed: Option<(Vec<TrackSpec>, Result<Receiver, String>)> = None;
        let sink_refused = Arc::clone(&refused);
        let captions = match &job.captions {
            Some(feed) => Some(Captions::new(Arc::clone(feed), v.fps_num, v.fps_den)?),
            None => None,
        };
        let open = |tracks: Vec<TrackSpec>| -> Result<Box<dyn Sink>, PluginError> {
            headers = Some(Headers::new(job, kbps, &tracks)?);
            if wants_copy {
                let rx = fanout.subscribe(ring).map_err(|e| format!("the copy of the stream could not be started: {e}"));
                copy_feed = Some((tracks, rx));
            }
            Ok(Box::new(Publish { fanout, pool, refused: sink_refused, video_gap: false, captions }))
        };
        let native = Native::start_with(job, open, PacketQueue::new(QUEUE_BYTES, QUEUE_SPAN_NS), DISK_WAIT)?;
        let Some(headers) = headers else {
            return Err(PluginError::Failed("the stream's encoders gave no configuration".into()));
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
        let dests: Vec<Arc<Dest>> = targets
            .into_iter()
            .map(|t| {
                let service = service(&t.id);
                Arc::new(Dest::new(t, Arc::clone(&counts), service))
            })
            .collect();
        let hub = Arc::new(Hub {
            control,
            headers,
            ring,
            timing,
            seams,
            dests: Mutex::new(dests.clone()),
            left: Mutex::default(),
            counts,
            net: NetWatch::default(),
            auto,
        });
        let mut s = Streaming { native, hub, stats: Arc::clone(&job.stats), refused, copy, stopped: false };
        for d in dests.iter().filter(|d| d.target.enabled) {
            if let Err(e) = spawn(&s.hub, d, None, None) {
                let _ = s.stop();
                return Err(e);
            }
        }
        Ok(s)
    }

    /// Whether one more destination can send: each destination that sends or tries to, and a copy
    /// that holds its subscription, takes one of the [`MAX_SUBSCRIBERS`] subscriptions.
    fn room(&self) -> Result<(), PluginError> {
        let copy = usize::from(self.copy.as_ref().is_some_and(StreamCopy::subscribed));
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
        self.native.video()
    }

    fn audio(&self) -> Arc<Fifo<PooledBlock>> {
        self.native.audio()
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
        // The encoders finish, and the last packets go to the rings; then the fan-out ends.
        let r = self.native.stop();
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
        lock(&self.hub.dests).iter().map(|d| d.state(&self.hub)).collect()
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
        if !d.running() {
            self.room()?;
        }
        spawn(&self.hub, &d, key, None).map(|_| ())
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
        if found.as_ref().is_some_and(|d| d.sending()) {
            // It keeps the server and key it sends with, so `setup` is not used.
            return Ok(false);
        }
        let target = Target { id: id.to_string(), server: setup.server.clone(), key: setup.key.clone(), enabled: true };
        target.config().map_err(|e| PluginError::Failed(format!("destination `{id}`: {e}")))?;
        let service = studiocraft_model::services::service(&setup.service);
        // One asked to stop whose thread has not ended still holds its subscription, and starts
        // again with `setup`.
        if !found.as_ref().is_some_and(|d| d.running()) {
            self.room()?;
        }
        let d = match found {
            Some(d) => d,
            None => self.hub.find_or_add(id, service)?,
        };
        spawn(&self.hub, &d, Some(&target.key), Some((&target.server, service)))
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

    fn retry_destination(&self, id: &str) -> Result<bool, PluginError> {
        let d = self.hub.find(id)?;
        if self.stopped || self.hub.control.is_ended() {
            return Err(PluginError::Failed("the stream is ending".into()));
        }
        Ok(d.try_now())
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
