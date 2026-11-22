use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;

use craft_flv::amf0::{self, Value};
use craft_rtmp::{ChunkReader, ChunkWriter, MAX_MESSAGE, Message, MessageHeader, csid, msg_type, user_control};

use super::*;
use crate::native::tests::{Feeder, job};

/// The size of the handshake's C1, C2, S1 and S2 packets.
const PACKET: usize = 1536;
/// The stream id the test server gives in its answer to createStream.
const STREAM_ID: u32 = 1;
/// How long a test waits for something that should happen at once before it fails. Only a broken
/// build waits this long.
const GUARD: Duration = Duration::from_secs(60);

/// What the test server does with one connection.
#[derive(Clone, Copy, Debug)]
enum Plan {
    /// Take the stream and read it until the publisher closes it.
    Accept,
    /// Refuse the publish command.
    Refuse,
    /// Refuse the connect command.
    RefuseConnect,
    /// Take the stream and drop the connection after this many audio and video messages.
    DropAfter(usize),
}

/// What the test server saw.
#[derive(Clone, Debug, PartialEq)]
enum Seen {
    /// The publish command, with the stream's name.
    Publish(String),
    /// An audio or video message: video or not, its first two bytes, and its timestamp.
    Media { video: bool, head: [u8; 2], timestamp: u32 },
    /// The connection ended.
    Closed,
}

/// One server connection.
struct Conn {
    s: TcpStream,
    reader: ChunkReader,
    writer: ChunkWriter,
}

impl Conn {
    fn handshake(&mut self) -> io::Result<()> {
        let mut c0c1 = vec![0u8; 1 + PACKET];
        self.s.read_exact(&mut c0c1)?;
        let mut reply = vec![3, 0, 0, 0, 7, 0, 0, 0, 0];
        reply.extend((0..PACKET - 8).map(|i| (i * 7) as u8));
        reply.extend_from_slice(&c0c1[1..]);
        self.s.write_all(&reply)?;
        let mut c2 = vec![0u8; PACKET];
        self.s.read_exact(&mut c2)
    }

    fn recv(&mut self) -> Result<Message, craft_rtmp::Error> {
        let mut buf = [0u8; 16 * 1024];
        loop {
            if let Some(m) = self.reader.next_message()? {
                return Ok(m);
            }
            match self.s.read(&mut buf) {
                Ok(0) => return Err(craft_rtmp::Error::Closed),
                Ok(n) => self.reader.push(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(craft_rtmp::Error::Io(e)),
            }
        }
    }

    fn send(&mut self, csid: u32, type_id: u8, stream_id: u32, payload: &[u8]) -> io::Result<()> {
        self.writer.write_message(&mut self.s, &MessageHeader { csid, type_id, stream_id, timestamp: 0 }, &[payload])?;
        self.s.flush()
    }

    fn command(&mut self, stream_id: u32, values: &[Value]) -> io::Result<()> {
        let mut b = Vec::new();
        for v in values {
            v.write(&mut b).unwrap();
        }
        self.send(csid::COMMAND, msg_type::COMMAND, stream_id, &b)
    }

    fn status(&mut self, level: &str, code: &str) -> io::Result<()> {
        self.command(STREAM_ID, &[s("onStatus"), Value::Number(0.0), Value::Null, info(level, code)])
    }

    /// Answers everything up to the publish command, and returns the stream's name.
    fn serve_until_publish(&mut self) -> Result<String, craft_rtmp::Error> {
        loop {
            let m = self.recv()?;
            if m.header.type_id != msg_type::COMMAND {
                continue;
            }
            let values = amf0::read_all(&m.payload).unwrap();
            let name = values[0].as_str().unwrap_or_default().to_owned();
            let txn = values[1].clone();
            let io = craft_rtmp::Error::Io;
            match name.as_str() {
                "connect" => {
                    let window = 2_500_000u32.to_be_bytes();
                    self.send(csid::CONTROL, msg_type::WINDOW_ACK_SIZE, 0, &window).map_err(io)?;
                    let mut bw = window.to_vec();
                    bw.push(2);
                    self.send(csid::CONTROL, msg_type::SET_PEER_BANDWIDTH, 0, &bw).map_err(io)?;
                    let mut begin = user_control::STREAM_BEGIN.to_be_bytes().to_vec();
                    begin.extend_from_slice(&0u32.to_be_bytes());
                    self.send(csid::CONTROL, msg_type::USER_CONTROL, 0, &begin).map_err(io)?;
                    let props = Value::Object(vec![("fmsVer".into(), s("test/1,0"))]);
                    self.command(0, &[s("_result"), txn, props, info("status", "NetConnection.Connect.Success")]).map_err(io)?;
                }
                "releaseStream" => self.command(0, &[s("_result"), txn, Value::Null, Value::Undefined]).map_err(io)?,
                "createStream" => self.command(0, &[s("_result"), txn, Value::Null, Value::Number(f64::from(STREAM_ID))]).map_err(io)?,
                "publish" => return Ok(values.get(3).and_then(Value::as_str).unwrap_or_default().to_owned()),
                _ => {}
            }
        }
    }

    /// Answers the connect command with `_error`, then reads until the publisher closes the
    /// connection.
    fn refuse_connect(&mut self) -> Result<(), craft_rtmp::Error> {
        loop {
            let m = self.recv()?;
            if m.header.type_id != msg_type::COMMAND {
                continue;
            }
            let values = amf0::read_all(&m.payload).unwrap();
            if values[0].as_str() == Some("connect") {
                let txn = values[1].clone();
                self.command(0, &[s("_error"), txn, Value::Null, info("error", "NetConnection.Connect.Rejected")]).map_err(craft_rtmp::Error::Io)?;
            }
        }
    }

    /// Runs `plan`, telling `seen` what arrives.
    fn run(&mut self, plan: Plan, seen: &mpsc::Sender<Seen>) -> Result<(), craft_rtmp::Error> {
        self.handshake().map_err(craft_rtmp::Error::Io)?;
        if let Plan::RefuseConnect = plan {
            return self.refuse_connect();
        }
        let name = self.serve_until_publish()?;
        let _ = seen.send(Seen::Publish(name));
        if let Plan::Refuse = plan {
            self.status("error", "NetStream.Publish.BadName").map_err(craft_rtmp::Error::Io)?;
        } else {
            self.status("status", "NetStream.Publish.Start").map_err(craft_rtmp::Error::Io)?;
        }
        let mut media = 0;
        loop {
            let m = self.recv()?;
            let video = m.header.type_id == msg_type::VIDEO;
            if video || m.header.type_id == msg_type::AUDIO {
                let head = [m.payload.first().copied().unwrap_or(0), m.payload.get(1).copied().unwrap_or(0)];
                let _ = seen.send(Seen::Media { video, head, timestamp: m.header.timestamp });
                media += 1;
                if matches!(plan, Plan::DropAfter(n) if media >= n) {
                    return Ok(());
                }
            }
        }
    }
}

fn s(v: &str) -> Value {
    Value::String(v.to_owned())
}

fn info(level: &str, code: &str) -> Value {
    Value::Object(vec![("level".into(), s(level)), ("code".into(), s(code)), ("description".into(), s("Test."))])
}

/// A loopback server that runs one plan on each connection, in order, and then stops listening.
/// Returns its `rtmp://` URL and what it sees.
fn server(plans: Vec<Plan>) -> (String, mpsc::Receiver<Seen>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for plan in plans {
            let Ok((s, _)) = listener.accept() else { return };
            let mut c = Conn { s, reader: ChunkReader::new(MAX_MESSAGE), writer: ChunkWriter::new() };
            let _ = c.run(plan, &tx);
            drop(c);
            let _ = tx.send(Seen::Closed);
        }
    });
    (format!("rtmp://127.0.0.1:{port}/live"), rx)
}

/// A loopback address nothing listens on.
fn closed_url() -> String {
    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    format!("rtmp://127.0.0.1:{port}/live")
}

fn target(id: &str, server: &str, key: &str) -> Target {
    Target { id: id.into(), server: server.into(), key: key.into(), enabled: true }
}

/// Short waits before a try again, for the tests.
fn quick() -> Timing {
    let one = Duration::from_millis(1);
    Timing { first: one, longest: one, max_tries: 3, refused: one, jitter: false, ..Timing::default() }
}

/// Feeds the stream on a thread of its own until it is dropped.
struct Feed {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Feed {
    fn new(out: &Streaming) -> Feed {
        let (video, audio) = (out.video(), out.audio());
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            let mut f = Feeder::default();
            while !flag.load(Ordering::SeqCst) {
                f.feed(&video, &audio, 0.1);
            }
        });
        Feed { stop, thread: Some(thread) }
    }
}

impl Drop for Feed {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The next thing `rx` sees.
fn next(rx: &mpsc::Receiver<Seen>) -> Seen {
    rx.recv_timeout(GUARD).unwrap_or_else(|e| panic!("the server saw nothing more: {e}"))
}

/// Takes what `rx` sees up to `count` media messages, and returns those messages.
fn media(rx: &mpsc::Receiver<Seen>, count: usize) -> Vec<Seen> {
    let mut got = Vec::new();
    while got.len() < count {
        match next(rx) {
            m @ Seen::Media { .. } => got.push(m),
            Seen::Closed => panic!("the connection ended after {} media messages", got.len()),
            Seen::Publish(_) => {}
        }
    }
    got
}

/// Takes what `rx` sees until the connection ends.
fn until_closed(rx: &mpsc::Receiver<Seen>) {
    while next(rx) != Seen::Closed {}
}

/// Waits until `f` holds for the state of the destination `id`, checking again each time its
/// status is set. [`GUARD`] only ends a test that would otherwise hang.
fn until(out: &Streaming, id: &str, f: impl Fn(&DestinationState) -> bool) -> DestinationState {
    let Ok(dest) = out.hub.find(id) else { panic!("no destination `{id}`") };
    loop {
        let seen = *lock(&dest.changes);
        let d = dest.state();
        if f(&d) {
            return d;
        }
        let changes = lock(&dest.changes);
        let (changes, waited) = dest.changed.wait_timeout_while(changes, GUARD, |c| *c == seen).unwrap_or_else(PoisonError::into_inner);
        drop(changes);
        assert!(!waited.timed_out(), "{d:?}");
    }
}

/// Each connection sends the H.264 and AAC sequence headers first, then a keyframe.
fn starts_with_headers_and_a_keyframe(m: &[Seen]) {
    let heads: Vec<(bool, [u8; 2])> = m
        .iter()
        .take(3)
        .map(|s| match s {
            Seen::Media { video, head, .. } => (*video, *head),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(heads, [(true, [0x17, 0]), (false, [0xAF, 0]), (true, [0x17, 1])]);
}

#[test]
fn a_refused_destination_does_not_stop_the_other() {
    let (a, seen_a) = server(vec![Plan::Accept]);
    let (b, seen_b) = server(vec![Plan::Refuse]);
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let mut out = Streaming::start_timed(&job, vec![target("a", &a, "key-a"), target("b", &b, "key-b")], quick()).unwrap();
    let feed = Feed::new(&out);
    assert_eq!(next(&seen_a), Seen::Publish("key-a".into()));
    starts_with_headers_and_a_keyframe(&media(&seen_a, 3));
    assert_eq!(next(&seen_b), Seen::Publish("key-b".into()));
    let b = until(&out, "b", |d| matches!(d.status, DestinationStatus::Failed(_)));
    assert!(
        matches!(&b.status, DestinationStatus::Failed(f) if f.cause == Cause::RejectedPublish && f.detail.contains("NetStream.Publish.BadName")),
        "{b:?}"
    );
    until_closed(&seen_b);
    // The other destination goes on after the refusal, and the stream runs.
    media(&seen_a, 30);
    let a = until(&out, "a", |d| d.status == DestinationStatus::Live);
    assert!(a.bytes > 0 && a.reconnects == 0, "{a:?}");
    assert!(matches!(out.status(), OutputStatus::Starting | OutputStatus::Running), "{:?}", out.status());
    // The keys appear in no state, target or status.
    let shown = format!("{:?} {:?} {}", out.destinations(), out.status(), out.target());
    assert!(!shown.contains("key-a") && !shown.contains("key-b"), "{shown}");
    drop(feed);
    out.stop().unwrap();
    until_closed(&seen_a);
    assert_eq!(until(&out, "a", |_| true).status, DestinationStatus::Stopped);
}

#[test]
fn a_lost_connection_is_made_again() {
    let (a, seen) = server(vec![Plan::DropAfter(12), Plan::Accept]);
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let mut out = Streaming::start_timed(&job, vec![target("a", &a, "k")], quick()).unwrap();
    let feed = Feed::new(&out);
    starts_with_headers_and_a_keyframe(&media(&seen, 12));
    until_closed(&seen);
    // The second connection starts again with the headers and a keyframe, timed from 0.
    assert_eq!(next(&seen), Seen::Publish("k".into()));
    let again = media(&seen, 20);
    starts_with_headers_and_a_keyframe(&again);
    assert!(matches!(again[2], Seen::Media { timestamp: 0, .. }), "{again:?}");
    let d = until(&out, "a", |d| d.status == DestinationStatus::Live && d.reconnects == 1);
    assert!(d.bytes > 0, "{d:?}");
    assert_eq!(out.reconnects(), 1, "the sum over the destinations");
    assert_eq!(out.recoveries(), 1, "it went live again once after the loss");
    assert_eq!(out.destination_counts(), DestinationCounts { live: 1, retrying: 0 });
    drop(feed);
    out.stop().unwrap();
    until_closed(&seen);
}

#[test]
fn a_refusal_after_a_reconnect_is_tried_again() {
    let (a, seen) = server(vec![Plan::DropAfter(12), Plan::Refuse, Plan::Refuse, Plan::Accept]);
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let mut out = Streaming::start_timed(&job, vec![target("a", &a, "k")], Timing { max_tries: 10, ..quick() }).unwrap();
    let feed = Feed::new(&out);
    media(&seen, 12);
    until_closed(&seen);
    // Two refusals of the same key after the lost connection, then the server takes the stream.
    for _ in 0..2 {
        assert_eq!(next(&seen), Seen::Publish("k".into()));
        until_closed(&seen);
    }
    assert_eq!(next(&seen), Seen::Publish("k".into()));
    starts_with_headers_and_a_keyframe(&media(&seen, 5));
    let d = until(&out, "a", |d| d.status == DestinationStatus::Live);
    assert_eq!(d.reconnects, 1, "{d:?}");
    drop(feed);
    out.stop().unwrap();
    until_closed(&seen);
}

#[test]
fn a_fourth_refusal_after_a_reconnect_ends_the_destination() {
    let (a, seen) = server(vec![Plan::DropAfter(12), Plan::Refuse, Plan::Refuse, Plan::Refuse, Plan::Refuse]);
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let mut out = Streaming::start_timed(&job, vec![target("a", &a, "k")], Timing { max_tries: 10, ..quick() }).unwrap();
    let feed = Feed::new(&out);
    media(&seen, 12);
    until_closed(&seen);
    for _ in 0..4 {
        assert_eq!(next(&seen), Seen::Publish("k".into()));
        until_closed(&seen);
    }
    let d = until(&out, "a", |d| matches!(d.status, DestinationStatus::Failed(_)));
    let DestinationStatus::Failed(f) = &d.status else { panic!("{d:?}") };
    assert_eq!(f.cause, Cause::RejectedPublish, "{f:?}");
    drop(feed);
    assert!(out.stop().is_err());
}

#[test]
fn only_a_publish_refusal_after_a_lost_connection_is_tried_again() {
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let timing = || Timing { max_tries: 10, ..quick() };
    // A refused connect command after a reconnect stays final.
    let (a, seen) = server(vec![Plan::DropAfter(12), Plan::RefuseConnect]);
    let mut out = Streaming::start_timed(&job, vec![target("a", &a, "k")], timing()).unwrap();
    let feed = Feed::new(&out);
    media(&seen, 12);
    until_closed(&seen);
    until_closed(&seen);
    let d = until(&out, "a", |d| matches!(d.status, DestinationStatus::Failed(_)));
    assert!(matches!(&d.status, DestinationStatus::Failed(f) if f.cause == Cause::RejectedConnect), "{d:?}");
    drop(feed);
    assert!(out.stop().is_err());
    // A destination stopped and started again lost no connection, with the same key or another.
    for key in [None, Some("k2")] {
        let (b, seen) = server(vec![Plan::Accept, Plan::Refuse]);
        let mut out = Streaming::start_timed(&job, vec![target("b", &b, "k")], timing()).unwrap();
        let feed = Feed::new(&out);
        media(&seen, 5);
        out.stop_destination("b").unwrap();
        until_closed(&seen);
        until(&out, "b", |d| d.status == DestinationStatus::Stopped);
        out.start_destination("b", key).unwrap();
        assert_eq!(next(&seen), Seen::Publish(key.unwrap_or("k").into()));
        let d = until(&out, "b", |d| matches!(d.status, DestinationStatus::Failed(_)));
        assert!(matches!(&d.status, DestinationStatus::Failed(f) if f.cause == Cause::RejectedPublish), "{key:?}: {d:?}");
        drop(feed);
        let _ = out.stop();
    }
    // Stopped and started again while it waits after a refusal, it tries at once, and the next
    // refusal is final.
    let (c, seen) = server(vec![Plan::DropAfter(12), Plan::Refuse, Plan::Refuse]);
    let mut out = Streaming::start_timed(&job, vec![target("c", &c, "k")], Timing { refused: GUARD, ..timing() }).unwrap();
    let feed = Feed::new(&out);
    media(&seen, 12);
    until_closed(&seen);
    assert_eq!(next(&seen), Seen::Publish("k".into()));
    until(&out, "c", |d| matches!(&d.status, DestinationStatus::Retrying { failure, .. } if failure.cause == Cause::RejectedPublish));
    // A start of a destination that was not stopped changes nothing.
    out.start_destination("c", None).unwrap();
    assert!(!out.hub.find("c").unwrap().restarted.load(Ordering::SeqCst));
    out.stop_destination("c").unwrap();
    out.start_destination("c", None).unwrap();
    until_closed(&seen);
    assert_eq!(next(&seen), Seen::Publish("k".into()));
    let d = until(&out, "c", |d| matches!(d.status, DestinationStatus::Failed(_)));
    assert!(matches!(&d.status, DestinationStatus::Failed(f) if f.cause == Cause::RejectedPublish), "{d:?}");
    drop(feed);
    let _ = out.stop();
}

#[test]
fn a_restart_the_connection_outlived_does_not_change_the_next_try() {
    let (a, seen) = server(vec![Plan::DropAfter(12), Plan::Refuse, Plan::Accept]);
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let mut out = Streaming::start_timed(&job, vec![target("a", &a, "k")], Timing { max_tries: 10, ..quick() }).unwrap();
    assert_eq!(next(&seen), Seen::Publish("k".into()));
    until(&out, "a", |d| d.status == DestinationStatus::Live);
    // A stop and start that the sending loop did not see; every packet is sent after it.
    out.hub.find("a").unwrap().restarted.store(true, Ordering::SeqCst);
    let feed = Feed::new(&out);
    media(&seen, 12);
    until_closed(&seen);
    // The refusal follows the lost connection, so it is tried again, and the server takes the stream.
    assert_eq!(next(&seen), Seen::Publish("k".into()));
    until_closed(&seen);
    assert_eq!(next(&seen), Seen::Publish("k".into()));
    let d = until(&out, "a", |d| d.status == DestinationStatus::Live && d.reconnects == 1);
    assert_eq!(d.reconnects, 1);
    drop(feed);
    out.stop().unwrap();
    until_closed(&seen);
}

#[test]
fn the_counts_follow_the_destinations() {
    let (a, seen) = server(vec![Plan::Accept]);
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    // The wait before the next try outlasts the test, and the stop ends it.
    let waits = Timing { first: GUARD, longest: GUARD, ..quick() };
    let mut out = Streaming::start_timed(&job, vec![target("a", &a, "k"), target("b", &closed_url(), "k")], waits).unwrap();
    let feed = Feed::new(&out);
    media(&seen, 3);
    until(&out, "a", |d| d.status == DestinationStatus::Live);
    until(&out, "b", |d| matches!(d.status, DestinationStatus::Retrying { .. }));
    assert_eq!(out.destination_counts(), DestinationCounts { live: 1, retrying: 1 });
    assert_eq!(out.recoveries(), 0, "a destination that never went live has not recovered");
    out.stop_destination("b").unwrap();
    until(&out, "b", |d| d.status == DestinationStatus::Stopped);
    assert_eq!(out.destination_counts(), DestinationCounts { live: 1, retrying: 0 });
    drop(feed);
    out.stop().unwrap();
    until_closed(&seen);
    until(&out, "a", |d| d.status == DestinationStatus::Stopped);
    assert_eq!(out.destination_counts(), DestinationCounts::default());
}

#[test]
fn reconnect_off_fails_on_the_first_loss() {
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let off = Timing { reconnect: false, ..quick() };
    // A lost connection is not made again.
    let (a, seen) = server(vec![Plan::DropAfter(12), Plan::Accept]);
    let mut out = Streaming::start_timed(&job, vec![target("a", &a, "k")], off.clone()).unwrap();
    let feed = Feed::new(&out);
    let d = until(&out, "a", |d| matches!(d.status, DestinationStatus::Failed(_)));
    let DestinationStatus::Failed(f) = &d.status else { panic!("{d:?}") };
    assert_eq!(f.cause, Cause::Lost, "{f:?}");
    assert!(f.detail.contains("automatic reconnect is off") && !f.detail.contains("gave up"), "{f:?}");
    drop(feed);
    assert!(out.stop().is_err());
    assert_eq!(next(&seen), Seen::Publish("k".into()));
    // A connection that cannot be made is not tried again.
    let mut out = Streaming::start_timed(&job, vec![target("a", &closed_url(), "k")], off).unwrap();
    let d = until(&out, "a", |d| matches!(d.status, DestinationStatus::Failed(_)));
    let DestinationStatus::Failed(f) = &d.status else { panic!("{d:?}") };
    assert_eq!(f.cause, Cause::Refused, "{f:?}");
    assert!(f.detail.contains("automatic reconnect is off"), "{f:?}");
    assert!(out.stop().is_err());
}

#[test]
fn max_tries_gives_up_at_the_count() {
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    for tries in [1, 5] {
        let timing = Timing { max_tries: tries, ..quick() };
        let mut out = Streaming::start_timed(&job, vec![target("a", &closed_url(), "k")], timing).unwrap();
        let d = until(&out, "a", |d| matches!(d.status, DestinationStatus::Failed(_)));
        let DestinationStatus::Failed(f) = &d.status else { panic!("{d:?}") };
        assert!(f.detail.ends_with(&format!("gave up after {tries} tries")), "{tries}: {f:?}");
        assert!(out.stop().is_err());
    }
}

#[test]
fn the_waits_double_up_to_the_longest() {
    let secs = Duration::from_secs;
    let waits = |t: &Timing, n: u32| (1..=n).map(|f| t.delay(f)).collect::<Vec<_>>();
    assert_eq!(waits(&Timing::default(), 7), [secs(2), secs(4), secs(8), secs(16), secs(30), secs(30), secs(30)]);
    let five = Timing::from(ReconnectSettings { first_delay_s: 5, max_delay_s: 20, ..ReconnectSettings::default() });
    assert_eq!(waits(&five, 4), [secs(5), secs(10), secs(20), secs(20)]);
    let flat = Timing::from(ReconnectSettings { first_delay_s: 60, max_delay_s: 60, ..ReconnectSettings::default() });
    assert_eq!(waits(&flat, 3), [secs(60); 3]);
    // Large counts neither overflow nor pass the longest wait.
    let most = Timing::from(ReconnectSettings { first_delay_s: 60, max_delay_s: 600, max_tries: 100, enabled: true });
    assert_eq!((most.delay(33), most.delay(u32::MAX)), (secs(600), secs(600)));
    assert_eq!(most.delay(0), secs(60), "no failure yet waits the first wait");
    let off = Timing::from(ReconnectSettings { enabled: false, ..ReconnectSettings::default() });
    assert!(off.gives_up(1) && !Timing::default().gives_up(9) && Timing::default().gives_up(10));
}

#[test]
fn jitter_stays_within_a_fifth_and_above_a_second() {
    let mut state = seed("a");
    assert_ne!(state, 0);
    let mut seen = std::collections::BTreeSet::new();
    for wait in [Duration::from_secs(2), Duration::from_secs(30), Duration::from_secs(600), Duration::from_millis(1100)] {
        for _ in 0..2000 {
            let j = jittered(wait, next_random(&mut state));
            assert!(j <= wait, "{wait:?} -> {j:?}");
            assert!(j >= wait.mul_f64(0.8).max(Duration::from_secs(1)), "{wait:?} -> {j:?}");
            seen.insert((wait, j));
        }
    }
    // Each wait has 201 possible cuts, and the cut of 1.1 s stops at 1 s.
    assert!(seen.len() > 500, "the cuts vary: {}", seen.len());
    assert_eq!(jittered(Duration::from_secs(10), 0), Duration::from_secs(10), "no cut");
    assert_eq!(jittered(Duration::from_secs(10), 200), Duration::from_secs(8), "the largest cut");
    assert_eq!(jittered(Duration::from_secs(10), 201), Duration::from_secs(10), "the count wraps");
    assert_eq!(jittered(Duration::from_millis(5), 200), Duration::from_millis(5), "a wait below a second is kept");
    assert_eq!(jittered(Duration::from_millis(1100), 200), Duration::from_secs(1), "the cut stops at a second");
    assert_ne!(seed("a"), seed("b"));
}

#[test]
fn connections_lost_soon_after_they_are_made_count_as_failed_tries() {
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    // The server would take more connections than the destination tries.
    let (a, seen) = server(vec![Plan::DropAfter(2); 6]);
    let mut out = Streaming::start_timed(&job, vec![target("a", &a, "k")], quick()).unwrap();
    let feed = Feed::new(&out);
    let d = until(&out, "a", |d| matches!(d.status, DestinationStatus::Failed(_)));
    let DestinationStatus::Failed(f) = &d.status else { panic!("{d:?}") };
    assert!(f.detail.contains("gave up after 3 tries") && !f.detail.contains("could not connect"), "{f:?}");
    assert_eq!(f.cause, Cause::Lost, "{f:?}");
    assert_eq!(d.reconnects, 3);
    drop(feed);
    assert!(out.stop().is_err());
    drop(seen);
    // Connections that last count from one again, so the destination goes on.
    let (b, seen) = server(vec![Plan::DropAfter(2), Plan::DropAfter(2), Plan::DropAfter(2), Plan::Accept]);
    let mut out = Streaming::start_timed(&job, vec![target("b", &b, "k")], Timing { stable: Duration::ZERO, ..quick() }).unwrap();
    let feed = Feed::new(&out);
    let d = until(&out, "b", |d| d.reconnects == 3 && d.status == DestinationStatus::Live);
    assert!(d.bytes > 0, "{d:?}");
    assert_eq!(out.reconnects(), 3);
    drop(feed);
    out.stop().unwrap();
    drop(seen);
}

#[test]
fn one_destination_stops_and_starts_while_the_other_goes_on() {
    let (a, seen_a) = server(vec![Plan::Accept]);
    let (b, seen_b) = server(vec![Plan::Accept, Plan::Accept]);
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let mut off = target("c", &closed_url(), "k");
    off.enabled = false;
    let mut out = Streaming::start_timed(&job, vec![target("a", &a, "k"), target("b", &b, "k"), off], quick()).unwrap();
    let feed = Feed::new(&out);
    media(&seen_a, 5);
    media(&seen_b, 5);
    // A destination that is off is not started.
    assert_eq!(until(&out, "c", |_| true).status, DestinationStatus::Stopped);
    out.stop_destination("b").unwrap();
    until_closed(&seen_b);
    assert_eq!(until(&out, "b", |d| !matches!(d.status, DestinationStatus::Live)).status, DestinationStatus::Stopped);
    assert_eq!(out.target(), a, "the target lists the destinations that send");
    media(&seen_a, 20);
    // Started again with a key, it publishes with that key.
    out.start_destination("b", Some("k2")).unwrap();
    assert_eq!(next(&seen_b), Seen::Publish("k2".into()));
    starts_with_headers_and_a_keyframe(&media(&seen_b, 3));
    assert_eq!(out.target(), format!("{a}, {b}"));
    media(&seen_a, 5);
    // Starting one that is sending does nothing, and an unknown one is an error.
    out.start_destination("a", None).unwrap();
    out.start_destination("b", Some("k3")).unwrap();
    assert_eq!(*lock(&out.hub.find("b").unwrap().key), "k2", "a destination that sends keeps its key");
    assert!(out.start_destination("x", None).is_err() && out.stop_destination("x").is_err());
    drop(feed);
    out.stop().unwrap();
    until_closed(&seen_a);
    until_closed(&seen_b);
    assert!(out.start_destination("b", None).is_err(), "a stopped stream starts no destination");
}

/// A loopback server that takes one connection, says when it has it, and answers nothing until
/// `go` tells it what to do: `true` runs [`Plan::Accept`], `false` drops the connection.
fn held_server() -> (String, mpsc::Receiver<()>, mpsc::Sender<bool>, mpsc::Receiver<Seen>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (accepted_tx, accepted) = mpsc::channel();
    let (go, go_rx) = mpsc::channel::<bool>();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let Ok((s, _)) = listener.accept() else { return };
        let _ = accepted_tx.send(());
        let mut c = Conn { s, reader: ChunkReader::new(MAX_MESSAGE), writer: ChunkWriter::new() };
        if go_rx.recv_timeout(GUARD) == Ok(true) {
            let _ = c.run(Plan::Accept, &tx);
        }
        drop(c);
        let _ = tx.send(Seen::Closed);
    });
    (format!("rtmp://127.0.0.1:{port}/live"), accepted, go, rx)
}

#[test]
fn a_stop_that_lands_while_a_destination_connects_is_honored() {
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    // The connect goes on to publish after the stop: the destination closes without sending.
    // The connect fails after the stop: the destination stops instead of trying again.
    for completes in [true, false] {
        let (a, accepted, go, seen) = held_server();
        let mut out = Streaming::start_timed(&job, vec![target("a", &a, "k")], quick()).unwrap();
        let feed = Feed::new(&out);
        accepted.recv_timeout(GUARD).unwrap();
        assert_eq!(until(&out, "a", |_| true).status, DestinationStatus::Connecting);
        out.stop_destination("a").unwrap();
        // The thread is still inside the connect, so the stop has not ended it yet.
        assert_eq!(until(&out, "a", |_| true).status, DestinationStatus::Connecting, "completes: {completes}");
        go.send(completes).unwrap();
        if completes {
            assert_eq!(next(&seen), Seen::Publish("k".into()));
        }
        assert_eq!(next(&seen), Seen::Closed, "completes: {completes}: nothing was sent after the stop");
        let d = until(&out, "a", |d| !matches!(d.status, DestinationStatus::Connecting));
        assert_eq!((d.status, d.reconnects), (DestinationStatus::Stopped, 0), "completes: {completes}");
        let dest = out.hub.find("a").unwrap();
        // The thread sets the status and ends with its slot locked.
        let slot = lock(&dest.thread);
        assert!(!dest.running(), "completes: {completes}: the thread ended");
        drop(slot);
        assert!(out.target().is_empty(), "completes: {completes}: {}", out.target());
        drop(feed);
        out.stop().unwrap();
    }
}

#[test]
fn a_refused_destination_reports_refused() {
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    // The wait before the next try outlasts the test, and the stop ends it.
    let waits = Timing { first: GUARD, longest: GUARD, ..quick() };
    let mut out = Streaming::start_timed(&job, vec![target("a", &closed_url(), "k")], waits).unwrap();
    // A try again names the cause and the address that refused.
    let d = until(&out, "a", |d| matches!(d.status, DestinationStatus::Retrying { .. }));
    let DestinationStatus::Retrying { failure, .. } = &d.status else { panic!("{d:?}") };
    assert_eq!(failure.cause, Cause::Refused, "{failure:?}");
    assert_eq!(failure.addresses.len(), 1, "{failure:?}");
    assert!(failure.addresses[0].address.starts_with("127.0.0.1:"), "{failure:?}");
    assert_eq!(failure.addresses[0].cause, Cause::Refused, "{failure:?}");
    out.stop().unwrap();
    // Giving up keeps the cause.
    let mut out = Streaming::start_timed(&job, vec![target("a", &closed_url(), "k")], quick()).unwrap();
    let d = until(&out, "a", |d| matches!(d.status, DestinationStatus::Failed(_)));
    let DestinationStatus::Failed(f) = &d.status else { panic!("{d:?}") };
    assert_eq!(f.cause, Cause::Refused, "{f:?}");
    assert!(f.detail.contains("gave up after 3 tries"), "{f:?}");
    assert!(out.stop().is_err());
}

#[test]
fn causes_follow_the_error_kinds() {
    use craft_rtmp::{Error as E, TlsFailure, TlsKind};
    use std::io::{Error as IoError, ErrorKind as K};
    let tls = |kind| E::Tls(TlsFailure { kind, message: "m".into() });
    let at = |port: u16| std::net::SocketAddr::from(([192, 0, 2, 1], port));
    let connect = |kinds: &[K]| E::Connect(kinds.iter().enumerate().map(|(i, k)| (at(i as u16 + 1), IoError::from(*k))).collect());
    let cases: Vec<(E, bool, Cause)> = vec![
        (E::BadUrl("x"), false, Cause::BadAddress),
        (E::BadKey("x"), false, Cause::BadKey),
        (E::Resolve(IoError::other("no such host")), false, Cause::LookupFailed),
        (connect(&[]), false, Cause::LookupFailed),
        (connect(&[K::ConnectionRefused]), false, Cause::Refused),
        (connect(&[K::TimedOut, K::ConnectionRefused]), false, Cause::Refused),
        (connect(&[K::NetworkUnreachable, K::TimedOut]), false, Cause::TimedOut),
        (connect(&[K::NetworkUnreachable, K::HostUnreachable, K::NetworkDown]), false, Cause::NoRoute),
        (connect(&[K::NetworkUnreachable, K::PermissionDenied]), false, Cause::Other),
        (tls(TlsKind::UntrustedCertificate), false, Cause::CertificateUntrusted),
        (tls(TlsKind::NameMismatch), false, Cause::CertificateNameMismatch),
        (tls(TlsKind::Expired), false, Cause::CertificateExpired),
        (tls(TlsKind::CertificateNotAccepted), false, Cause::CertificateNotAccepted),
        (tls(TlsKind::HandshakeAlert), false, Cause::TlsFailed),
        (tls(TlsKind::Other), false, Cause::TlsFailed),
        (E::Rejected { command: "connect", code: String::new(), description: String::new() }, false, Cause::RejectedConnect),
        (E::Rejected { command: "publish", code: String::new(), description: String::new() }, false, Cause::RejectedPublish),
        // A refusal of another command, or of the stream after it started, names no setting to check.
        (E::Rejected { command: "createStream", code: String::new(), description: String::new() }, false, Cause::Other),
        (E::Rejected { command: "the stream", code: String::new(), description: String::new() }, true, Cause::Other),
        (E::Closed, false, Cause::ClosedDuringSetup),
        (E::Io(IoError::from(K::ConnectionReset)), false, Cause::ClosedDuringSetup),
        (E::Timeout("the handshake"), false, Cause::SetupTimedOut),
        (E::Closed, true, Cause::Lost),
        (E::Io(IoError::from(K::ConnectionReset)), true, Cause::Lost),
        (E::Stalled(Duration::from_secs(5)), true, Cause::Lost),
        (E::Broken, true, Cause::Lost),
        (E::Protocol("x"), false, Cause::Other),
    ];
    for (e, live, want) in cases {
        let f = failure(&e, live);
        assert_eq!(f.cause, want, "{e:?} (live {live})");
        assert_eq!(f.detail, e.to_string());
    }
    // Every address tried keeps its own outcome, in order.
    let f = failure(&connect(&[K::NetworkUnreachable, K::ConnectionRefused]), false);
    let each: Vec<(String, Cause)> = f.addresses.iter().map(|a| (a.address.clone(), a.cause)).collect();
    assert_eq!(each, [("192.0.2.1:1".to_string(), Cause::NoRoute), ("192.0.2.1:2".to_string(), Cause::Refused)]);
    // Windows reports the resolver's answer as a code; it is read only there.
    let not_found = failure(&E::Resolve(IoError::from_raw_os_error(11001)), false).cause;
    let no_answer = failure(&E::Resolve(IoError::from_raw_os_error(11002)), false).cause;
    if cfg!(windows) {
        assert_eq!((not_found, no_answer), (Cause::LookupNotFound, Cause::LookupNoAnswer));
    } else {
        assert_eq!((not_found, no_answer), (Cause::LookupFailed, Cause::LookupFailed));
    }
    // The ids are distinct.
    let ids: std::collections::HashSet<&str> = Cause::ALL.iter().map(|c| c.id()).collect();
    assert_eq!(ids.len(), Cause::ALL.len());
}

#[test]
fn the_stream_fails_when_every_destination_fails() {
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let mut one = Streaming::start_timed(&job, vec![target("a", &closed_url(), "k")], quick()).unwrap();
    until(&one, "a", |d| matches!(d.status, DestinationStatus::Failed(_)));
    let OutputStatus::Failed(e) = one.status() else { panic!("{:?}", one.status()) };
    assert!(e.contains("gave up after 3 tries"), "{e}");
    // The stop reports the failure, so it is not shown as a stream that ended well.
    let Err(stopped) = one.stop() else { panic!("a failed stream stopped without its failure") };
    assert!(stopped.to_string().contains("gave up after 3 tries"), "{stopped}");
    assert!(one.stop().is_ok(), "a second stop does nothing");

    let (live, seen) = server(vec![Plan::Accept]);
    let mut two = Streaming::start_timed(&job, vec![target("a", &closed_url(), "k"), target("b", &live, "k")], quick()).unwrap();
    let feed = Feed::new(&two);
    media(&seen, 3);
    until(&two, "a", |d| matches!(d.status, DestinationStatus::Failed(_)));
    assert!(matches!(two.status(), OutputStatus::Starting | OutputStatus::Running), "one destination still sends: {:?}", two.status());
    two.stop_destination("b").unwrap();
    until_closed(&seen);
    drop(feed);
    assert!(two.stop().is_err(), "the one left sending was stopped and the other failed");

    let mut both = Streaming::start_timed(&job, vec![target("a", &closed_url(), "k"), target("b", &closed_url(), "k")], quick()).unwrap();
    until(&both, "a", |d| matches!(d.status, DestinationStatus::Failed(_)));
    until(&both, "b", |d| matches!(d.status, DestinationStatus::Failed(_)));
    let OutputStatus::Failed(e) = both.status() else { panic!("{:?}", both.status()) };
    assert!(e.starts_with("every destination failed") && e.contains("a: ") && e.contains("b: "), "{e}");
    assert!(both.stop().is_err());
}

#[test]
fn a_stream_keeps_a_keyframe_interval_longer_than_a_recording_takes() {
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 10 });
    let mut out = Streaming::start_timed(&job, vec![target("a", &closed_url(), "k")], quick()).unwrap();
    // The destination may have given up already; the stop then reports it.
    let _ = out.stop();
}

#[test]
fn destinations_are_checked_before_the_stream_starts() {
    let j = job(RateControl::Bitrate { kbps: 400, keyframe_s: 2 });
    let ok = "rtmp://127.0.0.1:9/live";
    let mut off = target("a", ok, "k");
    off.enabled = false;
    let cases: Vec<(Vec<Target>, &str)> = vec![
        (vec![], "no destination"),
        (vec![target("", ok, "k")], "has no id"),
        (vec![target("a", ok, "k"), target("a", ok, "k")], "used twice"),
        (vec![target("a", "http://x/live", "k")], "destination `a`: the server URL is not valid"),
        (vec![target("a", ok, "a key")], "destination `a`: the stream key is not valid"),
        (vec![target("a", "rtmp://127.0.0.1:9/live", "")], "the stream key is not valid"),
        (vec![off], "every destination is off"),
        ((0..=MAX_SUBSCRIBERS).map(|i| target(&i.to_string(), ok, "k")).collect(), "at most 64 destinations"),
    ];
    for (targets, want) in cases {
        let Err(e) = Streaming::start_timed(&j, targets, quick()) else { panic!("{want}: the stream started") };
        assert!(e.to_string().contains(want), "{want}: {e}");
    }
    let Err(e) = Streaming::start_timed(&job(RateControl::Quality(23)), vec![target("a", ok, "k")], quick()) else { panic!("started") };
    assert!(e.to_string().contains("bitrate"), "{e}");
}

#[test]
fn a_destination_that_is_off_is_checked_when_it_starts() {
    let mut off = target("b", "rtmp://127.0.0.1:9/live", "");
    off.enabled = false;
    let mut out =
        Streaming::start_timed(&job(RateControl::Bitrate { kbps: 400, keyframe_s: 2 }), vec![target("a", &closed_url(), "k"), off], quick()).unwrap();
    let Err(e) = out.start_destination("b", None) else { panic!("a destination without a key started") };
    assert!(e.to_string().contains("destination `b`: the stream key is not valid"), "{e}");
    assert!(out.start_destination("b", Some(" ")).is_err(), "a blank key");
    assert_eq!(until(&out, "b", |_| true).status, DestinationStatus::Stopped);
    // Given a key later, it starts.
    out.start_destination("b", Some("late")).unwrap();
    assert_ne!(until(&out, "b", |_| true).status, DestinationStatus::Stopped);
    let _ = out.stop();
}

#[test]
fn a_key_at_the_end_of_the_address_is_split_off() {
    let with_key = target("a", " rtmp://host/live/name ", "").config().unwrap();
    assert_eq!((with_key.endpoint.app(), with_key.key.expose()), ("live", "name"));
    let separate = target("a", "rtmps://host/app/", "secret").config().unwrap();
    assert_eq!((separate.endpoint.app(), separate.key.expose()), ("app", "secret"));
    assert!(target("a", "rtmp://host/live", "").config().is_err());
    assert_eq!(split_name("rtmp://host/a/b/c"), Some(("rtmp://host/a/b", "c")));
    assert_eq!(split_name("rtmp://host/live"), None);
    assert_eq!(split_name("rtmp://host/live/name?auth=t"), Some(("rtmp://host/live", "name?auth=t")));
    assert!(!format!("{:?}", target("a", "rtmp://h/live", "secret")).contains("secret"));
    // A name at the end of the address is a key too, and status and logs leave it out.
    assert_eq!(target("a", " rtmp://host/live/name/ ", "").shown(), "rtmp://host/live");
    assert_eq!(target("a", "rtmp://host/live/name", "k").shown(), "rtmp://host/live/name");
    assert!(!format!("{:?}", target("a", "rtmp://h/live/secret", "")).contains("secret"));
    assert_eq!(target("a", "rtmps://host/app?token=secret", "k").shown(), "rtmps://host/app?<redacted>");
    assert_eq!(target("a", "rtmp://host/live/name?token=secret", "").shown(), "rtmp://host/live?<redacted>");
}

/// Every server a streaming service preset fills in connects over TLS with the key as given.
#[test]
fn every_preset_server_is_an_rtmps_address_the_client_takes() {
    let servers: Vec<(&str, &str)> = studiocraft_model::SERVICES.iter().filter_map(|s| s.server.map(|server| (s.id, server))).collect();
    assert!(!servers.is_empty());
    for (id, server) in servers {
        let c = target(id, server, "live_key").config().unwrap_or_else(|e| panic!("{id}: {server}: {e}"));
        assert!(c.endpoint.tls(), "{id}");
        assert_eq!((c.endpoint.port(), c.key.expose()), (443, "live_key"), "{id}");
        assert!(!c.endpoint.app().is_empty() && !c.endpoint.app().ends_with('/'), "{id}: {}", c.endpoint.app());
        assert_eq!(target(id, server, "live_key").shown(), server, "{id}: the server is shown as the preset gives it");
    }
}

#[test]
fn after_a_video_packet_is_left_out_video_waits_for_a_keyframe() {
    let fanout = Fanout::new();
    let control = fanout.control();
    let rates =
        StreamRates { video_packets_per_second: 30, audio_packets_per_second: 47, bits_per_second: 1_000_000, ticks_per_second: 1_000_000_000 };
    let mut rx = control.subscribe(RingConfig::for_stream(&rates, RING_SECONDS)).unwrap();
    let refused = Arc::new(AtomicU64::new(0));
    let mut sink = Publish { fanout, pool: PacketPool::new(16, 0).unwrap(), refused: Arc::clone(&refused), video_gap: false };
    let item = |track, at: i64, keyframe, data: Vec<u8>| Item { track, order_ns: at, pts_ns: at, dts_ns: at, keyframe, data };
    let mut write = |i: Item| sink.write(i).unwrap();
    write(item(Track::Video, 0, true, vec![1]));
    // A buffer larger than a packet may be is refused, and so are the frames after it.
    let mut too_large = Vec::with_capacity(craft_packet::MAX_BUFFER_BYTES + 1);
    too_large.push(2);
    write(item(Track::Video, 1, false, too_large));
    write(item(Track::Audio, 1, true, vec![3]));
    write(item(Track::Video, 2, false, vec![4]));
    write(item(Track::Video, 3, true, vec![5]));
    write(item(Track::Video, 4, false, vec![6]));
    drop(write);
    drop(sink);
    let mut got = Vec::new();
    while let Ok(p) = rx.try_recv() {
        got.push(p.data()[0]);
    }
    assert_eq!(got, [1, 3, 5, 6]);
    assert_eq!(refused.load(Ordering::Relaxed), 2);
}

#[test]
fn timestamps_count_from_the_first_packet_in_milliseconds() {
    let mut pool = PacketPool::new(4, 0).unwrap();
    let mut data = vec![0, 0, 0, 1, 0x65];
    // A frame shown 2 frames after it is decoded, at 30 fps.
    let p = pool.share(&mut data, Info::video(5_066_666_667, 5_000_000_000, false)).unwrap();
    let Some((t, Body::AvcNalus { composition_time, .. })) = body(&p, 4_000_000_000) else { panic!("not video") };
    assert_eq!((t, composition_time), (1000, 66));
    let mut early = vec![0x21];
    let p = pool.share(&mut early, Info::audio(3_990_000_000)).unwrap();
    assert!(matches!(body(&p, 4_000_000_000), Some((0, Body::AacRaw(_)))), "a packet before the first one is sent at 0");
    assert_eq!(Timing::default().delay(1), Duration::from_secs(2));
    assert_eq!(Timing::default().delay(9), Duration::from_secs(30));
}
