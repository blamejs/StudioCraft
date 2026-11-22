use std::io::{self, Read, Write};
use std::net::TcpListener;
use std::sync::PoisonError;

use super::*;

/// How long a test waits for something that should happen at once before it fails. Only a broken
/// build waits this long.
const GUARD: Duration = Duration::from_secs(60);

/// The documented address of `TEST-NET-1`, for addresses nothing answers.
fn at(last: u8, port: u16) -> SocketAddr {
    SocketAddr::from(([192, 0, 2, last], port))
}

fn timed_out(addr: SocketAddr) -> craft_rtmp::Error {
    craft_rtmp::Error::Connect(vec![(addr, io::Error::from(io::ErrorKind::TimedOut))])
}

type LookupFn = dyn Fn(&str, u16) -> io::Result<Vec<SocketAddr>> + Send + Sync;
type ReachFn = dyn Fn(&str, SocketAddr, bool) -> Result<(), craft_rtmp::Error> + Send + Sync;
type FetchFn = dyn Fn(&str) -> Result<Vec<u8>, String> + Send + Sync;

/// Probes that answer as a test sets them, and record every call.
pub(crate) struct Fake {
    pub(crate) default_route: Route,
    pub(crate) route: Route,
    pub(crate) proxies: Vec<&'static str>,
    pub(crate) lookup: Box<LookupFn>,
    pub(crate) reach: Box<ReachFn>,
    pub(crate) fetch: Box<FetchFn>,
    pub(crate) calls: Mutex<Vec<String>>,
}

impl Fake {
    /// Every route is there, a name looks up to one address, and every connection succeeds.
    pub(crate) fn new() -> Fake {
        Fake {
            default_route: Route::Selected,
            route: Route::Selected,
            proxies: Vec::new(),
            lookup: Box::new(|_, port| Ok(vec![at(1, port)])),
            reach: Box::new(|_, _, _| Ok(())),
            fetch: Box::new(|url| Err(format!("{url} is not set up"))),
            calls: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn calls(&self) -> Vec<String> {
        lock(&self.calls).clone()
    }

    fn note(&self, call: String) {
        lock(&self.calls).push(call);
    }
}

impl Probes for Fake {
    fn route(&self, addrs: &[SocketAddr]) -> Route {
        if addrs.is_empty() {
            self.note("route any".into());
            return self.default_route;
        }
        self.note(format!("route {addrs:?}"));
        self.route
    }

    fn proxies(&self) -> Vec<&'static str> {
        self.proxies.clone()
    }

    fn lookup(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        self.note(format!("lookup {host} {port}"));
        (self.lookup)(host, port)
    }

    fn reach(&self, host: &str, addr: SocketAddr, tls: bool, _timeout: Duration) -> Result<(), craft_rtmp::Error> {
        self.note(format!("reach {host} {addr} {tls}"));
        (self.reach)(host, addr, tls)
    }

    fn fetch(&self, url: &str, _limit: u64) -> Result<Vec<u8>, String> {
        self.note(format!("fetch {url}"));
        (self.fetch)(url)
    }
}

/// A clock the test moves on.
pub(crate) struct TestClock {
    base: Instant,
    ahead: Mutex<Duration>,
}

impl TestClock {
    pub(crate) fn new() -> Arc<TestClock> {
        Arc::new(TestClock { base: Instant::now(), ahead: Mutex::new(Duration::ZERO) })
    }

    pub(crate) fn advance(&self, by: Duration) {
        *lock(&self.ahead) += by;
    }
}

impl Clock for TestClock {
    fn now(&self) -> Instant {
        self.base + *lock(&self.ahead)
    }
}

pub(crate) fn checks_with(fake: Fake, clock: Arc<TestClock>) -> (Arc<Checks>, Arc<Fake>) {
    let fake = Arc::new(fake);
    let probes: Arc<dyn Probes> = Arc::clone(&fake) as Arc<dyn Probes>;
    (Checks::with(probes, clock, Limits::default()), fake)
}

/// Waits until `f` holds, checking again each time a check finishes or its threads end.
/// [`GUARD`] only ends a test that would otherwise hang.
pub(crate) fn until_checks<T>(checks: &Checks, f: impl Fn(&Checks) -> Option<T>) -> T {
    loop {
        let seen = *lock(&checks.signal.changes);
        if let Some(t) = f(checks) {
            return t;
        }
        let changes = lock(&checks.signal.changes);
        let (changes, waited) = checks.signal.changed.wait_timeout_while(changes, GUARD, |c| *c == seen).unwrap_or_else(PoisonError::into_inner);
        drop(changes);
        assert!(!waited.timed_out(), "the check did not change");
    }
}

/// Waits for the finished check of `id`, in the profile with no name.
pub(crate) fn done(checks: &Checks, id: &str) -> Diagnosis {
    done_in(checks, "", id)
}

/// Waits for the finished check of `id` in `profile`.
pub(crate) fn done_in(checks: &Checks, profile: &str, id: &str) -> Diagnosis {
    until_checks(checks, |c| match c.state(profile, id) {
        Some(CheckState::Done(d)) => Some(*d),
        _ => None,
    })
}

fn subject(server: &str) -> Subject {
    Subject { profile: String::new(), id: "a".into(), server: server.into(), service: None, last: None, service_hosts: false, automatic: false }
}

fn twitch() -> &'static Service {
    let Some(s) = studiocraft_model::services::service("twitch") else { panic!("no twitch preset") };
    s
}

fn ingest_list() -> Vec<u8> {
    serde_json::json!({ "ingests": [
        { "name": "Default", "default": true, "priority": 0, "availability": 1.0,
          "url_template_secure": "rtmps://usw.example/app/{stream_key}" },
        { "name": "Down", "priority": 1, "availability": 0.0, "url_template_secure": "rtmps://down.example/app/{stream_key}" },
        { "name": "West", "priority": 3, "availability": 1.0, "url_template_secure": "rtmps://west.example/app/{stream_key}" },
        { "name": "East", "priority": 2, "availability": 1.0, "url_template": "rtmp://east.example/app/{stream_key}" },
        { "name": "Third", "priority": 4, "availability": 1.0, "url_template_secure": "rtmps://third.example/app/{stream_key}" },
    ] })
    .to_string()
    .into_bytes()
}

fn summary(status: &str) -> Vec<u8> {
    serde_json::json!({
        "components": [{ "name": "Chat", "status": "major_outage" }, { "name": "Video (Broadcasting)", "status": status }],
        "status": { "indicator": "minor", "description": "Minor Service Outage" },
    })
    .to_string()
    .into_bytes()
}

#[test]
fn a_check_contacts_only_the_server_and_never_sends_the_key() {
    let mut fake = Fake::new();
    fake.proxies = vec!["HTTPS_PROXY"];
    // The stream's port times out, and port 443 of the same server answers.
    fake.reach = Box::new(|_, addr, _| if addr.port() == 443 { Ok(()) } else { Err(timed_out(addr)) });
    let (checks, fake) = checks_with(fake, TestClock::new());
    let server = "rtmp://ingest.example/live/secret-key?auth=hidden";
    checks.start(Subject { last: Some(Failure::new(Cause::TimedOut, "timed out")), ..subject(server) }).unwrap();
    let d = done(&checks, "a");
    assert_eq!(d.verdict, Verdict::PortNotAnswering443Does, "{d:?}");
    assert_eq!((d.host.as_str(), d.port, d.tls), ("ingest.example", 1935, false));
    assert_eq!(d.english(), "ingest.example answers on port 443 but not on port 1935. This network or the server may be blocking port 1935");
    let calls = fake.calls();
    for want in ["route any", "lookup ingest.example 1935", "reach ingest.example 192.0.2.1:1935 false", "reach ingest.example 192.0.2.1:443 true"] {
        assert!(calls.iter().any(|c| c == want), "{want} is missing from {calls:?}");
    }
    assert!(calls.iter().all(|c| !c.contains("secret") && !c.contains("hidden") && !c.starts_with("fetch")), "{calls:?}");
    let seen: Vec<(Check, &str, Outcome)> = d.evidence.iter().map(|e| (e.check, e.target.as_str(), e.outcome)).collect();
    assert_eq!(
        seen,
        [
            (Check::DefaultRoute, "", Outcome::Ok),
            (Check::Proxy, "HTTPS_PROXY", Outcome::Set),
            (Check::Lookup, "ingest.example", Outcome::Ok),
            (Check::Route, "192.0.2.1", Outcome::Ok),
            (Check::Connect, "192.0.2.1:1935", Outcome::Failed(Cause::TimedOut)),
            (Check::Connect443, "192.0.2.1:443", Outcome::Ok),
        ]
    );
    assert!(d.evidence.iter().all(|e| !e.detail.contains("secret")), "{:?}", d.evidence);
    until_checks(&checks, |c| (c.running() == 0).then_some(()));
}

#[test]
fn the_services_other_hosts_are_contacted_only_when_asked() {
    let mut fake = Fake::new();
    // The configured server does not answer; the service's other servers do, east.example only at
    // its second address.
    fake.lookup = Box::new(|host, port| Ok(if host == "east.example" { vec![at(1, port), at(2, port)] } else { vec![at(1, port)] }));
    fake.reach =
        Box::new(|host, addr, _| if host == "usw.example" || (host == "east.example" && addr == at(1, 443)) { Err(timed_out(addr)) } else { Ok(()) });
    fake.fetch = Box::new(|url| if url.contains("ingest") { Ok(ingest_list()) } else { Ok(summary("operational")) });
    let (checks, fake) = checks_with(fake, TestClock::new());
    let s = Subject { service: Some(twitch()), ..subject("rtmps://usw.example/app") };
    checks.start(s.clone()).unwrap();
    let d = done(&checks, "a");
    assert_eq!(d.verdict, Verdict::ServerNotAnswering, "{d:?}");
    assert!(fake.calls().iter().all(|c| !c.starts_with("fetch") && !c.contains("west") && !c.contains("east")), "{:?}", fake.calls());
    // Asked for: the list and the summary of the service the destination is set up for, and its
    // two first available other servers on port 443.
    lock(&fake.calls).clear();
    checks.start(Subject { service_hosts: true, ..s }).unwrap();
    let d = done(&checks, "a");
    assert_eq!((d.verdict, d.other.as_deref()), (Verdict::IngestNotAnsweringOtherDoes, Some("east.example")), "{d:?}");
    assert_eq!(d.english(), "usw.example cannot be reached from this network, but east.example can");
    let calls = fake.calls();
    let mut fetched: Vec<&str> = calls.iter().filter(|c| c.starts_with("fetch")).map(String::as_str).collect();
    fetched.sort_unstable();
    let (Some(ingests), Some(status)) = (twitch().ingests, twitch().status) else { panic!("twitch publishes both") };
    let mut want = [format!("fetch {ingests}"), format!("fetch {status}")];
    want.sort_unstable();
    assert_eq!(fetched, want.iter().map(String::as_str).collect::<Vec<_>>());
    for want in ["reach east.example 192.0.2.1:443 true", "reach east.example 192.0.2.2:443 true", "reach west.example 192.0.2.1:443 true"] {
        assert!(calls.iter().any(|c| c == want), "{want} is missing from {calls:?}");
    }
    assert!(calls.iter().all(|c| !c.contains("third") && !c.contains("down")), "{calls:?}");
    until_checks(&checks, |c| (c.running() == 0).then_some(()));
}

#[test]
fn a_problem_on_the_status_page_comes_before_the_connections() {
    let mut fake = Fake::new();
    fake.reach = Box::new(|_, addr, _| Err(timed_out(addr)));
    fake.fetch = Box::new(|url| if url.contains("ingest") { Err("HTTP 503".into()) } else { Ok(summary("partial_outage")) });
    let (checks, _fake) = checks_with(fake, TestClock::new());
    checks.start(Subject { service: Some(twitch()), service_hosts: true, ..subject("rtmps://usw.example/app") }).unwrap();
    let d = done(&checks, "a");
    assert_eq!(d.verdict, Verdict::ServiceReportsProblem, "{d:?}");
    let shown = format!("{} reports a problem with Video (Broadcasting): partial outage. This server may also be affected", twitch().name);
    assert_eq!(d.english(), shown);
    let list = d.evidence.iter().find(|e| e.check == Check::IngestList).map(|e| (e.outcome, e.detail.as_str()));
    assert_eq!(list, Some((Outcome::Failed(Cause::Other), "HTTP 503")));
    until_checks(&checks, |c| (c.running() == 0).then_some(()));
}

#[test]
fn a_lookup_that_never_answers_is_reported_after_the_lookup_wait() {
    let (started, wait_start) = mpsc::channel();
    let (release, hold) = mpsc::channel::<()>();
    let hold = Mutex::new(hold);
    let mut fake = Fake::new();
    fake.lookup = Box::new(move |_, _| {
        let _ = started.send(());
        let _ = lock(&hold).recv();
        Err(io::Error::other("late"))
    });
    let clock = TestClock::new();
    let (checks, _fake) = checks_with(fake, Arc::clone(&clock));
    checks.start(subject("rtmp://slow.example/live")).unwrap();
    wait_start.recv_timeout(GUARD).unwrap();
    assert_eq!(checks.state("", "a"), Some(CheckState::Running { automatic: false }));
    clock.advance(LOOKUP_WAIT);
    let d = done(&checks, "a");
    assert_eq!(d.verdict, Verdict::DnsNoAnswer, "{d:?}");
    assert_eq!(d.english(), "The DNS server did not answer for slow.example");
    // The lookup's thread keeps the check's place until the system returns.
    assert_eq!(checks.running(), 1);
    release.send(()).unwrap();
    until_checks(&checks, |c| (c.running() == 0).then_some(()));
    assert_eq!(checks.state("", "a").map(|s| matches!(s, CheckState::Done(_))), Some(true), "a late answer changes nothing");
}

#[test]
fn steps_still_running_at_the_deadline_are_reported_unfinished() {
    let (started, wait_start) = mpsc::channel();
    let (release, hold) = mpsc::channel::<()>();
    let hold = Mutex::new(hold);
    let mut fake = Fake::new();
    fake.reach = Box::new(move |_, _, _| {
        let _ = started.send(());
        let _ = lock(&hold).recv();
        Ok(())
    });
    let clock = TestClock::new();
    let (checks, _fake) = checks_with(fake, Arc::clone(&clock));
    checks.start(subject("rtmps://hang.example/app")).unwrap();
    wait_start.recv_timeout(GUARD).unwrap();
    clock.advance(CHECK_TIME);
    let d = done(&checks, "a");
    assert_eq!(d.verdict, Verdict::Unknown, "{d:?}");
    let connect = d.evidence.iter().find(|e| e.check == Check::SecureConnect).map(|e| e.outcome);
    assert_eq!(connect, Some(Outcome::Unfinished), "{:?}", d.evidence);
    drop(release);
    until_checks(&checks, |c| (c.running() == 0).then_some(()));
}

#[test]
fn one_check_runs_per_destination_and_at_most_four_in_all() {
    let (release, hold) = mpsc::channel::<()>();
    let (started, wait_start) = mpsc::channel();
    let hold = Mutex::new(hold);
    let mut fake = Fake::new();
    fake.reach = Box::new(move |_, _, _| {
        let _ = started.send(());
        let _ = lock(&hold).recv();
        Ok(())
    });
    let (checks, fake) = checks_with(fake, TestClock::new());
    for id in ["a", "b", "c", "d"] {
        checks.start(Subject { id: id.into(), ..subject("rtmps://203.0.113.9/app") }).unwrap();
        wait_start.recv_timeout(GUARD).unwrap();
    }
    assert!(fake.calls().iter().all(|c| !c.starts_with("lookup")), "an address needs no lookup");
    assert_eq!(checks.start(subject("rtmps://203.0.113.9/app")).err(), Some(CheckError::Running));
    assert_eq!(checks.start(Subject { id: "e".into(), ..subject("rtmps://203.0.113.9/app") }).err(), Some(CheckError::Busy));
    // A canceled check reports nothing, and keeps its place until its threads end.
    checks.cancel("Other", "a");
    assert_eq!(checks.state("", "a"), Some(CheckState::Running { automatic: false }), "another profile's `a` is another destination");
    checks.cancel("", "a");
    assert_eq!(checks.state("", "a"), None);
    assert_eq!(checks.start(Subject { id: "e".into(), ..subject("rtmps://203.0.113.9/app") }).err(), Some(CheckError::Busy));
    drop(release);
    until_checks(&checks, |c| (c.running() == 0).then_some(()));
    assert_eq!(checks.state("", "a"), None, "a canceled check never reports");
    assert!(matches!(checks.state("", "b"), Some(CheckState::Done(_))));
    assert!(checks.start(Subject { id: "e".into(), ..subject("rtmps://203.0.113.9/app") }).is_ok());
    done(&checks, "e");
    // The same id in another profile keeps its own check.
    checks.start(Subject { profile: "Other".into(), id: "b".into(), ..subject("rtmp://198.51.100.7/live") }).unwrap();
    assert_eq!(done_in(&checks, "Other", "b").host, "198.51.100.7");
    assert_eq!(checks.state("", "b").map(|s| matches!(s, CheckState::Done(d) if d.host == "203.0.113.9")), Some(true));
    assert!(same_server("rtmps://Host.example/app/key", "rtmps://host.example:443/other"));
    assert!(!same_server("rtmps://host.example/app", "rtmp://host.example/app"), "another scheme and port");
    assert!(!same_server("rtmp://host.example/app", "rtmp://host.example:1936/app"));
    assert!(!same_server("rtmp://host.example/app", "not an address"));
    let bad = checks.start(subject("http://ingest.example/app")).err();
    assert!(matches!(bad, Some(CheckError::BadAddress(_))), "{bad:?}");
    until_checks(&checks, |c| (c.running() == 0).then_some(()));
}

#[test]
fn a_destinations_new_server_replaces_its_running_check() {
    let (release, hold) = mpsc::channel::<()>();
    let (started, wait_start) = mpsc::channel();
    let hold = Mutex::new(hold);
    let mut fake = Fake::new();
    fake.reach = Box::new(move |_, _, _| {
        let _ = started.send(());
        let _ = lock(&hold).recv();
        Ok(())
    });
    let (checks, _fake) = checks_with(fake, TestClock::new());
    checks.start(subject("rtmps://203.0.113.9/app")).unwrap();
    wait_start.recv_timeout(GUARD).unwrap();
    assert_eq!(checks.start(subject("rtmps://203.0.113.9:443/other")).err(), Some(CheckError::Running), "the same server");
    checks.start(subject("rtmp://198.51.100.7/live")).unwrap();
    wait_start.recv_timeout(GUARD).unwrap();
    drop(release);
    until_checks(&checks, |c| (c.running() == 0).then_some(()));
    assert_eq!(done(&checks, "a").host, "198.51.100.7", "the replaced check reports nothing");
}

#[test]
fn another_ingest_servers_addresses_share_one_probe_time() {
    let clock = TestClock::new();
    let mut fake = Fake::new();
    fake.lookup = Box::new(|host, port| Ok(if host == "east.example" { (1..=4).map(|i| at(i, port)).collect() } else { vec![at(1, port)] }));
    // Each address of east.example takes the whole probe time and does not answer.
    let moved = Arc::clone(&clock);
    fake.reach = Box::new(move |host, addr, _| {
        if host == "east.example" {
            moved.advance(PROBE_TIMEOUT);
        }
        Err(timed_out(addr))
    });
    fake.fetch = Box::new(|url| if url.contains("ingest") { Ok(ingest_list()) } else { Ok(summary("operational")) });
    let (checks, fake) = checks_with(fake, clock);
    checks.start(Subject { service: Some(twitch()), service_hosts: true, ..subject("rtmps://usw.example/app") }).unwrap();
    done(&checks, "a");
    until_checks(&checks, |c| (c.running() == 0).then_some(()));
    let east: Vec<String> = fake.calls().into_iter().filter(|c| c.starts_with("reach east.example")).collect();
    assert_eq!(east, ["reach east.example 192.0.2.1:443 true"]);
}

#[test]
fn a_running_check_is_kept_when_finished_ones_make_room() {
    let (release, hold) = mpsc::channel::<()>();
    let hold = Mutex::new(hold);
    let mut fake = Fake::new();
    fake.reach = Box::new(move |host, _, _| {
        if host == "203.0.113.9" {
            let _ = lock(&hold).recv();
        }
        Ok(())
    });
    let (checks, _fake) = checks_with(fake, TestClock::new());
    checks.start(Subject { id: "old".into(), ..subject("rtmps://203.0.113.9/app") }).unwrap();
    // With the running check, the last of these needs one place more than are kept.
    for i in 0..MAX_KEPT {
        let id = format!("d{i}");
        checks.start(Subject { id: id.clone(), ..subject("rtmp://198.51.100.7/live") }).unwrap();
        done(&checks, &id);
        until_checks(&checks, |c| (c.running() == 1).then_some(()));
    }
    assert_eq!(checks.state("", "old"), Some(CheckState::Running { automatic: false }));
    assert_eq!(checks.state("", "d0"), None, "the oldest finished check made room");
    assert!(checks.state("", "d1").is_some());
    drop(release);
    assert!(done(&checks, "old").host == "203.0.113.9");
    until_checks(&checks, |c| (c.running() == 0).then_some(()));
}

#[test]
fn a_check_is_found_only_for_its_server_and_follows_its_profile() {
    let (checks, _fake) = checks_with(Fake::new(), TestClock::new());
    let server = "rtmp://198.51.100.7/live";
    checks.start(Subject { profile: "Show".into(), ..subject(server) }).unwrap();
    assert!(!done_in(&checks, "Show", "a").automatic);
    // Whether the latest check found is the automatic one.
    let found = |profiles: &[&str], server: &str| match checks.latest(profiles, "a", server, "") {
        Some(CheckState::Done(d)) => Some(d.automatic),
        _ => None,
    };
    assert_eq!(found(&["Show"], "rtmp://198.51.100.7/live/key"), Some(false));
    assert_eq!(found(&["Show"], "rtmp://198.51.100.8/live"), None, "a check of the old server is not shown for a new one");
    // A stream started under the profile's earlier name keeps its checks there; the newest wins.
    checks.start(Subject { profile: "Earlier".into(), automatic: true, ..subject(server) }).unwrap();
    assert!(done_in(&checks, "Earlier", "a").automatic);
    assert_eq!(found(&["Show", "Earlier"], server), Some(true));
    assert_eq!(found(&["Earlier", "Show"], server), Some(true));
    checks.rename_profile("Show", "Renamed");
    assert_eq!(found(&["Show"], server), None);
    assert_eq!(found(&["Renamed"], server), Some(false));
    checks.forget_profile("Renamed");
    assert_eq!(found(&["Renamed"], server), None);
    assert_eq!(found(&["Earlier"], server), Some(true), "another profile's checks are kept");
    // A check made for a service is not shown once the destination is set up for another.
    checks.start(Subject { profile: "Service".into(), service: Some(twitch()), ..subject(server) }).unwrap();
    assert!(done_in(&checks, "Service", "a").service.is_some());
    assert!(matches!(checks.latest(&["Service"], "a", server, twitch().id), Some(CheckState::Done(_))));
    assert_eq!(checks.latest(&["Service"], "a", server, ""), None);
    until_checks(&checks, |c| (c.running() == 0).then_some(()));
}

/// Results with the server's one address looked up and connected as `port` says.
fn results(port: Option<Result<(), Failure>>) -> Results {
    let a = at(1, 1935);
    Results {
        default_route: Some(Route::Selected),
        lookup: Step::Done(Ok(vec![a])),
        routes: vec![(a, Route::Selected)],
        port: port.map(|p| vec![(a, Step::Done(p))]).unwrap_or_default(),
        ..Results::default()
    }
}

fn failed(c: Cause) -> Result<(), Failure> {
    Err(Failure::new(c, c.id()))
}

#[test]
fn the_lookups_evidence_lists_a_bounded_number_of_addresses() {
    let mut r = results(Some(Ok(())));
    r.lookup = Step::Done(Ok((1..=200).map(|n| at(n, 1935)).collect()));
    let lines = evidence("h.example", 1935, true, &r);
    let lookup = lines.iter().find(|e| e.check == Check::Lookup).map(|e| e.detail.as_str());
    let listed: Vec<String> = (1..=LISTED_ADDRESSES).map(|n| format!("192.0.2.{n}")).collect();
    assert_eq!(lookup, Some(format!("{} and 192 more", listed.join(", ")).as_str()));
    r.lookup = Step::Done(Ok(vec![at(1, 1935), at(2, 1935)]));
    let lines = evidence("h.example", 1935, true, &r);
    let lookup = lines.iter().find(|e| e.check == Check::Lookup).map(|e| e.detail.as_str());
    assert_eq!(lookup, Some("192.0.2.1, 192.0.2.2"));
}

#[test]
fn each_verdict_follows_from_its_evidence() {
    let a = at(1, 443);
    let ok = Some(Ok(()));
    let rejected = Failure::new(Cause::RejectedPublish, "the server refused the stream");
    let lost = Failure::new(Cause::Lost, "the connection was lost");
    let problem = Status { component: "Video".into(), status: "major outage".into(), problem: true };
    let mut cases: Vec<(Results, Option<&Failure>, Option<&str>, Verdict)> = Vec::new();
    let mut r = results(Some(failed(Cause::NoRoute)));
    (r.default_route, r.routes[0].1) = (Some(Route::None), Route::None);
    cases.push((r, None, None, Verdict::NoNetwork));
    for (cause, verdict) in
        [(Cause::LookupNotFound, Verdict::NameNotFound), (Cause::LookupNoAnswer, Verdict::DnsNoAnswer), (Cause::LookupFailed, Verdict::LookupFailed)]
    {
        let mut r = results(None);
        r.lookup = Step::Done(Err(Failure::new(cause, "x")));
        cases.push((r, None, None, verdict));
    }
    cases.push((results(ok.clone()), Some(&rejected), None, Verdict::Rejected));
    cases.push((results(Some(failed(Cause::CertificateExpired))), None, None, Verdict::CertificateNotAccepted));
    let mut r = results(Some(failed(Cause::TimedOut)));
    r.status = Step::Done(Ok(problem.clone()));
    cases.push((r, None, Some("Svc"), Verdict::ServiceReportsProblem));
    let mut r = results(Some(failed(Cause::TimedOut)));
    r.others = vec![("b.example".into(), Step::Done(failed(Cause::TimedOut))), ("c.example".into(), Step::Done(Ok(())))];
    cases.push((r, None, Some("Svc"), Verdict::IngestNotAnsweringOtherDoes));
    let mut r = results(Some(failed(Cause::TimedOut)));
    r.port443 = vec![(a, Step::Done(Ok(())))];
    cases.push((r, None, None, Verdict::PortNotAnswering443Does));
    let mut r = results(Some(failed(Cause::Refused)));
    r.port443 = vec![(a, Step::Done(Ok(())))];
    cases.push((r, None, None, Verdict::Refused));
    let mut r = results(Some(failed(Cause::TimedOut)));
    r.others = vec![("b.example".into(), Step::Done(failed(Cause::TimedOut)))];
    cases.push((r, None, Some("Svc"), Verdict::ServiceNotAnswering));
    cases.push((results(Some(failed(Cause::TimedOut))), None, None, Verdict::ServerNotAnswering));
    cases.push((results(ok.clone()), Some(&lost), None, Verdict::ConnectsThenFails));
    cases.push((results(ok), None, None, Verdict::Reachable));
    cases.push((results(Some(failed(Cause::Other))), None, None, Verdict::Unknown));
    let mut decided = Vec::new();
    for (r, last, service, want) in &cases {
        let d = decide("h.example", 1935, true, *service, *last, r);
        assert_eq!(d.verdict, *want, "{r:?}");
        let english = d.english();
        assert!(!english.contains('{') && !english.is_empty(), "{want:?}: {english}");
        decided.push(d.verdict);
    }
    assert_eq!(decided, Verdict::ALL, "every verdict, in the order they are decided");
    let ids: Vec<&str> = Verdict::ALL.iter().map(|v| v.id()).collect();
    assert!(ids.iter().enumerate().all(|(i, id)| !ids[..i].contains(id)), "{ids:?}");
    // A connection that succeeds now after one that was refused is not a failure after connecting.
    let refused = Failure::new(Cause::Refused, "refused");
    assert_eq!(decide("h.example", 1935, true, None, Some(&refused), &results(Some(Ok(())))).verdict, Verdict::Reachable);
    // Other ingest servers that could not be looked up say nothing about whether they answer.
    let mut r = results(Some(failed(Cause::TimedOut)));
    r.others = vec![("b.example".into(), Step::Done(failed(Cause::LookupFailed)))];
    assert_eq!(decide("h.example", 1935, true, Some("Svc"), None, &r).verdict, Verdict::ServerNotAnswering);
    // A secure connection to one address outweighs a certificate another address refused.
    let mut r = results(Some(Ok(())));
    r.port.push((at(2, 1935), Step::Done(failed(Cause::CertificateExpired))));
    assert_eq!(decide("h.example", 1935, true, None, None, &r).verdict, Verdict::Reachable);
    // Every step of the longest check is listed, the status summary last.
    let mut r = results(Some(failed(Cause::TimedOut)));
    r.proxies = PROXY_VARIABLES.to_vec();
    let addrs: Vec<SocketAddr> = (1..=4).map(|i| at(i, 1935)).collect();
    r.lookup = Step::Done(Ok(addrs.clone()));
    r.routes = addrs.iter().map(|a| (*a, Route::Selected)).collect();
    r.port = addrs.iter().map(|a| (*a, Step::Done(failed(Cause::TimedOut)))).collect();
    r.port443 = addrs.iter().map(|a| (SocketAddr::new(a.ip(), 443), Step::Done(failed(Cause::TimedOut)))).collect();
    r.ingests = Step::Done(Ok(vec!["b.example".into(), "c.example".into()]));
    r.others = vec![("b.example".into(), Step::Done(failed(Cause::TimedOut))), ("c.example".into(), Step::Done(failed(Cause::TimedOut)))];
    r.status = Step::Done(Ok(problem.clone()));
    let d = decide("h.example", 1935, true, Some("Svc"), None, &r);
    assert_eq!(d.verdict, Verdict::ServiceReportsProblem, "{d:?}");
    assert_eq!(d.evidence.len(), MAX_EVIDENCE);
    assert_eq!(d.evidence.last().map(|e| (e.check, e.outcome)), Some((Check::StatusPage, Outcome::Problem)));
    // The other verdicts' values.
    let d = decide("h.example", 1935, true, None, None, &cases[5].0);
    assert_eq!(d.reason, Some(Cause::CertificateExpired), "{d:?}");
    assert!(
        d.english().starts_with("The secure connection to h.example was not accepted (the server's certificate has expired). "),
        "{}",
        d.english()
    );
    let d = decide("h.example", 1935, true, Some("Svc"), Some(&lost), &cases[13].0);
    assert_eq!(
        d.english(),
        "A secure connection to h.example on port 1935 succeeds; the failure came after that, while the stream was set up or sent: the connection was lost"
    );
    assert_eq!(templates().len(), 17, "two verdicts have a second message without TLS");
}

#[test]
fn the_ingest_list_and_the_status_summary_are_read_with_care() {
    assert_eq!(other_ingests(&ingest_list(), "usw.example"), Ok(vec!["east.example".to_string(), "west.example".to_string()]));
    assert_eq!(other_ingests(&ingest_list(), "EAST.example").map(|h| h.len()), Ok(2), "the same host is left out, in any case");
    let odd = serde_json::json!({ "ingests": [
        { "url_template_secure": "rtmps://user@evil.example/app" },
        { "url_template_secure": "https://web.example/app" },
        { "url_template_secure": "rtmps://ok.example:443/app" },
        { "url_template_secure": "rtmps://ok.example/app" },
        { "url_template_secure": 7 },
    ] });
    assert_eq!(other_ingests(odd.to_string().as_bytes(), "x"), Ok(vec!["ok.example".to_string()]), "odd hosts are skipped and repeats kept once");
    assert!(other_ingests(b"not json", "x").is_err());
    assert!(other_ingests(b"{\"servers\":[]}", "x").is_err());
    let c = "Video (Broadcasting)";
    let s = status_report(&summary("operational"), c).unwrap();
    assert_eq!((s.component.as_str(), s.status.as_str(), s.problem), (c, "operational", false));
    let s = status_report(&summary("degraded_performance"), c).unwrap();
    assert_eq!((s.status.as_str(), s.problem), ("degraded performance", true));
    let s = status_report(&summary("operational"), "Not listed").unwrap();
    assert_eq!((s.component.as_str(), s.status.as_str(), s.problem), ("", "Minor Service Outage", true), "the overall status is used");
    assert!(status_report(b"{}", c).is_err());
    assert!(status_report(b"[", c).is_err());
}

/// Answers each connection with the next of `answers`, then ends.
fn http_server(answers: Vec<Vec<u8>>) -> (String, std::thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let t = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for answer in answers {
            let (mut c, _) = listener.accept().unwrap();
            let mut seen = Vec::new();
            let mut buf = [0u8; 1024];
            while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = c.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                seen.extend_from_slice(&buf[..n]);
            }
            requests.push(String::from_utf8_lossy(&seen).into_owned());
            c.write_all(&answer).unwrap();
        }
        requests
    });
    (url, t)
}

#[test]
fn a_fetch_follows_no_redirect_and_reads_at_most_its_limit() {
    let elsewhere = TcpListener::bind("127.0.0.1:0").unwrap();
    elsewhere.set_nonblocking(true).unwrap();
    let redirect =
        format!("HTTP/1.1 302 Found\r\nLocation: http://{}/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", elsewhere.local_addr().unwrap());
    let big = format!("HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\n{}", "x".repeat(100));
    let small = "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello".to_string();
    let missing = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string();
    let (url, server) = http_server([redirect, big, small, missing].map(String::into_bytes).to_vec());
    let e = fetch(&url, 64, GUARD).err().unwrap_or_default();
    assert!(e.contains("redirect (HTTP 302)"), "{e}");
    assert!(matches!(elsewhere.accept(), Err(e) if e.kind() == io::ErrorKind::WouldBlock), "the redirect is not followed");
    assert!(fetch(&url, 64, GUARD).is_err(), "a body over the limit is not read");
    assert_eq!(fetch(&url, 64, GUARD), Ok(b"hello".to_vec()));
    assert_eq!(fetch(&url, 64, GUARD), Err("the server answered HTTP 404".to_string()));
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 4);
    assert!(requests.iter().all(|r| r.starts_with("GET / HTTP/1.1\r\n") && !r.to_ascii_lowercase().contains("cookie")), "{requests:?}");
}
