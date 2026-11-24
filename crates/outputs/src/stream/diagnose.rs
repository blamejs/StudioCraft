//! The connection check of a stream destination: what this computer's network can and cannot
//! reach, and one verdict drawn from it.
//!
//! A check runs when a destination's third try in a row fails or it fails for good (once until it
//! goes live again or is started again), and when asked for ([`Checks::start`], the window's
//! Check Connection and `stream.diagnose`). It never runs in the background on its own.
//!
//! What it does, all at once, each step on a thread of its own:
//!
//! 1. Asks the system, which sends nothing, whether it has a route to the server's addresses and
//!    to any address ([`studiocraft_platform::net::route_to`]), and reads which proxy variables
//!    are set in the environment (their names only).
//! 2. Looks the server's name up again, waiting at most [`super::LOOKUP_WAIT`].
//! 3. Opens a TCP connection to each of up to [`craft_rtmp::MAX_ADDRESSES`] of the server's
//!    addresses on the destination's port and, for `rtmps://`, completes the TLS handshake, which
//!    sends the host name in clear; then closes it ([`craft_rtmp::reach`]). No RTMP bytes, no
//!    stream key and no part of the address after the host are sent.
//! 4. When the port is not 443: the same on port 443 of the same addresses.
//! 5. For a service that publishes them, and only when the check is asked for or the profile's
//!    `check_service` is on: fetches the service's list of ingest servers and connects to two of
//!    the others on port 443 with TLS, and fetches the service's status summary. The fetches
//!    follow no redirect and use no proxy.
//!
//! Each step that sends anything has [`PROBE_TIMEOUT`], and the check reports after at most
//! [`CHECK_TIME`] with what finished; the steps still running are reported as unfinished. At most
//! [`MAX_CHECKS`] checks run at once, counting the threads of earlier checks that have not ended; a
//! check that runs by itself after a failure waits for a place.

use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, LazyLock, Mutex, Weak};
use std::time::{Duration, Instant, SystemTime};

use craft_rtmp::Endpoint;
use serde_json::Value;
use studiocraft_model::services::Service;
use studiocraft_platform::net::Route;
use studiocraft_plugin::{Cause, Failure};

use super::{ALL_LOOKUPS, Clock, LOOKUP_WAIT, MAX_ALL_LOOKUPS, SystemClock, lock, reserve};

/// The longest a check takes before it reports, StudioCraft's own value.
pub const CHECK_TIME: Duration = Duration::from_secs(20);
/// How long one TCP connection, one TLS handshake or one fetch of a check may take, StudioCraft's
/// own value.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// The most checks that run at once in the process.
pub const MAX_CHECKS: u32 = 4;
/// The most evidence lines a check has: the route to any address, the proxy variables, the
/// lookup, a route, a connection and a connection on port 443 for each address tried, the list of
/// ingest servers, the other ingest servers and the status summary.
pub const MAX_EVIDENCE: usize = 1 + PROXY_VARIABLES.len() + 1 + 3 * craft_rtmp::MAX_ADDRESSES + 1 + OTHER_INGESTS + 1;
/// The largest list of ingest servers read, in bytes.
pub const INGEST_LIST_LIMIT: u64 = 256 * 1024;
/// The largest status summary read, in bytes.
pub const STATUS_LIMIT: u64 = 64 * 1024;
/// How many of the service's other ingest servers a check connects to.
pub const OTHER_INGESTS: usize = 2;
/// How many of the looked-up addresses the lookup's evidence line lists.
pub const LISTED_ADDRESSES: usize = 8;
/// The environment variables that name a proxy, which a check reports when set.
pub const PROXY_VARIABLES: [&str; 4] = ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"];
/// How many checks are kept, one per profile and destination id.
const MAX_KEPT: usize = 64;
/// How often a check looks at the clock again while it waits for its steps.
const POLL: Duration = Duration::from_millis(50);

/// What a check found, in one verdict. The first that fits wins, in this order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The system has no default route, and no route to any of the server's addresses that were
    /// checked; a failed lookup checks none.
    NoNetwork,
    /// The DNS server answered that the server's name does not exist (Windows only).
    NameNotFound,
    /// The DNS server did not answer for the server's name.
    DnsNoAnswer,
    /// The server's name could not be looked up for another reason.
    LookupFailed,
    /// The last failure was a refusal of the stream, and the server takes connections.
    Rejected,
    /// The TLS handshake refused the server's certificate.
    CertificateNotAccepted,
    /// The service's status summary reports a problem.
    ServiceReportsProblem,
    /// No address of the server connects, and another ingest server of the service does.
    IngestNotAnsweringOtherDoes,
    /// The server's port timed out, and port 443 of the same server connects.
    PortNotAnswering443Does,
    /// The server refused the TCP connection on its port.
    Refused,
    /// No host of the service that was tried answers, with a route and a working lookup.
    ServiceNotAnswering,
    /// The server does not answer on its port, with a route and a working lookup.
    ServerNotAnswering,
    /// The connection succeeds, and the last failure came after it.
    ConnectsThenFails,
    /// The connection succeeds, and there was no failure to explain or the last one came before a
    /// connection.
    Reachable,
    /// Nothing above fits.
    Unknown,
}

impl Verdict {
    /// Every verdict, in the order they are decided.
    pub const ALL: [Verdict; 15] = [
        Verdict::NoNetwork,
        Verdict::NameNotFound,
        Verdict::DnsNoAnswer,
        Verdict::LookupFailed,
        Verdict::Rejected,
        Verdict::CertificateNotAccepted,
        Verdict::ServiceReportsProblem,
        Verdict::IngestNotAnsweringOtherDoes,
        Verdict::PortNotAnswering443Does,
        Verdict::Refused,
        Verdict::ServiceNotAnswering,
        Verdict::ServerNotAnswering,
        Verdict::ConnectsThenFails,
        Verdict::Reachable,
        Verdict::Unknown,
    ];

    /// The stable id the control channel uses.
    pub fn id(self) -> &'static str {
        match self {
            Verdict::NoNetwork => "no_network",
            Verdict::NameNotFound => "name_not_found",
            Verdict::DnsNoAnswer => "dns_no_answer",
            Verdict::LookupFailed => "lookup_failed",
            Verdict::Rejected => "rejected",
            Verdict::CertificateNotAccepted => "certificate_not_accepted",
            Verdict::ServiceReportsProblem => "service_reports_problem",
            Verdict::IngestNotAnsweringOtherDoes => "ingest_not_answering_other_does",
            Verdict::PortNotAnswering443Does => "port_not_answering_443_does",
            Verdict::Refused => "refused",
            Verdict::ServiceNotAnswering => "service_not_answering",
            Verdict::ServerNotAnswering => "server_not_answering",
            Verdict::ConnectsThenFails => "connects_then_fails",
            Verdict::Reachable => "reachable",
            Verdict::Unknown => "unknown",
        }
    }
}

/// What one evidence line checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Check {
    /// The system's route to any address.
    DefaultRoute,
    /// The system's route to one of the server's addresses.
    Route,
    /// A proxy variable in the environment.
    Proxy,
    /// The lookup of the server's name.
    Lookup,
    /// A TCP connection to the server's port.
    Connect,
    /// A TCP connection and TLS handshake to the server's port.
    SecureConnect,
    /// A TCP connection and TLS handshake to port 443 of the server.
    Connect443,
    /// The service's list of ingest servers.
    IngestList,
    /// A TCP connection and TLS handshake to another ingest server of the service.
    OtherIngest,
    /// The service's status summary.
    StatusPage,
}

impl Check {
    /// The stable id the control channel uses.
    pub fn id(self) -> &'static str {
        match self {
            Check::DefaultRoute => "default_route",
            Check::Route => "route",
            Check::Proxy => "proxy",
            Check::Lookup => "lookup",
            Check::Connect => "connect",
            Check::SecureConnect => "secure_connect",
            Check::Connect443 => "connect_443",
            Check::IngestList => "ingest_list",
            Check::OtherIngest => "other_ingest",
            Check::StatusPage => "status_page",
        }
    }
}

/// How one evidence line came out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// It worked: a route, a connection, an answer.
    Ok,
    /// It failed, for this cause.
    Failed(Cause),
    /// The proxy variable is set.
    Set,
    /// The service's status summary reports a problem.
    Problem,
    /// The system could not say.
    Unknown,
    /// It had not finished when the check reported.
    Unfinished,
}

impl Outcome {
    /// The stable id the control channel uses: `ok`, `set`, `problem`, `unknown`, `unfinished`,
    /// or the id of the cause of a failure.
    pub fn id(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Failed(c) => c.id(),
            Outcome::Set => "set",
            Outcome::Problem => "problem",
            Outcome::Unknown => "unknown",
            Outcome::Unfinished => "unfinished",
        }
    }
}

/// One thing a check looked at, and what it found.
#[derive(Clone, Debug, PartialEq)]
pub struct Evidence {
    pub check: Check,
    /// The host, address or variable it looked at; empty for the route to any address.
    pub target: String,
    pub outcome: Outcome,
    /// What the system or the server reported, in English.
    pub detail: String,
}

/// What a check found: its verdict, with the values its message names, and the evidence.
#[derive(Clone, Debug, PartialEq)]
pub struct Diagnosis {
    pub verdict: Verdict,
    /// The server's host and port, and whether it is `rtmps://`.
    pub host: String,
    pub port: u16,
    pub tls: bool,
    /// The service's name, for a destination set up for one.
    pub service: Option<String>,
    /// The other ingest server that connected, for [`Verdict::IngestNotAnsweringOtherDoes`].
    pub other: Option<String>,
    /// The status summary's component and its status, or its overall description, for
    /// [`Verdict::ServiceReportsProblem`].
    pub component: Option<String>,
    pub status: Option<String>,
    /// Why the certificate was refused, for [`Verdict::CertificateNotAccepted`].
    pub reason: Option<Cause>,
    /// The last failure's or the lookup's error, in English.
    pub detail: String,
    /// At most [`MAX_EVIDENCE`] lines.
    pub evidence: Vec<Evidence>,
    /// When the check reported.
    pub at: SystemTime,
    /// Whether it ran by itself after a failure.
    pub automatic: bool,
}

impl Diagnosis {
    /// The verdict's message: an English template with `{name}` placeholders, and their values,
    /// to be translated when shown. A certificate's `reason` is English text for [`Diagnosis::reason`],
    /// which the window shows in its own words.
    pub fn message(&self) -> (&'static str, Vec<(&'static str, String)>) {
        let host = ("host", self.host.clone());
        let port = ("port", self.port.to_string());
        let service = ("service", self.service.clone().unwrap_or_default());
        match self.verdict {
            Verdict::NoNetwork => ("This computer has no network route", vec![]),
            Verdict::NameNotFound => ("The server name {host} does not exist; check the server address", vec![host]),
            Verdict::DnsNoAnswer => ("The DNS server did not answer for {host}", vec![host]),
            Verdict::LookupFailed => ("The server name {host} could not be looked up: {detail}", vec![host, ("detail", self.detail.clone())]),
            Verdict::Rejected => ("The server refused the stream; check the stream key and the server address", vec![]),
            Verdict::CertificateNotAccepted => (
                "The secure connection to {host} was not accepted ({reason}). The server address may be wrong, the server's certificate may be \
                 faulty, this computer's clock may be wrong, or something on this network may be intercepting secure connections",
                vec![host, ("reason", certificate_reason(self.reason.unwrap_or(Cause::CertificateNotAccepted)).to_string())],
            ),
            Verdict::ServiceReportsProblem => (
                "{service} reports a problem with {component}: {status}. This server may also be affected",
                vec![service, ("component", self.component.clone().unwrap_or_default()), ("status", self.status.clone().unwrap_or_default())],
            ),
            Verdict::IngestNotAnsweringOtherDoes => {
                ("{host} cannot be reached from this network, but {other} can", vec![host, ("other", self.other.clone().unwrap_or_default())])
            }
            Verdict::PortNotAnswering443Does => {
                ("{host} answers on port 443 but not on port {port}. This network or the server may be blocking port {port}", vec![host, port])
            }
            Verdict::Refused => {
                ("{host} refused the connection on port {port}; nothing on that server accepts connections on that port", vec![host, port])
            }
            Verdict::ServiceNotAnswering => (
                "None of the servers of {service} that were tried answer from this network. The service may be down, or something between this \
                 network and the service may be blocking it",
                vec![service],
            ),
            Verdict::ServerNotAnswering => {
                ("{host} does not answer on port {port}. The server may be down, or this network may be blocking it", vec![host, port])
            }
            Verdict::ConnectsThenFails if self.tls => (
                "A secure connection to {host} on port {port} succeeds; the failure came after that, while the stream was set up or sent: {detail}",
                vec![host, port, ("detail", self.detail.clone())],
            ),
            Verdict::ConnectsThenFails => (
                "A connection to {host} on port {port} succeeds; the failure came after that, while the stream was set up or sent: {detail}",
                vec![host, port, ("detail", self.detail.clone())],
            ),
            Verdict::Reachable if self.tls => ("A secure connection to {host} on port {port} succeeds", vec![host, port]),
            Verdict::Reachable => ("A connection to {host} on port {port} succeeds", vec![host, port]),
            Verdict::Unknown => ("The cause could not be determined", vec![]),
        }
    }

    /// The message in English.
    pub fn english(&self) -> String {
        let (template, args) = self.message();
        args.iter().fold(template.to_string(), |text, (k, v)| text.replace(&format!("{{{k}}}"), v))
    }
}

/// Why a certificate was refused, in English.
fn certificate_reason(c: Cause) -> &'static str {
    match c {
        Cause::CertificateUntrusted => "the server's certificate is not from a trusted authority",
        Cause::CertificateNameMismatch => "the server's certificate is for another name",
        Cause::CertificateExpired => "the server's certificate has expired",
        _ => "the server's certificate was not accepted",
    }
}

/// Every message template [`Diagnosis::message`] gives, for the translation tests.
pub fn templates() -> Vec<&'static str> {
    let mut out = Vec::new();
    for v in Verdict::ALL {
        for tls in [false, true] {
            let d = Diagnosis { verdict: v, tls, ..blank() };
            let (t, _) = d.message();
            if !out.contains(&t) {
                out.push(t);
            }
        }
    }
    out
}

fn blank() -> Diagnosis {
    Diagnosis {
        verdict: Verdict::Unknown,
        host: String::new(),
        port: 0,
        tls: false,
        service: None,
        other: None,
        component: None,
        status: None,
        reason: None,
        detail: String::new(),
        evidence: Vec::new(),
        at: SystemTime::UNIX_EPOCH,
        automatic: false,
    }
}

/// What to check. It has no `Debug`, since the server's address may end with the stream key.
#[derive(Clone)]
pub struct Subject {
    /// The name of the profile the destination belongs to, or a key of the stream that sends to
    /// it, which keeps apart destinations of different profiles and streams that have the same id.
    pub profile: String,
    /// The destination's id.
    pub id: String,
    /// The destination's server address. Only its scheme, host and port are used.
    pub server: String,
    /// The service the destination is set up for.
    pub service: Option<&'static Service>,
    /// The destination's last failure, when it has one.
    pub last: Option<Failure>,
    /// Whether the check also contacts the service's other hosts (step 5).
    pub service_hosts: bool,
    /// Whether the check runs by itself after a failure.
    pub automatic: bool,
    /// The owner's flag of whether the destination was checked. It is cleared here when the check
    /// waited for a place and then found no thread, so a later failure checks it again.
    pub checked: Option<Arc<AtomicBool>>,
    /// Whether the owner still wants this check, one flag per check: a check waiting for a place
    /// is dropped once the owner clears it. The flag stays locked while the check starts, so a
    /// clear either comes after the start or stops it.
    pub wanted: Option<Arc<Mutex<bool>>>,
}

/// Whether the server addresses `a` and `b` have the same scheme, host and port, so a failure of
/// one is a failure of the server a check of the other contacts.
pub fn same_server(a: &str, b: &str) -> bool {
    match (Endpoint::parse(a.trim()), Endpoint::parse(b.trim())) {
        (Ok(a), Ok(b)) => a.tls() == b.tls() && a.port() == b.port() && a.host().eq_ignore_ascii_case(b.host()),
        _ => false,
    }
}

/// A check of a destination, running or finished.
#[derive(Clone, Debug, PartialEq)]
pub enum CheckState {
    Running { automatic: bool },
    Done(Box<Diagnosis>),
}

/// Why a check did not start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckError {
    /// A check of this destination is running.
    Running,
    /// [`MAX_CHECKS`] checks are running.
    Busy,
    /// The server address is not valid.
    BadAddress(String),
    /// No thread could be started.
    NoThread(String),
}

impl std::fmt::Display for CheckError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CheckError::Running => f.write_str("a connection check of this destination is running"),
            CheckError::Busy => write!(f, "{MAX_CHECKS} connection checks are running; try again when one has finished"),
            CheckError::BadAddress(e) => write!(f, "the server address is not valid: {e}"),
            CheckError::NoThread(e) => write!(f, "no thread for the connection check: {e}"),
        }
    }
}

/// What a check uses from outside. Tests pass their own.
pub(crate) trait Probes: Send + Sync {
    /// The system's route to any of `addrs`, or to any address when empty.
    fn route(&self, addrs: &[SocketAddr]) -> Route;
    /// The proxy variables set in the environment.
    fn proxies(&self) -> Vec<&'static str>;
    fn lookup(&self, host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>>;
    /// A TCP connection to `addr`, and a TLS handshake for `host` over it with `tls`, each within
    /// `timeout`.
    fn reach(&self, host: &str, addr: SocketAddr, tls: bool, timeout: Duration) -> Result<(), craft_rtmp::Error>;
    /// The body of `url`, at most `limit` bytes, with no redirect followed and no proxy.
    fn fetch(&self, url: &str, limit: u64) -> Result<Vec<u8>, String>;
}

/// The system's network.
struct System;

impl Probes for System {
    fn route(&self, addrs: &[SocketAddr]) -> Route {
        studiocraft_platform::net::route_to(addrs)
    }

    fn proxies(&self) -> Vec<&'static str> {
        PROXY_VARIABLES.into_iter().filter(|v| std::env::var_os(v).is_some_and(|s| !s.is_empty())).collect()
    }

    fn lookup(&self, host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> {
        super::system_resolve(host, port)
    }

    fn reach(&self, host: &str, addr: SocketAddr, tls: bool, timeout: Duration) -> Result<(), craft_rtmp::Error> {
        let mut r = craft_rtmp::Reach::new(host, tls);
        r.connect_timeout = timeout;
        r.tls_timeout = timeout;
        craft_rtmp::reach(&r, addr)
    }

    fn fetch(&self, url: &str, limit: u64) -> Result<Vec<u8>, String> {
        fetch(url, limit, PROBE_TIMEOUT)
    }
}

/// The body of `url`, at most `limit` bytes, within `timeout`. A redirect is an error and is not
/// followed, and no proxy is used, whatever the environment says.
pub(crate) fn fetch(url: &str, limit: u64, timeout: Duration) -> Result<Vec<u8>, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .tls_config(ureq::tls::TlsConfig::builder().root_certs(ureq::tls::RootCerts::PlatformVerifier).build())
        .proxy(None)
        .max_redirects(0)
        .http_status_as_error(false)
        .timeout_global(Some(timeout))
        .build()
        .into();
    let response = agent.get(url).call().map_err(|e| e.to_string())?;
    let status = response.status().as_u16();
    if (300..400).contains(&status) {
        return Err(format!("the server answered with a redirect (HTTP {status}), which is not followed"));
    }
    if status != 200 {
        return Err(format!("the server answered HTTP {status}"));
    }
    response.into_body().into_with_config().limit(limit).read_to_vec().map_err(|e| e.to_string())
}

/// The time limits of a check. Tests shorten nothing: they move the clock.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    pub(crate) total: Duration,
    pub(crate) lookup: Duration,
    pub(crate) poll: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { total: CHECK_TIME, lookup: LOOKUP_WAIT, poll: POLL }
    }
}

/// One destination's check in [`Checks`].
struct Entry {
    profile: String,
    id: String,
    /// The server address the check contacts.
    server: String,
    /// The id of the service the check reads about, or empty.
    service: &'static str,
    state: CheckState,
    /// Which start this is, so a canceled check never writes over a later one.
    run: u64,
    cancel: Arc<AtomicBool>,
}

/// The connection checks of the process, by profile and destination id: at most [`MAX_CHECKS`]
/// running and [`MAX_KEPT`] kept.
pub struct Checks {
    probes: Arc<dyn Probes>,
    clock: Arc<dyn Clock>,
    limits: Limits,
    /// Checks whose threads have not all ended.
    running: Arc<AtomicU32>,
    runs: AtomicU64,
    entries: Mutex<VecDeque<Entry>>,
    /// Checks that run by themselves and found [`MAX_CHECKS`] checks running, oldest first: each
    /// starts when a place is free. At most [`MAX_KEPT`], one per destination.
    waiting: Mutex<VecDeque<Subject>>,
    /// Signaled when a check finishes and when the threads of a check have all ended.
    pub(crate) signal: Arc<Signal>,
}

static CHECKS: LazyLock<Arc<Checks>> = LazyLock::new(|| Checks::with(Arc::new(System), Arc::new(SystemClock), Limits::default()));

/// The process's connection checks, on the system's network.
pub fn checks() -> Arc<Checks> {
    Arc::clone(&CHECKS)
}

/// A count of changes, with a signal for each.
#[derive(Default)]
pub(crate) struct Signal {
    pub(crate) changes: Mutex<u64>,
    pub(crate) changed: Condvar,
}

impl Signal {
    fn touch(&self) {
        let mut changes = lock(&self.changes);
        *changes = changes.wrapping_add(1);
        self.changed.notify_all();
    }
}

/// Holds one of [`MAX_CHECKS`] places until every thread of a check has ended, then starts the
/// checks waiting for a place once the check's thread has started.
struct Slot {
    running: Arc<AtomicU32>,
    signal: Arc<Signal>,
    checks: Weak<Checks>,
    started: AtomicBool,
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.running.fetch_sub(1, Ordering::SeqCst);
        self.signal.touch();
        if self.started.load(Ordering::SeqCst)
            && let Some(checks) = self.checks.upgrade()
        {
            checks.start_waiting();
        }
    }
}

impl Checks {
    pub(crate) fn with(probes: Arc<dyn Probes>, clock: Arc<dyn Clock>, limits: Limits) -> Arc<Checks> {
        Arc::new(Checks {
            probes,
            clock,
            limits,
            running: Arc::new(AtomicU32::new(0)),
            runs: AtomicU64::new(0),
            entries: Mutex::new(VecDeque::new()),
            waiting: Mutex::new(VecDeque::new()),
            signal: Arc::new(Signal::default()),
        })
    }

    /// Starts a check of `s`. A running check of the same destination at the same server and
    /// service refuses it, and so do [`MAX_CHECKS`] running checks, except that a check that runs
    /// by itself then waits and starts when a place is free. A running or waiting check of the
    /// destination at another server or service is canceled.
    ///
    /// When [`MAX_KEPT`] checks already wait, a check that runs by itself is refused like any
    /// other.
    pub fn start(self: &Arc<Self>, s: Subject) -> Result<(), CheckError> {
        // The waiting checks stay locked from the try to the end, so a place freed meanwhile
        // finds a check that starts waiting here, and a cancel finds every check it should stop.
        let mut waiting = lock(&self.waiting);
        // The checks whose owners cleared their flag take no place.
        waiting.retain(|o| o.wanted.as_ref().is_none_or(|f| *lock(f)));
        let waits = s.automatic.then(|| s.clone());
        let out = match (self.try_start(s, &mut waiting), waits) {
            (Err(CheckError::Busy), Some(w)) if waiting.iter().any(|o| o.profile == w.profile && o.id == w.id) || waiting.len() < MAX_KEPT => {
                waiting.retain(|o| o.profile != w.profile || o.id != w.id);
                waiting.push_back(w);
                Ok(None)
            }
            (out, _) => out,
        };
        drop(waiting);
        // Letting the place go after the lock may start a waiting check.
        out.map(drop)
    }

    /// Starts the checks waiting for a place, oldest first, while places are free. A check that
    /// finds no place stays first, for the next place that is freed. One that finds no thread is
    /// dropped with a warning, and its `checked` flag is cleared. One whose `wanted` flag its
    /// owner cleared, because the destination went live or was started again, is dropped.
    fn start_waiting(self: &Arc<Self>) {
        let mut started = Vec::new();
        let mut waiting = lock(&self.waiting);
        while let Some(s) = waiting.front().cloned() {
            match self.try_start(s.clone(), &mut waiting) {
                Err(CheckError::Busy) => break,
                Ok(Some(slot)) => started.push(slot),
                Ok(None) => {
                    waiting.pop_front();
                }
                Err(e) => {
                    waiting.pop_front();
                    // A running check of the destination at its server reports instead.
                    if let CheckError::NoThread(_) = e {
                        log::warn!("stream destination `{}`: the connection was not checked: {e}", s.id);
                        if let Some(flag) = &s.checked {
                            flag.store(false, Ordering::SeqCst);
                        }
                    }
                }
            }
        }
        drop(waiting);
        drop(started);
    }

    /// Starts a check of `s` unless one of the destination runs at the same server and service
    /// or no place is free; removes the destination's waiting checks once it starts. Returns its
    /// place, which the caller lets go after the waiting checks' lock, or `None` when the owner no
    /// longer wants the check. The owner's flag stays locked until the check has started.
    fn try_start(self: &Arc<Self>, s: Subject, waiting: &mut VecDeque<Subject>) -> Result<Option<Arc<Slot>>, CheckError> {
        let flag = s.wanted.clone();
        let wanted = flag.as_ref().map(|f| lock(f));
        if wanted.as_ref().is_some_and(|w| !**w) {
            return Ok(None);
        }
        let endpoint = Endpoint::parse(s.server.trim()).map_err(|e| CheckError::BadAddress(e.to_string()))?;
        let (profile, id, server) = (s.profile.clone(), s.id.clone(), s.server.trim().to_string());
        let service = s.service.map_or("", |svc| svc.id);
        let mut entries = lock(&self.entries);
        let same = |e: &Entry| e.profile == profile && e.id == id;
        let running = |e: &Entry| same(e) && matches!(e.state, CheckState::Running { .. });
        if entries.iter().any(|e| running(e) && e.service == service && same_server(&e.server, &server)) {
            return Err(CheckError::Running);
        }
        if !reserve(&self.running, MAX_CHECKS) {
            return Err(CheckError::Busy);
        }
        let slot = Arc::new(Slot {
            running: Arc::clone(&self.running),
            signal: Arc::clone(&self.signal),
            checks: Arc::downgrade(self),
            started: AtomicBool::new(false),
        });
        let run = self.runs.fetch_add(1, Ordering::SeqCst);
        let cancel = Arc::new(AtomicBool::new(false));
        let (checks, flag, held) = (Arc::clone(self), Arc::clone(&cancel), Arc::clone(&slot));
        let automatic = s.automatic;
        let spawned = std::thread::Builder::new().name("stream check".into()).spawn(move || {
            let d = checks.run(&s, &endpoint, &flag, &held);
            if let Some(d) = d {
                checks.finish(run, d);
            }
        });
        if let Err(e) = spawned {
            return Err(CheckError::NoThread(e.to_string()));
        }
        slot.started.store(true, Ordering::SeqCst);
        for e in entries.iter().filter(|e| running(e)) {
            e.cancel.store(true, Ordering::SeqCst);
        }
        entries.retain(|e| !same(e));
        waiting.retain(|w| w.profile != profile || w.id != id);
        // The oldest finished checks make room; a running one is kept until it reports.
        while entries.len() >= MAX_KEPT {
            let Some(oldest) = entries.iter().position(|e| matches!(e.state, CheckState::Done(_))) else { break };
            entries.remove(oldest);
        }
        entries.push_back(Entry { profile, id, server, service, state: CheckState::Running { automatic }, run, cancel });
        drop(entries);
        drop(wanted);
        Ok(Some(slot))
    }

    /// The check of destination `id` of `profile`, running or finished; `None` when there has been
    /// none.
    pub fn state(&self, profile: &str, id: &str) -> Option<CheckState> {
        lock(&self.entries).iter().find(|e| e.profile == profile && e.id == id).map(|e| e.state.clone())
    }

    /// The latest check of destination `id` under any of the names in `profiles` that contacted
    /// the same server as `server` ([`same_server`]) for the same `service` (its id, or empty);
    /// `None` when there is none.
    pub fn latest(&self, profiles: &[&str], id: &str, server: &str, service: &str) -> Option<CheckState> {
        lock(&self.entries)
            .iter()
            .filter(|e| e.id == id && e.service == service && profiles.contains(&e.profile.as_str()) && same_server(&e.server, server))
            .max_by_key(|e| e.run)
            .map(|e| e.state.clone())
    }

    /// Moves the checks of the profile `old` to its new name `new`.
    pub fn rename_profile(&self, old: &str, new: &str) {
        // The waiting checks are locked first, as in `start`.
        let mut waiting = lock(&self.waiting);
        for w in waiting.iter_mut().filter(|w| w.profile == old) {
            w.profile = new.to_string();
        }
        for e in lock(&self.entries).iter_mut().filter(|e| e.profile == old) {
            e.profile = new.to_string();
        }
    }

    /// Forgets the checks of the profile `name`, which was removed. A running one goes on and
    /// reports nothing, and a waiting one does not start.
    pub fn forget_profile(&self, name: &str) {
        let mut waiting = lock(&self.waiting);
        waiting.retain(|w| w.profile != name);
        let mut entries = lock(&self.entries);
        for e in entries.iter().filter(|e| e.profile == name) {
            e.cancel.store(true, Ordering::SeqCst);
        }
        entries.retain(|e| e.profile != name);
    }

    /// Cancels a running check of destination `id` of `profile`: it stops waiting for its steps
    /// and reports nothing. A waiting one does not start, and a finished check is kept.
    pub fn cancel(&self, profile: &str, id: &str) {
        let mut waiting = lock(&self.waiting);
        waiting.retain(|w| w.profile != profile || w.id != id);
        let mut entries = lock(&self.entries);
        let running = |e: &Entry| e.profile == profile && e.id == id && matches!(e.state, CheckState::Running { .. });
        for e in entries.iter().filter(|e| running(e)) {
            e.cancel.store(true, Ordering::SeqCst);
        }
        entries.retain(|e| !running(e));
    }

    /// How many checks wait for a place.
    #[cfg(test)]
    pub(crate) fn waiting(&self) -> usize {
        lock(&self.waiting).len()
    }

    /// How many checks have threads that have not ended.
    pub fn running(&self) -> u32 {
        self.running.load(Ordering::SeqCst)
    }

    fn finish(&self, run: u64, d: Diagnosis) {
        let mut entries = lock(&self.entries);
        if let Some(e) = entries.iter_mut().find(|e| e.run == run) {
            e.state = CheckState::Done(Box::new(d));
        }
        drop(entries);
        self.signal.touch();
    }

    /// Runs the steps and decides; `None` when canceled.
    fn run(&self, s: &Subject, endpoint: &Endpoint, cancel: &AtomicBool, slot: &Arc<Slot>) -> Option<Diagnosis> {
        let start = self.clock.now();
        let (host, port, tls) = (endpoint.host().to_string(), endpoint.port(), endpoint.tls());
        let (tx, rx) = mpsc::channel::<Msg>();
        let job = Job { probes: Arc::clone(&self.probes), clock: Arc::clone(&self.clock), slot: Arc::clone(slot), tx };
        let mut r = Results { default_route: Some(self.probes.route(&[])), proxies: self.probes.proxies(), ..Results::default() };
        match host.parse::<IpAddr>() {
            Ok(ip) => {
                self.addresses(&mut r, &job, &host, port, tls, vec![SocketAddr::new(ip, port)]);
                // An address needs no lookup.
                r.lookup = Step::NotRun;
            }
            Err(_) => r.lookup = job.lookup(&host, port),
        }
        let service = s.service.filter(|_| s.service_hosts);
        if let Some(url) = service.and_then(|svc| svc.ingests) {
            r.ingests = job.fetch(url, INGEST_LIST_LIMIT, Msg::Ingests);
        }
        if let Some(url) = service.and_then(|svc| svc.status) {
            r.status = job.fetch(url, STATUS_LIMIT, Msg::Status);
        }
        // Set when the lookup was given up at its limit, so an answer timed within the limit that
        // is read later still counts.
        let mut lookup_given_up = false;
        loop {
            if cancel.load(Ordering::SeqCst) {
                return None;
            }
            let waited = self.clock.now().saturating_duration_since(start);
            let lookup_late = r.lookup.unfinished() && waited >= self.limits.lookup;
            if waited >= self.limits.total {
                if lookup_late {
                    r.lookup = Step::Done(Err(no_answer(self.limits.lookup)));
                }
                break;
            }
            // The replies already queued are read before the lookup is given up at its limit.
            let msg = match rx.try_recv() {
                Ok(msg) => Ok(msg),
                Err(_) => {
                    if lookup_late {
                        r.lookup = Step::Done(Err(no_answer(self.limits.lookup)));
                        lookup_given_up = true;
                    }
                    if !r.pending() {
                        break;
                    }
                    rx.recv_timeout(self.limits.poll)
                }
            };
            match msg {
                Ok(Msg::Lookup(answer, at)) if r.lookup.unfinished() || lookup_given_up => {
                    lookup_given_up = false;
                    if at.saturating_duration_since(start) >= self.limits.lookup {
                        r.lookup = Step::Done(Err(no_answer(self.limits.lookup)));
                        continue;
                    }
                    match answer {
                        Ok(addrs) if addrs.is_empty() => r.lookup = Step::Done(Err(Failure::new(Cause::LookupFailed, "the name has no address"))),
                        Ok(addrs) => self.addresses(&mut r, &job, &host, port, tls, addrs),
                        Err(e) => r.lookup = Step::Done(Err(super::failure(&craft_rtmp::Error::Resolve(e), false))),
                    }
                }
                Ok(Msg::Lookup(..)) => {}
                Ok(Msg::Port(i, out)) => set(&mut r.port, i, out),
                Ok(Msg::Port443(i, out)) => set(&mut r.port443, i, out),
                Ok(Msg::Other(i, out)) => set(&mut r.others, i, out),
                Ok(Msg::Ingests(body)) => {
                    let hosts = body.and_then(|b| other_ingests(&b, &host));
                    if let Ok(list) = &hosts {
                        for (i, other) in list.iter().enumerate() {
                            r.others.push((other.clone(), Step::Unfinished));
                            if !job.other(i, other) {
                                set(&mut r.others, i, Err(Failure::new(Cause::Other, "no thread for the check")));
                            }
                        }
                    }
                    r.ingests = Step::Done(hosts);
                }
                Ok(Msg::Status(body)) => {
                    let component = service.and_then(|svc| svc.status_component).unwrap_or("");
                    r.status = Step::Done(body.and_then(|b| status_report(&b, component)));
                }
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {}
            }
        }
        if cancel.load(Ordering::SeqCst) {
            return None;
        }
        let mut d = decide(&host, port, tls, s.service.map(|svc| svc.name), s.last.as_ref(), &r);
        d.automatic = s.automatic;
        d.at = SystemTime::now();
        Some(d)
    }

    /// Takes the server's addresses: checks the route to each of the first ones and starts their
    /// connections.
    fn addresses(&self, r: &mut Results, job: &Job, host: &str, port: u16, tls: bool, addrs: Vec<SocketAddr>) {
        let tried = first_of_each(&addrs);
        r.lookup = Step::Done(Ok(addrs));
        for (i, a) in tried.iter().enumerate() {
            r.routes.push((*a, self.probes.route(&[*a])));
            r.port.push((*a, Step::Unfinished));
            if !job.reach(host, *a, tls, move |out| Msg::Port(i, out)) {
                set(&mut r.port, i, Err(Failure::new(Cause::Other, "no thread for the check")));
            }
            if port != 443 {
                let a443 = SocketAddr::new(a.ip(), 443);
                r.port443.push((a443, Step::Unfinished));
                if !job.reach(host, a443, true, move |out| Msg::Port443(i, out)) {
                    set(&mut r.port443, i, Err(Failure::new(Cause::Other, "no thread for the check")));
                }
            }
        }
    }
}

/// The failure of a lookup that did not answer within `wait`.
fn no_answer(wait: Duration) -> Failure {
    Failure::new(Cause::LookupNoAnswer, format!("the server's name was not looked up within {} s", wait.as_secs()))
}

/// Up to [`craft_rtmp::MAX_ADDRESSES`] of `addrs`, alternating between the address families from
/// the first address's, as a stream's connection tries them.
fn first_of_each(addrs: &[SocketAddr]) -> Vec<SocketAddr> {
    let Some(first) = addrs.first() else { return Vec::new() };
    let mut same = addrs.iter().filter(|a| a.is_ipv6() == first.is_ipv6());
    let mut other = addrs.iter().filter(|a| a.is_ipv6() != first.is_ipv6());
    let mut out = Vec::new();
    while out.len() < craft_rtmp::MAX_ADDRESSES {
        let (a, b) = (same.next(), other.next());
        if a.is_none() && b.is_none() {
            break;
        }
        out.extend(a.into_iter().chain(b).copied());
    }
    out.truncate(craft_rtmp::MAX_ADDRESSES);
    out
}

fn set<K>(list: &mut [(K, Step<Result<(), Failure>>)], i: usize, out: Result<(), Failure>) {
    if let Some((_, step)) = list.get_mut(i) {
        *step = Step::Done(out);
    }
}

/// What a step's thread reports.
enum Msg {
    Lookup(std::io::Result<Vec<SocketAddr>>, Instant),
    Port(usize, Result<(), Failure>),
    Port443(usize, Result<(), Failure>),
    Other(usize, Result<(), Failure>),
    Ingests(Result<Vec<u8>, String>),
    Status(Result<Vec<u8>, String>),
}

/// What the threads of one check share.
struct Job {
    probes: Arc<dyn Probes>,
    clock: Arc<dyn Clock>,
    slot: Arc<Slot>,
    tx: Sender<Msg>,
}

impl Job {
    /// Runs `f` on a thread of its own that holds the check's place, and sends what it gives.
    /// Whether the thread started.
    fn spawn(&self, f: impl FnOnce(&dyn Probes, &dyn Clock) -> Msg + Send + 'static) -> bool {
        let (probes, clock, slot, tx) = (Arc::clone(&self.probes), Arc::clone(&self.clock), Arc::clone(&self.slot), self.tx.clone());
        std::thread::Builder::new()
            .name("stream check step".into())
            .spawn(move || {
                let msg = f(probes.as_ref(), clock.as_ref());
                // Fails when the check stopped waiting, which drops the answer.
                let _ = tx.send(msg);
                drop(slot);
            })
            .is_ok()
    }

    /// Starts the lookup of `host`, within the process's limit of lookups outstanding.
    fn lookup(&self, host: &str, port: u16) -> Step<Result<Vec<SocketAddr>, Failure>> {
        if !reserve(&ALL_LOOKUPS, MAX_ALL_LOOKUPS) {
            return Step::Done(Err(Failure::new(Cause::LookupsPending, "too many lookups of server names have not finished")));
        }
        let name = host.to_string();
        let started = self.spawn(move |p, clock| {
            let answer = p.lookup(&name, port);
            ALL_LOOKUPS.fetch_sub(1, Ordering::SeqCst);
            Msg::Lookup(answer, clock.now())
        });
        if !started {
            ALL_LOOKUPS.fetch_sub(1, Ordering::SeqCst);
            return Step::Done(Err(Failure::new(Cause::Other, "no thread for the lookup")));
        }
        Step::Unfinished
    }

    fn reach(&self, host: &str, addr: SocketAddr, tls: bool, msg: impl FnOnce(Result<(), Failure>) -> Msg + Send + 'static) -> bool {
        let name = host.to_string();
        self.spawn(move |p, _| msg(p.reach(&name, addr, tls, PROBE_TIMEOUT).map_err(|e| super::failure(&e, false))))
    }

    fn fetch<T>(&self, url: &'static str, limit: u64, msg: fn(Result<Vec<u8>, String>) -> Msg) -> Step<Result<T, String>> {
        if self.spawn(move |p, _| msg(p.fetch(url, limit))) { Step::Unfinished } else { Step::Done(Err("no thread for the check".into())) }
    }

    /// Looks `host`, another ingest server, up and connects to it on port 443 with TLS, as step
    /// `i` of the other ingests.
    fn other(&self, i: usize, host: &str) -> bool {
        if !reserve(&ALL_LOOKUPS, MAX_ALL_LOOKUPS) {
            let _ = self.tx.send(Msg::Other(i, Err(Failure::new(Cause::LookupsPending, "too many lookups of server names have not finished"))));
            return true;
        }
        let name = host.to_string();
        let started = self.spawn(move |p, clock| {
            let addrs = p.lookup(&name, 443);
            ALL_LOOKUPS.fetch_sub(1, Ordering::SeqCst);
            let out = match addrs {
                Ok(a) => {
                    let mut out = Err(Failure::new(Cause::LookupFailed, "the name has no address"));
                    // Each address in turn, until one connects, all within one probe time.
                    let start = clock.now();
                    for addr in first_of_each(&a) {
                        let left = PROBE_TIMEOUT.saturating_sub(clock.now().saturating_duration_since(start));
                        if left.is_zero() {
                            break;
                        }
                        out = p.reach(&name, addr, true, left).map_err(|e| super::failure(&e, false));
                        if out.is_ok() {
                            break;
                        }
                    }
                    out
                }
                Err(e) => Err(super::failure(&craft_rtmp::Error::Resolve(e), false)),
            };
            Msg::Other(i, out)
        });
        if !started {
            ALL_LOOKUPS.fetch_sub(1, Ordering::SeqCst);
        }
        started
    }
}

/// One step of a check.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) enum Step<T> {
    /// The step does not apply to this check.
    #[default]
    NotRun,
    /// The step had not finished when the check reported.
    Unfinished,
    Done(T),
}

impl<T> Step<T> {
    fn unfinished(&self) -> bool {
        matches!(self, Step::Unfinished)
    }
}

/// What the service's status summary says.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Status {
    /// The component's name, or empty for the summary as a whole.
    pub(crate) component: String,
    /// The status as the summary words it.
    pub(crate) status: String,
    /// Whether the status is anything but working.
    pub(crate) problem: bool,
}

/// What the steps of a check found.
#[derive(Debug, Default)]
pub(crate) struct Results {
    pub(crate) default_route: Option<Route>,
    pub(crate) proxies: Vec<&'static str>,
    pub(crate) lookup: Step<Result<Vec<SocketAddr>, Failure>>,
    pub(crate) routes: Vec<(SocketAddr, Route)>,
    pub(crate) port: Vec<(SocketAddr, Step<Result<(), Failure>>)>,
    pub(crate) port443: Vec<(SocketAddr, Step<Result<(), Failure>>)>,
    pub(crate) ingests: Step<Result<Vec<String>, String>>,
    pub(crate) others: Vec<(String, Step<Result<(), Failure>>)>,
    pub(crate) status: Step<Result<Status, String>>,
}

impl Results {
    /// Whether a step has not finished.
    fn pending(&self) -> bool {
        self.lookup.unfinished()
            || self.ingests.unfinished()
            || self.status.unfinished()
            || self.port.iter().chain(&self.port443).any(|(_, s)| s.unfinished())
            || self.others.iter().any(|(_, s)| s.unfinished())
    }
}

/// The host of an `rtmp://` or `rtmps://` address, lowercased; `None` when it has none or holds
/// anything but letters, digits, dots and hyphens.
pub(crate) fn host_of(url: &str) -> Option<String> {
    let rest = url.strip_prefix("rtmps://").or_else(|| url.strip_prefix("rtmp://"))?;
    let end = rest.find(['/', ':', '?', '#']).unwrap_or(rest.len());
    let host = rest.get(..end)?;
    let valid = !host.is_empty() && host.len() <= 253 && host.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');
    valid.then(|| host.to_ascii_lowercase())
}

/// The most entries of an ingest list read.
pub(crate) const MAX_INGESTS: usize = 512;

/// Up to [`OTHER_INGESTS`] hosts of the ingest list `body` other than `host`: those available, in
/// the order of their priority (lowest first) and then of the list, each once.
pub(crate) fn other_ingests(body: &[u8], host: &str) -> Result<Vec<String>, String> {
    let json: Value = serde_json::from_slice(body).map_err(|e| format!("the list of ingest servers is not valid JSON: {e}"))?;
    let Some(list) = json.get("ingests").and_then(Value::as_array) else {
        return Err("the list of ingest servers has no `ingests` list".into());
    };
    let mut found: Vec<(f64, String)> = Vec::new();
    for entry in list.iter().take(MAX_INGESTS) {
        let available = entry.get("availability").and_then(Value::as_f64).is_none_or(|a| a > 0.0);
        let url = entry.get("url_template_secure").or_else(|| entry.get("url_template")).and_then(Value::as_str);
        let Some(other) = url.and_then(host_of) else { continue };
        if !available || other.eq_ignore_ascii_case(host) || found.iter().any(|(_, h)| *h == other) {
            continue;
        }
        let priority = entry.get("priority").and_then(Value::as_f64).filter(|p| p.is_finite()).unwrap_or(f64::MAX);
        found.push((priority, other));
    }
    found.sort_by(|a, b| a.0.total_cmp(&b.0));
    Ok(found.into_iter().take(OTHER_INGESTS).map(|(_, h)| h).collect())
}

/// What the status summary `body` says about `component`, or about the whole service when it
/// does not list `component`.
pub(crate) fn status_report(body: &[u8], component: &str) -> Result<Status, String> {
    let json: Value = serde_json::from_slice(body).map_err(|e| format!("the status summary is not valid JSON: {e}"))?;
    let named = json
        .get("components")
        .and_then(Value::as_array)
        .and_then(|list| list.iter().find(|c| !component.is_empty() && c.get("name").and_then(Value::as_str) == Some(component)));
    if let Some(status) = named.and_then(|c| c.get("status")).and_then(Value::as_str) {
        let shown = status.replace('_', " ");
        return Ok(Status { component: component.to_string(), status: shown, problem: status != "operational" });
    }
    let overall = json.get("status");
    let indicator = overall.and_then(|s| s.get("indicator")).and_then(Value::as_str);
    let description = overall.and_then(|s| s.get("description")).and_then(Value::as_str);
    match (indicator, description) {
        (Some(i), Some(d)) => Ok(Status { component: String::new(), status: d.chars().take(200).collect(), problem: i != "none" }),
        _ => Err("the status summary names no status".into()),
    }
}

/// Whether a failure of `cause` came after the connection and the secure handshake succeeded.
fn after_connecting(c: Cause) -> bool {
    matches!(c, Cause::ClosedDuringSetup | Cause::SetupTimedOut | Cause::RejectedConnect | Cause::RejectedPublish | Cause::Lost | Cause::Other)
}

fn is_certificate(c: Cause) -> bool {
    matches!(c, Cause::CertificateUntrusted | Cause::CertificateNameMismatch | Cause::CertificateExpired | Cause::CertificateNotAccepted)
}

fn ok<T>(s: &Step<Result<T, Failure>>) -> bool {
    matches!(s, Step::Done(Ok(_)))
}

fn failed_with<T>(s: &Step<Result<T, Failure>>) -> Option<&Failure> {
    match s {
        Step::Done(Err(f)) => Some(f),
        _ => None,
    }
}

/// The verdict and evidence of a check of `host`:`port` from its results, with `last` the
/// destination's last failure.
pub(crate) fn decide(host: &str, port: u16, tls: bool, service: Option<&str>, last: Option<&Failure>, r: &Results) -> Diagnosis {
    let mut d =
        Diagnosis { host: host.to_string(), port, tls, service: service.map(str::to_string), evidence: evidence(host, port, tls, r), ..blank() };
    let any_ok = r.port.iter().any(|(_, s)| ok(s));
    let all_failed = !r.port.is_empty() && r.port.iter().all(|(_, s)| failed_with(s).is_some());
    let port_causes: Vec<Cause> = r.port.iter().filter_map(|(_, s)| failed_with(s)).map(|f| f.cause).collect();
    // Every address failed before a connection was made, and none answered with a refusal.
    let unreached = all_failed && port_causes.iter().all(|c| matches!(c, Cause::TimedOut | Cause::NoRoute));
    // A failed lookup checks no address, so with no default route it gives this verdict too. The
    // lookup's error does not say whether a DNS server was reached: only Windows reports a code,
    // and other systems report text that is not read.
    let no_route = r.default_route == Some(Route::None) && r.routes.iter().all(|(_, route)| *route == Route::None);
    let other_ok = r.others.iter().find(|(_, s)| ok(s)).map(|(h, _)| h.clone());
    let status_problem = match &r.status {
        Step::Done(Ok(s)) if s.problem => Some(s.clone()),
        _ => None,
    };
    d.verdict = if no_route {
        Verdict::NoNetwork
    } else if let Some(f) = failed_with(&r.lookup) {
        d.detail.clone_from(&f.detail);
        match f.cause {
            Cause::LookupNotFound => Verdict::NameNotFound,
            Cause::LookupNoAnswer => Verdict::DnsNoAnswer,
            _ => Verdict::LookupFailed,
        }
    } else if r.lookup.unfinished() {
        Verdict::DnsNoAnswer
    } else if any_ok && last.is_some_and(|f| matches!(f.cause, Cause::RejectedConnect | Cause::RejectedPublish)) {
        Verdict::Rejected
    } else if let Some(c) = port_causes.iter().copied().find(|c| tls && !any_ok && is_certificate(*c)) {
        d.reason = Some(c);
        Verdict::CertificateNotAccepted
    } else if let Some(s) = status_problem {
        d.component = Some(if s.component.is_empty() { service.unwrap_or_default().to_string() } else { s.component });
        d.status = Some(s.status);
        Verdict::ServiceReportsProblem
    } else if unreached && let Some(other) = other_ok {
        d.other = Some(other);
        Verdict::IngestNotAnsweringOtherDoes
    } else if all_failed && !port_causes.contains(&Cause::Refused) && port_causes.contains(&Cause::TimedOut) && r.port443.iter().any(|(_, s)| ok(s)) {
        Verdict::PortNotAnswering443Does
    } else if all_failed && port_causes.contains(&Cause::Refused) {
        Verdict::Refused
    } else if unreached
        && service.is_some()
        && !r.others.is_empty()
        && r.others.iter().all(|(_, s)| failed_with(s).is_some_and(|f| matches!(f.cause, Cause::TimedOut | Cause::NoRoute)))
    {
        Verdict::ServiceNotAnswering
    } else if all_failed && port_causes.iter().all(|c| matches!(c, Cause::TimedOut | Cause::NoRoute)) {
        Verdict::ServerNotAnswering
    } else if any_ok {
        match last.filter(|f| after_connecting(f.cause)) {
            Some(f) => {
                d.detail.clone_from(&f.detail);
                Verdict::ConnectsThenFails
            }
            // No failure, or one before a connection that now succeeds.
            None => Verdict::Reachable,
        }
    } else {
        Verdict::Unknown
    };
    d
}

fn route_outcome(route: Route) -> Outcome {
    match route {
        Route::None => Outcome::Failed(Cause::NoRoute),
        Route::Selected => Outcome::Ok,
        Route::Unknown => Outcome::Unknown,
    }
}

fn step_outcome<T>(s: &Step<Result<T, Failure>>) -> Option<(Outcome, String)> {
    match s {
        Step::NotRun => None,
        Step::Unfinished => Some((Outcome::Unfinished, String::new())),
        Step::Done(Ok(_)) => Some((Outcome::Ok, String::new())),
        Step::Done(Err(f)) => Some((Outcome::Failed(f.cause), f.detail.clone())),
    }
}

fn fetch_outcome<T>(s: &Step<Result<T, String>>, ok: impl Fn(&T) -> (Outcome, String)) -> Option<(Outcome, String)> {
    match s {
        Step::NotRun => None,
        Step::Unfinished => Some((Outcome::Unfinished, String::new())),
        Step::Done(Ok(v)) => Some(ok(v)),
        Step::Done(Err(e)) => Some((Outcome::Failed(Cause::Other), e.clone())),
    }
}

/// The first [`LISTED_ADDRESSES`] of `addrs`, followed by how many more there are.
fn listed_addresses(addrs: &[SocketAddr]) -> String {
    let mut out = addrs.iter().take(LISTED_ADDRESSES).map(|a| a.ip().to_string()).collect::<Vec<_>>().join(", ");
    let more = addrs.len().saturating_sub(LISTED_ADDRESSES);
    if more > 0 {
        out.push_str(&format!(" and {more} more"));
    }
    out
}

/// The evidence lines of `r`, at most [`MAX_EVIDENCE`].
fn evidence(host: &str, port: u16, tls: bool, r: &Results) -> Vec<Evidence> {
    let mut out = Vec::new();
    let mut push = |check: Check, target: String, (outcome, detail): (Outcome, String)| out.push(Evidence { check, target, outcome, detail });
    if let Some(route) = r.default_route {
        push(Check::DefaultRoute, String::new(), (route_outcome(route), String::new()));
    }
    for v in &r.proxies {
        push(Check::Proxy, (*v).to_string(), (Outcome::Set, String::new()));
    }
    let lookup = match &r.lookup {
        Step::Done(Ok(addrs)) => Some((Outcome::Ok, listed_addresses(addrs))),
        other => step_outcome(other),
    };
    if let Some(l) = lookup {
        push(Check::Lookup, host.to_string(), l);
    }
    for (a, route) in &r.routes {
        push(Check::Route, a.ip().to_string(), (route_outcome(*route), String::new()));
    }
    let connect = if tls { Check::SecureConnect } else { Check::Connect };
    for (a, s) in &r.port {
        if let Some(o) = step_outcome(s) {
            push(connect, SocketAddr::new(a.ip(), port).to_string(), o);
        }
    }
    for (a, s) in &r.port443 {
        if let Some(o) = step_outcome(s) {
            push(Check::Connect443, a.to_string(), o);
        }
    }
    if let Some(o) = fetch_outcome(&r.ingests, |hosts| (Outcome::Ok, hosts.join(", "))) {
        push(Check::IngestList, String::new(), o);
    }
    for (h, s) in &r.others {
        if let Some(o) = step_outcome(s) {
            push(Check::OtherIngest, h.clone(), o);
        }
    }
    let status = fetch_outcome(&r.status, |s| {
        let text = if s.component.is_empty() { s.status.clone() } else { format!("{}: {}", s.component, s.status) };
        (if s.problem { Outcome::Problem } else { Outcome::Ok }, text)
    });
    if let Some(o) = status {
        push(Check::StatusPage, String::new(), o);
    }
    out.truncate(MAX_EVIDENCE);
    out
}

#[cfg(test)]
pub(crate) mod tests;
