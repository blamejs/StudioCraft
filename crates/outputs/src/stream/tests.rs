use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use craft_flv::amf0::{self, Value};
use craft_rtmp::{ChunkReader, ChunkWriter, MAX_MESSAGE, Message, MessageHeader, csid, msg_type, user_control};

use studiocraft_media::pool::Pool;
use studiocraft_media::video::VideoFrame;
use studiocraft_model::Settings;

use super::*;
use crate::native::tests::{Feeder, decode, job, packets, temp_file};

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
    /// An audio or video message: video or not, its first two bytes, its timestamp, and the
    /// closed-caption pairs of a video message's SEI.
    Media { video: bool, head: [u8; 2], timestamp: u32, captions: Vec<[u8; 2]> },
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
                // A video tag's NAL units follow its five bytes of frame type, packet type and
                // composition time.
                let captions = match m.payload.get(5..) {
                    Some(au) if video && head[1] == 1 => sei::read_sei(Codec::H264, au),
                    _ => Vec::new(),
                };
                let _ = seen.send(Seen::Media { video, head, timestamp: m.header.timestamp, captions });
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

/// A loopback address nothing listens on. Port 1 is outside the range the system gives a listener
/// bound to port 0, so another test's server never takes it.
fn closed_url() -> String {
    "rtmp://127.0.0.1:1/live".into()
}

fn target(id: &str, server: &str, key: &str) -> Target {
    Target { id: id.into(), server: server.into(), key: key.into(), enabled: true, encoder: None }
}

/// Short waits before a try again, for the tests.
fn quick() -> Timing {
    let one = Duration::from_millis(1);
    Timing { first: one, longest: one, max_tries: 3, refused: one, jitter: false, ..Timing::default() }
}

/// The input queues of an encoder a [`Feed`] waits for, with the fan-out that ends with it.
type Paced = (Control, Arc<Fifo<Arc<PooledFrame>>>, Arc<Fifo<PooledBlock>>);

/// Feeds the stream on a thread of its own until it is dropped.
struct Feed {
    stop: Arc<AtomicBool>,
    /// The encoders whose queues it waits for before it feeds more.
    paced: Arc<Mutex<Vec<Paced>>>,
    thread: Option<JoinHandle<()>>,
}

impl Feed {
    fn new(out: &Streaming) -> Feed {
        let (video, audio) = (out.video(), out.audio());
        let stop = Arc::new(AtomicBool::new(false));
        let paced: Arc<Mutex<Vec<Paced>>> = Arc::default();
        let (flag, waits) = (Arc::clone(&stop), Arc::clone(&paced));
        let thread = std::thread::spawn(move || {
            let mut f = Feeder::default();
            // A split takes the input at once, so the feeder's own wait on the stream's queues
            // does not hold it back: it waits for every running encoder's queues instead.
            let busy = || lock(&waits).iter().any(|(c, v, a)| !c.is_ended() && (v.len() >= 4 || a.len() >= 64));
            while !flag.load(Ordering::SeqCst) {
                if busy() {
                    std::thread::yield_now();
                    continue;
                }
                f.feed(&video, &audio, 0.1);
            }
        });
        let feed = Feed { stop, paced, thread: Some(thread) };
        feed.pace(out);
        feed
    }

    /// Waits from here on for the queues of every encoder `out` runs, those started since the
    /// feed started included.
    fn pace(&self, out: &Streaming) {
        let mut all = vec![(out.hub.control.clone(), out.native.video(), out.native.audio())];
        all.extend(lock(&out.own).iter().map(|(f, n)| (f.control.clone(), n.video(), n.audio())));
        *lock(&self.paced) = all;
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
        let d = dest.state(&out.hub);
        if f(&d) {
            return d;
        }
        let changes = lock(&dest.changes);
        let (changes, waited) = dest.changed.wait_timeout_while(changes, GUARD, |c| *c == seen).unwrap_or_else(PoisonError::into_inner);
        drop(changes);
        assert!(!waited.timed_out(), "{d:?}");
    }
}

/// Checks that the thread of `id`, whose status shows it stopped or failed, no longer runs. The
/// thread sets that status and ends with its slot locked, so once the slot is taken it has ended.
fn until_ended(out: &Streaming, id: &str) {
    let Ok(dest) = out.hub.find(id) else { panic!("no destination `{id}`") };
    let slot = lock(&dest.thread);
    assert!(!dest.running(), "the thread of `{id}` still runs");
    drop(slot);
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
fn a_restart_while_it_waits_gets_every_try_again() {
    // A lost connection and a refusal are two failed tries of three.
    let (a, seen) = server(vec![Plan::DropAfter(12), Plan::Refuse, Plan::DropAfter(1), Plan::Accept]);
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let mut out = Streaming::start_timed(&job, vec![target("a", &a, "k")], Timing { refused: GUARD, ..quick() }).unwrap();
    let feed = Feed::new(&out);
    media(&seen, 12);
    until_closed(&seen);
    assert_eq!(next(&seen), Seen::Publish("k".into()));
    until(&out, "a", |d| matches!(&d.status, DestinationStatus::Retrying { failure, .. } if failure.cause == Cause::RejectedPublish));
    // A stop and start that reaches the waiting thread, as `spawn` makes it.
    let dest = out.hub.find("a").unwrap();
    dest.restarted.store(true, Ordering::SeqCst);
    if let Some(t) = lock(&dest.thread).as_ref() {
        t.thread().unpark();
    }
    until_closed(&seen);
    // The connection lost after the start is its first failed try, so it tries again.
    assert_eq!(next(&seen), Seen::Publish("k".into()));
    until_closed(&seen);
    assert_eq!(next(&seen), Seen::Publish("k".into()));
    let d = until(&out, "a", |d| d.status == DestinationStatus::Live);
    assert_eq!(d.reconnects, 2, "{d:?}");
    drop(feed);
    out.stop().unwrap();
    until_closed(&seen);
}

#[test]
fn a_restart_as_the_connection_is_lost_does_not_end_the_destination() {
    let timing = quick();
    let soon = Duration::ZERO;
    // Two failed tries of three: a third loss soon after connecting ends the destination, unless it
    // was started again meanwhile.
    assert!(timing.gives_up(failures_after_loss(&timing, 2, soon, false)));
    assert_eq!(failures_after_loss(&timing, 2, soon, true), 0);
    assert!(!timing.gives_up(failures_after_loss(&timing, 2, soon, true)));
    assert_eq!(failures_after_loss(&timing, 2, timing.stable, false), 1);
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
    assert_eq!(out.hub.find("b").unwrap().setup().key, "k2", "a destination that sends keeps its key");
    assert!(out.start_destination("x", None).is_err() && out.stop_destination("x").is_err());
    drop(feed);
    out.stop().unwrap();
    until_closed(&seen_a);
    until_closed(&seen_b);
    assert!(out.start_destination("b", None).is_err(), "a stopped stream starts no destination");
}

fn setup(id: &str, server: &str, key: &str) -> DestinationSetup {
    DestinationSetup { id: id.into(), server: server.into(), key: key.into(), service: String::new(), encoder: None }
}

#[test]
fn a_destination_is_added_and_moved_to_another_server_while_the_stream_runs() {
    let (a, seen_a) = server(vec![Plan::Accept]);
    let (b, seen_b) = server(vec![Plan::Accept]);
    let (c, seen_c) = server(vec![Plan::Accept]);
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let mut out = Streaming::start_timed(&job, vec![target("a", &a, "k")], quick()).unwrap();
    let feed = Feed::new(&out);
    media(&seen_a, 5);
    // A destination the stream was not started with is added, and sends from a keyframe on.
    out.start_destination_as(&DestinationSetup { service: "twitch".into(), ..setup("b", &b, "kb") }).unwrap();
    assert_eq!(next(&seen_b), Seen::Publish("kb".into()));
    starts_with_headers_and_a_keyframe(&media(&seen_b, 3));
    let ids: Vec<String> = out.destinations().into_iter().map(|d| d.id).collect();
    assert_eq!(ids, ["a", "b"], "an added destination follows those the stream started with");
    assert_eq!(out.hub.find("b").unwrap().setup().service.map(|s| s.id), Some("twitch"));
    assert_eq!(out.target(), format!("{a}, {b}"));
    // One that is sending keeps its server and key, so a setup it does not use is not checked.
    out.start_destination_as(&setup("b", &c, "kc")).unwrap();
    out.start_destination_as(&setup("b", "http://127.0.0.1/live", "")).unwrap();
    let dest = out.hub.find("b").unwrap();
    let now = dest.setup();
    assert_eq!((now.server.as_str(), now.key.as_str()), (b.as_str(), "kb"));
    // Stopped, it starts again at its new server with its new key, while the other goes on.
    out.stop_destination("b").unwrap();
    until_closed(&seen_b);
    until(&out, "b", |d| d.status == DestinationStatus::Stopped);
    out.start_destination_as(&setup("b", &c, "kc")).unwrap();
    assert_eq!(next(&seen_c), Seen::Publish("kc".into()));
    starts_with_headers_and_a_keyframe(&media(&seen_c, 3));
    assert_eq!(out.target(), format!("{a}, {c}"));
    assert_eq!(dest.setup().service, None, "it takes the service it is set up for now");
    media(&seen_a, 5);
    // A setup that is not valid is refused and adds nothing.
    assert!(out.start_destination_as(&setup("d", "http://127.0.0.1/live", "k")).is_err());
    assert!(out.start_destination_as(&setup("d", &c, "")).is_err(), "no key and no name at the end of the address");
    assert!(out.start_destination_as(&setup("", &c, "k")).is_err());
    assert_eq!(out.destinations().len(), 2);
    drop(feed);
    out.stop().unwrap();
    until_closed(&seen_a);
    until_closed(&seen_c);
    assert!(out.start_destination_as(&setup("e", &c, "k")).is_err(), "a stopped stream adds no destination");
}

#[test]
fn a_destination_started_again_before_its_stop_ended_it_takes_the_new_setup() {
    // The old server takes the stream, or refuses it, which alone would end the destination.
    for plan in [Plan::Accept, Plan::Refuse] {
        let (a, seen_a) = server(vec![Plan::Accept]);
        let (held, accepted, go, seen_held) = held_server_as(plan);
        let (c, seen_c) = server(vec![Plan::Accept]);
        let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
        let mut out = Streaming::start_timed(&job, vec![target("a", &a, "k")], quick()).unwrap();
        let feed = Feed::new(&out);
        media(&seen_a, 3);
        assert!(out.start_destination_as(&setup("b", &held, "kb")).unwrap(), "an added destination takes its setup");
        accepted.recv_timeout(GUARD).unwrap();
        out.stop_destination("b").unwrap();
        // The thread is still inside the connect, so the stop has not ended it yet.
        assert_eq!(until(&out, "b", |_| true).status, DestinationStatus::Connecting);
        assert!(out.start_destination_as(&setup("b", &c, "kc")).unwrap(), "one asked to stop takes the new setup");
        go.send(true).unwrap();
        // The connection made with the old setup closes before anything is sent on it, and the
        // next one goes to the new server with the new key.
        assert_eq!(next(&seen_held), Seen::Publish("kb".into()));
        assert_eq!(next(&seen_held), Seen::Closed, "{plan:?}");
        assert_eq!(next(&seen_c), Seen::Publish("kc".into()), "{plan:?}");
        starts_with_headers_and_a_keyframe(&media(&seen_c, 3));
        assert_eq!(out.target(), format!("{a}, {c}"));
        assert!(!out.start_destination_as(&setup("b", &held, "kb")).unwrap(), "one that is sending keeps its setup");
        drop(feed);
        out.stop().unwrap();
        until_closed(&seen_a);
        until_closed(&seen_c);
    }
}

/// Gives `d` the server `server` and the key `key` as a Stop and a Start that come together do
/// before its thread sees the stop: with its slot locked, the setup, then `renew` and
/// `restarted`, then a wake.
fn renewed(d: &Dest, server: &str, key: &str) {
    let slot = lock(&d.thread);
    assert!(set_up(d, Given { key: Some(key), server: Some((server, None)), ..Given::default() }));
    d.renew.store(true, Ordering::SeqCst);
    d.restarted.store(true, Ordering::SeqCst);
    if let Some(t) = slot.as_ref() {
        t.thread().unpark();
    }
}

#[test]
fn a_packet_that_comes_after_a_new_setup_is_not_sent_with_the_old_one() {
    let (a, seen_a) = server(vec![Plan::Accept]);
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    // The stream gives the headers; its own destination plays no part.
    let mut out = Streaming::start_timed(&job, vec![target("x", &closed_url(), "k")], quick()).unwrap();
    let main = Arc::clone(&out.hub.main);
    let d = Arc::new(Dest::new(target("a", &a, "k"), Arc::new(Counts::default()), None, Arc::clone(&main)));
    let mut p = Publisher::connect_to(&d.config().unwrap(), &[addr(&a)]).unwrap();
    assert_eq!(next(&seen_a), Seen::Publish("k".into()));
    let (mut fanout, mut rx, _) = copy_feed();
    let sender = {
        let d = Arc::clone(&d);
        std::thread::spawn(move || {
            let end = send_all(&d, &main.headers, &mut p, &mut rx);
            let _ = p.close();
            matches!(end, End::Done)
        })
    };
    // Once its headers are out, the thread waits for a packet.
    assert_eq!(media(&seen_a, 2).len(), 2, "the two sequence headers");
    let mut changes = lock(&d.changes);
    while d.status() != DestinationStatus::Live {
        let (next, waited) = d.changed.wait_timeout(changes, GUARD).unwrap_or_else(PoisonError::into_inner);
        changes = next;
        assert!(!waited.timed_out(), "{:?}", d.status());
    }
    drop(changes);
    // A new setup, then a packet that ends the wait: the packet does not go out with the old one.
    d.renew.store(true, Ordering::SeqCst);
    let mut pool = PacketPool::new(4, 0).unwrap();
    let packet = pool.share(&mut vec![0, 0, 0, 1, 0x65], Info::video(0, 0, true)).unwrap();
    assert_eq!(fanout.publish(&packet), 1);
    assert!(sender.join().unwrap(), "sending ends for the new setup");
    assert_eq!(next(&seen_a), Seen::Closed, "a packet went out with the old setup");
    let _ = out.stop();
}

#[test]
fn a_destination_started_again_after_its_failed_try_does_not_give_up() {
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let mut out = Streaming::start_timed(&job, vec![target("a", &closed_url(), "k")], quick()).unwrap();
    let d = Dest::new(target("b", &closed_url(), "k"), Arc::new(Counts::default()), None, Arc::clone(&out.hub.main));
    let f = Failure::new(Cause::Refused, "refused");
    // A Stop and a Start came after the try that failed: the thread goes on with the setup it was
    // given.
    d.alive.store(true, Ordering::SeqCst);
    d.restarted.store(true, Ordering::SeqCst);
    assert!(!fail(&out.hub, &d, f.clone(), &d.setup()));
    assert!(d.running() && !matches!(d.status(), DestinationStatus::Failed(_)), "{:?}", d.status());
    // Without one, it gives up.
    d.restarted.store(false, Ordering::SeqCst);
    assert!(fail(&out.hub, &d, f, &d.setup()));
    assert!(!d.running() && matches!(d.status(), DestinationStatus::Failed(_)), "{:?}", d.status());
    let _ = out.stop();
}

#[test]
fn a_start_that_finds_its_thread_ending_takes_the_new_setup() {
    let (a, seen_a) = server(vec![Plan::Accept]);
    let (c, seen_c) = server(vec![Plan::Accept]);
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let mut out = Streaming::start_timed(&job, vec![target("a", &a, "k")], quick()).unwrap();
    let feed = Feed::new(&out);
    media(&seen_a, 3);
    // `b` shows as running while the test holds its slot, as a thread does before it ends.
    let b = out.hub.find_or_add("b", None, &out.hub.main).unwrap();
    let slot = lock(&b.thread);
    b.alive.store(true, Ordering::SeqCst);
    let applied = std::thread::scope(|s| {
        let start = s.spawn(|| out.start_destination_as(&setup("b", &c, "kc")));
        // The thread decides to end with its slot locked, as `ends` and `fail` do.
        b.alive.store(false, Ordering::SeqCst);
        drop(slot);
        start.join().unwrap()
    });
    assert!(applied.unwrap(), "a start that finds no thread sending takes its setup");
    assert_eq!(next(&seen_c), Seen::Publish("kc".into()));
    starts_with_headers_and_a_keyframe(&media(&seen_c, 3));
    drop(feed);
    out.stop().unwrap();
    until_closed(&seen_a);
    until_closed(&seen_c);
}

#[test]
fn a_connection_never_pairs_the_key_of_one_setup_with_the_server_of_another() {
    let setups = [("rtmp://127.0.0.1:1935/live", "key-one"), ("rtmp://127.0.0.2:1935/live", "key-two")];
    let hosts = ["127.0.0.1", "127.0.0.2"];
    // A running stream gives the destination its encoder.
    let (a, _seen_a) = server(vec![Plan::Accept]);
    let mut out = Streaming::start_timed(&job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 }), vec![target("a", &a, "k")], quick()).unwrap();
    let main = Arc::clone(&out.hub.main);
    let d = Arc::new(Dest::new(target("b", setups[0].0, setups[0].1), Arc::new(Counts::default()), None, main));
    let start = Arc::new(std::sync::Barrier::new(2));
    let writer = {
        let (d, start) = (Arc::clone(&d), Arc::clone(&start));
        std::thread::spawn(move || {
            start.wait();
            for i in 0..200_000 {
                let (server, key) = setups[(i + 1) % 2];
                assert!(set_up(&d, Given { key: Some(key), server: Some((server, None)), ..Given::default() }), "each setup changes both");
            }
        })
    };
    start.wait();
    let mut reads = 0u64;
    loop {
        // Read after the check, so the last turn reads the final setup. A writer that panicked has
        // finished too, and the join below reports its panic.
        let finished = writer.is_finished();
        let s = d.setup();
        assert!(setups.contains(&(s.server.as_str(), s.key.as_str())), "a mixed setup: {} with {}", s.server, s.key);
        let c = d.config().unwrap();
        let pair = (c.endpoint.host(), c.key.expose());
        assert!(pair == (hosts[0], setups[0].1) || pair == (hosts[1], setups[1].1), "a connection to {} with the other key", pair.0);
        reads += 1;
        if finished {
            break;
        }
    }
    writer.join().unwrap();
    assert!(reads > 0);
    assert_eq!(d.setup().key, setups[0].1, "the last setup given is the one kept");
    out.stop().unwrap();
}

#[test]
fn an_added_destination_takes_the_place_of_one_that_ended_when_the_list_is_full() {
    let (a, seen_a) = server(vec![Plan::Accept]);
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let mut out = Streaming::start_timed(&job, vec![target("a", &a, "k")], quick()).unwrap();
    let feed = Feed::new(&out);
    media(&seen_a, 3);
    // Each added one has its connect command refused and fails, except d5, which loses its first
    // connection, makes it again and is stopped. The refusals come from this test's own server,
    // so a port another test frees and binds again takes none of these connections.
    let (refusing, _refused) = server(vec![Plan::RefuseConnect; 2 * MAX_SUBSCRIBERS]);
    let (five, seen_five) = server(vec![Plan::DropAfter(2), Plan::Accept, Plan::Accept]);
    for i in 1..MAX_SUBSCRIBERS {
        let id = format!("d{i}");
        if i == 5 {
            out.start_destination_as(&setup(&id, &five, "k")).unwrap();
            until(&out, &id, |d| d.reconnects == 1 && d.status == DestinationStatus::Live);
            out.stop_destination(&id).unwrap();
            until(&out, &id, |d| d.status == DestinationStatus::Stopped);
            until_ended(&out, &id);
            continue;
        }
        out.start_destination_as(&setup(&id, &refusing, "k")).unwrap();
        until(&out, &id, |d| matches!(d.status, DestinationStatus::Failed(_)));
        until_ended(&out, &id);
    }
    assert_eq!(out.destinations().len(), MAX_SUBSCRIBERS);
    let bytes = out.destinations().into_iter().find(|d| d.id == "d5").map(|d| d.bytes).unwrap();
    assert_eq!(out.reconnects(), 1);
    // The one stopped by request gives its place before the failed ones, and the stream's sums
    // keep its counts.
    out.start_destination_as(&setup("new", &refusing, "k")).unwrap();
    let ids: Vec<String> = out.destinations().into_iter().map(|d| d.id).collect();
    assert_eq!(ids.len(), MAX_SUBSCRIBERS);
    assert_eq!(ids.get(5).map(String::as_str), Some("new"), "{ids:?}");
    assert_eq!(out.reconnects(), 1);
    until(&out, "new", |d| matches!(d.status, DestinationStatus::Failed(_)));
    until_ended(&out, "new");
    // Then the first that failed. Added again under its id, d5 goes on from its counts.
    out.start_destination_as(&setup("d5", &five, "k")).unwrap();
    let ids: Vec<String> = out.destinations().into_iter().map(|d| d.id).collect();
    assert_eq!(ids.get(1).map(String::as_str), Some("d5"), "{ids:?}");
    assert!(ids.iter().any(|i| i == "a"), "a destination that sends keeps its place");
    let d = until(&out, "d5", |d| d.status == DestinationStatus::Live);
    assert_eq!(d.reconnects, 1, "{d:?}");
    assert!(d.bytes >= bytes, "{d:?}");
    assert_eq!(out.reconnects(), 1, "each connection made again is counted once");
    media(&seen_a, 3);
    drop(seen_five);
    drop(feed);
    out.stop().unwrap();
    until_closed(&seen_a);
}

/// A loopback server that takes one connection, says when it has it, and answers nothing until
/// `go` tells it what to do: `true` runs [`Plan::Accept`], `false` drops the connection.
fn held_server() -> (String, mpsc::Receiver<()>, mpsc::Sender<bool>, mpsc::Receiver<Seen>) {
    held_server_as(Plan::Accept)
}

/// [`held_server`], with `true` running `plan`.
fn held_server_as(plan: Plan) -> (String, mpsc::Receiver<()>, mpsc::Sender<bool>, mpsc::Receiver<Seen>) {
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
            let _ = c.run(plan, &tx);
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
    // The server can see the connection close before the destination's thread reports the stop.
    until(&two, "b", |d| d.status == DestinationStatus::Stopped);
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
    let mut sink = Publish { fanout, pool: PacketPool::new(16, 0).unwrap(), refused: Arc::clone(&refused), video_gap: false, captions: None };
    let item = |track, at: i64, keyframe, data: Vec<u8>| Item { track, audio: 0, order_ns: at, pts_ns: at, dts_ns: at, keyframe, data };
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

/// A clock that moves only when the test moves it.
struct TestClock {
    base: Instant,
    ahead: Mutex<Duration>,
}

impl TestClock {
    fn new() -> Arc<TestClock> {
        Arc::new(TestClock { base: Instant::now(), ahead: Mutex::new(Duration::ZERO) })
    }

    fn advance(&self, by: Duration) {
        *lock(&self.ahead) += by;
    }
}

impl Clock for TestClock {
    fn now(&self) -> Instant {
        self.base + *lock(&self.ahead)
    }
}

/// What the scripted resolver does for one lookup.
enum Answer {
    /// Answers at once.
    Now(io::Result<Vec<SocketAddr>>),
    /// Answers with these addresses once the test sends on the channel or drops its sender.
    After(mpsc::Receiver<()>, Vec<SocketAddr>),
}

/// A resolver that gives `answers` in order, whatever the name, and says on the returned channel
/// each time it is asked and each time it returns.
fn scripted(answers: Vec<Answer>) -> (Arc<Resolve>, mpsc::Receiver<&'static str>) {
    let queue = Mutex::new(VecDeque::from(answers));
    let (tx, rx) = mpsc::channel();
    let tx = Mutex::new(tx);
    let resolve = move |_: &str, _: u16| {
        let next = lock(&queue).pop_front();
        let _ = lock(&tx).send("asked");
        let answer = match next {
            Some(Answer::Now(r)) => r,
            Some(Answer::After(release, addrs)) => {
                let _ = release.recv();
                Ok(addrs)
            }
            None => Err(io::Error::other("no more answers")),
        };
        let _ = lock(&tx).send("answered");
        answer
    };
    (Arc::new(resolve), rx)
}

/// The address of an `rtmp://` URL the tests made.
fn addr(url: &str) -> SocketAddr {
    url.trim_start_matches("rtmp://").trim_end_matches("/live").parse().unwrap()
}

/// A destination whose server's name only the scripted resolver knows.
fn named(id: &str) -> Target {
    target(id, "rtmp://ingest.example.test/live", "k")
}

/// The next note from the scripted resolver.
fn heard(rx: &mpsc::Receiver<&'static str>) -> &'static str {
    rx.recv_timeout(GUARD).unwrap_or_else(|e| panic!("the resolver said nothing more: {e}"))
}

/// Waits until no lookup of `id` is outstanding, checking each time a lookup ends.
fn until_no_lookups(out: &Streaming, id: &str) {
    let Ok(dest) = out.hub.find(id) else { panic!("no destination `{id}`") };
    let mut changes = lock(&dest.changes);
    while dest.lookups.load(Ordering::SeqCst) > 0 {
        let (next, waited) = dest.changed.wait_timeout(changes, GUARD).unwrap_or_else(PoisonError::into_inner);
        changes = next;
        assert!(!waited.timed_out(), "a lookup is still outstanding");
    }
}

#[test]
fn a_stop_during_a_hung_lookup_returns_at_once() {
    let (release, hold) = mpsc::channel();
    let (resolve, asked) = scripted(vec![Answer::After(hold, vec![addr(&closed_url())])]);
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let seams = Seams { clock: TestClock::new(), resolve, route: routed() };
    let mut out = Streaming::start_with(&job, vec![named("a")], quick(), seams).unwrap();
    assert_eq!(heard(&asked), "asked");
    // The resolver has not answered, and the stop does not wait for it.
    out.stop_destination("a").unwrap();
    let d = until(&out, "a", |d| d.status == DestinationStatus::Stopped);
    assert_eq!(d.status, DestinationStatus::Stopped);
    let dest = out.hub.find("a").unwrap();
    // The thread sets the status and ends with its slot locked.
    let slot = lock(&dest.thread);
    assert!(!dest.running(), "the destination's thread ended");
    drop(slot);
    assert_eq!(dest.lookups.load(Ordering::SeqCst), 1, "the lookup is still outstanding");
    // Once the resolver answers, its thread ends and the answer is not used.
    drop(release);
    assert_eq!(heard(&asked), "answered");
    until_no_lookups(&out, "a");
    assert_eq!(until(&out, "a", |_| true).status, DestinationStatus::Stopped);
    out.stop().unwrap();
}

#[test]
fn a_new_setup_during_a_hung_lookup_is_used_at_once() {
    let (release, hold) = mpsc::channel();
    let (resolve, asked) = scripted(vec![Answer::After(hold, vec![addr(&closed_url())])]);
    let (c, seen_c) = server(vec![Plan::Accept]);
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    // The test's clock does not move, so the old server's lookup waits until it answers.
    let seams = Seams { clock: TestClock::new(), resolve, route: routed() };
    let mut out = Streaming::start_with(&job, vec![named("a")], quick(), seams).unwrap();
    let feed = Feed::new(&out);
    assert_eq!(heard(&asked), "asked");
    renewed(&out.hub.find("a").unwrap(), &c, "kc");
    // The try ends without the old server's answer, and the next one goes to the new server.
    assert_eq!(next(&seen_c), Seen::Publish("kc".into()));
    starts_with_headers_and_a_keyframe(&media(&seen_c, 3));
    drop(release);
    assert_eq!(heard(&asked), "answered");
    until_no_lookups(&out, "a");
    drop(feed);
    out.stop().unwrap();
    until_closed(&seen_c);
}

#[test]
fn an_ip_address_is_not_looked_up() {
    let (url, seen) = server(vec![Plan::Accept]);
    let (resolve, asked) = scripted(Vec::new());
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let seams = Seams { clock: TestClock::new(), resolve, route: routed() };
    let mut out = Streaming::start_with(&job, vec![target("a", &url, "k")], quick(), seams).unwrap();
    let feed = Feed::new(&out);
    media(&seen, 1);
    until(&out, "a", |d| d.status == DestinationStatus::Live);
    assert!(asked.try_recv().is_err(), "the resolver was not asked");
    assert_eq!(out.hub.find("a").unwrap().lookups.load(Ordering::SeqCst), 0);
    drop(feed);
    out.stop().unwrap();
    until_closed(&seen);
}

/// A clock that the test can hold still for every thread but the lookup threads, which always read
/// it at once.
struct HeldClock {
    clock: Arc<TestClock>,
    held: Mutex<()>,
}

impl Clock for HeldClock {
    fn now(&self) -> Instant {
        if std::thread::current().name() != Some("stream lookup") {
            drop(lock(&self.held));
        }
        self.clock.now()
    }
}

#[test]
fn an_answer_in_time_is_used_when_the_try_wakes_late() {
    let (live, seen) = server(vec![Plan::Accept]);
    let (release, hold) = mpsc::channel::<()>();
    let (resolve, asked) = scripted(vec![Answer::After(hold, vec![addr(&live)])]);
    let clock = TestClock::new();
    let held = Arc::new(HeldClock { clock: Arc::clone(&clock), held: Mutex::new(()) });
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let seams = Seams { clock: Arc::clone(&held) as Arc<dyn Clock>, resolve, route: routed() };
    let mut out = Streaming::start_with(&job, vec![named("a")], quick(), seams).unwrap();
    let feed = Feed::new(&out);
    assert_eq!(heard(&asked), "asked");
    // The destination's thread stops at its next clock read while the answer comes in time.
    let still = lock(&held.held);
    drop(release);
    assert_eq!(heard(&asked), "answered");
    until_no_lookups(&out, "a");
    // It reads the clock past the deadline, and still uses the answer.
    clock.advance(LOOKUP_WAIT);
    drop(still);
    assert_eq!(next(&seen), Seen::Publish("k".into()));
    let d = until(&out, "a", |d| d.status == DestinationStatus::Live);
    assert_eq!(d.reconnects, 0);
    drop(feed);
    out.stop().unwrap();
    until_closed(&seen);
}

#[test]
fn a_lookup_past_its_deadline_is_a_failed_try() {
    let (release, hold) = mpsc::channel::<()>();
    let (resolve, asked) = scripted(vec![Answer::After(hold, vec![addr(&closed_url())])]);
    let clock = TestClock::new();
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    // The wait before the next try outlasts the test, and the stop ends it.
    let timing = Timing { first: GUARD, longest: GUARD, ..quick() };
    let seams = Seams { clock: Arc::clone(&clock) as Arc<dyn Clock>, resolve, route: routed() };
    let mut out = Streaming::start_with(&job, vec![named("a")], timing, seams).unwrap();
    assert_eq!(heard(&asked), "asked");
    // Just short of the limit the try still waits.
    clock.advance(LOOKUP_WAIT - Duration::from_millis(1));
    assert_eq!(out.hub.find("a").unwrap().status(), DestinationStatus::Connecting);
    clock.advance(Duration::from_millis(1));
    let d = until(&out, "a", |d| matches!(d.status, DestinationStatus::Retrying { .. }));
    let DestinationStatus::Retrying { attempt: 2, failure } = &d.status else { panic!("{d:?}") };
    assert_eq!(failure.cause, Cause::LookupNoAnswer, "{failure:?}");
    assert!(failure.detail.contains("not looked up within 10 s"), "{failure:?}");
    out.stop().unwrap();
    drop(release);
}

#[test]
fn no_more_than_two_lookups_are_outstanding() {
    let (release_one, hold_one) = mpsc::channel::<()>();
    let (release_two, hold_two) = mpsc::channel::<()>();
    let closed = addr(&closed_url());
    let (resolve, asked) = scripted(vec![Answer::After(hold_one, vec![closed]), Answer::After(hold_two, vec![closed])]);
    let clock = TestClock::new();
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let seams = Seams { clock: Arc::clone(&clock) as Arc<dyn Clock>, resolve, route: routed() };
    let mut out = Streaming::start_with(&job, vec![named("a")], quick(), seams).unwrap();
    for _ in 0..2 {
        assert_eq!(heard(&asked), "asked");
        clock.advance(LOOKUP_WAIT);
    }
    // The third try finds two lookups outstanding, starts none, and is the last of three.
    let d = until(&out, "a", |d| matches!(d.status, DestinationStatus::Failed(_)));
    let DestinationStatus::Failed(f) = &d.status else { panic!("{d:?}") };
    assert_eq!(f.cause, Cause::LookupsPending, "{f:?}");
    assert!(f.detail.contains("an earlier lookup") && f.detail.contains("gave up after 3 tries"), "{f:?}");
    assert_eq!(out.hub.find("a").unwrap().lookups.load(Ordering::SeqCst), 2);
    assert!(asked.try_recv().is_err(), "the resolver was asked twice");
    drop((release_one, release_two));
    until_no_lookups(&out, "a");
    assert!(out.stop().is_err());
}

#[test]
fn the_lookup_count_stops_at_its_limit() {
    // The process-wide count outlives every stream; a restarted stream's destinations add to it.
    let count = AtomicU32::new(MAX_ALL_LOOKUPS - 2);
    assert!(reserve(&count, MAX_ALL_LOOKUPS));
    assert!(reserve(&count, MAX_ALL_LOOKUPS));
    assert!(!reserve(&count, MAX_ALL_LOOKUPS));
    assert_eq!(count.load(Ordering::SeqCst), MAX_ALL_LOOKUPS);
    count.fetch_sub(1, Ordering::SeqCst);
    assert!(reserve(&count, MAX_ALL_LOOKUPS));
    assert!(!reserve(&AtomicU32::new(0), 0));
}

#[test]
fn a_changed_address_is_used_on_the_next_try() {
    let (live, seen) = server(vec![Plan::Accept]);
    let (resolve, asked) = scripted(vec![Answer::Now(Ok(vec![addr(&closed_url())])), Answer::Now(Ok(vec![addr(&live)]))]);
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let seams = Seams { clock: TestClock::new(), resolve, route: routed() };
    let mut out = Streaming::start_with(&job, vec![named("a")], quick(), seams).unwrap();
    let feed = Feed::new(&out);
    // Each try looks the name up again, and the second answer is the server that takes the stream.
    assert_eq!(next(&seen), Seen::Publish("k".into()));
    let d = until(&out, "a", |d| d.status == DestinationStatus::Live);
    assert_eq!(d.reconnects, 0);
    assert_eq!([heard(&asked), heard(&asked), heard(&asked), heard(&asked)], ["asked", "answered", "asked", "answered"]);
    drop(feed);
    out.stop().unwrap();
    until_closed(&seen);
}

#[test]
fn a_late_answer_from_an_abandoned_lookup_is_discarded() {
    let (live, seen) = server(vec![Plan::Accept]);
    let (release, hold) = mpsc::channel::<()>();
    let (resolve, asked) = scripted(vec![Answer::After(hold, vec![addr(&closed_url())]), Answer::Now(Ok(vec![addr(&live)]))]);
    let clock = TestClock::new();
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let seams = Seams { clock: Arc::clone(&clock) as Arc<dyn Clock>, resolve, route: routed() };
    let mut out = Streaming::start_with(&job, vec![named("a")], quick(), seams).unwrap();
    let feed = Feed::new(&out);
    assert_eq!(heard(&asked), "asked");
    clock.advance(LOOKUP_WAIT);
    // The next try's lookup answers at once and the destination goes live.
    assert_eq!(next(&seen), Seen::Publish("k".into()));
    until(&out, "a", |d| d.status == DestinationStatus::Live);
    // The first lookup answers late with an address nothing listens on, and the stream goes on.
    drop(release);
    until_no_lookups(&out, "a");
    media(&seen, 10);
    let d = until(&out, "a", |_| true);
    assert_eq!((d.status, d.reconnects), (DestinationStatus::Live, 0));
    drop(feed);
    out.stop().unwrap();
    until_closed(&seen);
}

/// A route check whose answer the test sets through the returned handle.
fn switch(route: Route) -> (Arc<RouteCheck>, Arc<Mutex<Route>>) {
    let now = Arc::new(Mutex::new(route));
    let read = Arc::clone(&now);
    (Arc::new(move |_: &[SocketAddr]| *lock(&read)), now)
}

/// A route check that always finds a route.
fn routed() -> Arc<RouteCheck> {
    switch(Route::Selected).0
}

/// Times for the network tests: the wait before a try again outlasts the test, and the route is
/// checked every millisecond.
fn network_timing() -> Timing {
    Timing { first: GUARD, longest: GUARD, net_poll: Duration::from_millis(1), ..quick() }
}

fn waiting(d: &DestinationState) -> Option<u32> {
    match &d.status {
        DestinationStatus::WaitingForNetwork { attempt, .. } => Some(*attempt),
        _ => None,
    }
}

#[test]
fn offline_at_start_with_a_failing_lookup_waits_and_recovers() {
    let (url, seen) = server(vec![Plan::Accept]);
    let (resolve, asked) = scripted(vec![Answer::Now(Err(io::Error::other("no network"))), Answer::Now(Ok(vec![addr(&url)]))]);
    let (route, set) = switch(Route::None);
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let seams = Seams { clock: TestClock::new(), resolve, route };
    let mut out = Streaming::start_with(&job, vec![named("a")], network_timing(), seams).unwrap();
    let feed = Feed::new(&out);
    assert_eq!([heard(&asked), heard(&asked)], ["asked", "answered"]);
    // The lookup failed and there is no route: the destination waits without trying.
    let d = until(&out, "a", |d| waiting(d).is_some());
    let DestinationStatus::WaitingForNetwork { attempt: 2, failure } = &d.status else { panic!("{d:?}") };
    assert_eq!(failure.cause, Cause::LookupFailed, "{failure:?}");
    assert_eq!(out.destination_counts(), DestinationCounts { live: 0, retrying: 1 }, "a wait for the network counts as reconnecting");
    assert!(asked.try_recv().is_err(), "no try while there is no route");
    // The route returns: the destination tries at once, without its 60 s wait, and goes live.
    *lock(&set) = Route::Selected;
    assert_eq!(heard(&asked), "asked");
    assert_eq!(next(&seen), Seen::Publish("k".into()));
    until(&out, "a", |d| d.status == DestinationStatus::Live);
    media(&seen, 1);
    assert_eq!(out.hub.net.threads.load(Ordering::SeqCst), 1);
    drop(feed);
    out.stop().unwrap();
    until_closed(&seen);
}

#[test]
fn a_lost_connection_without_a_route_waits_for_the_network() {
    let (url, seen) = server(vec![Plan::DropAfter(5), Plan::Accept]);
    let (route, set) = switch(Route::Selected);
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let seams = Seams { clock: TestClock::new(), resolve: Arc::new(system_resolve), route };
    let mut out = Streaming::start_with(&job, vec![target("a", &url, "k")], network_timing(), seams).unwrap();
    assert_eq!(next(&seen), Seen::Publish("k".into()));
    // The network goes before the server drops the connection, which happens once media flows.
    *lock(&set) = Route::None;
    let feed = Feed::new(&out);
    let d = until(&out, "a", |d| waiting(d).is_some());
    let DestinationStatus::WaitingForNetwork { attempt: 2, failure } = &d.status else { panic!("{d:?}") };
    assert_eq!(failure.cause, Cause::Lost, "{failure:?}");
    *lock(&set) = Route::Selected;
    let d = until(&out, "a", |d| d.status == DestinationStatus::Live && d.reconnects == 1);
    assert_eq!(d.reconnects, 1);
    drop(feed);
    out.stop().unwrap();
    until_closed(&seen);
}

#[test]
fn offline_time_counts_against_the_tries() {
    let (route, _set) = switch(Route::None);
    let clock = TestClock::new();
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let timing = Timing { first: Duration::from_secs(10), longest: Duration::from_secs(10), ..network_timing() };
    let seams = Seams { clock: Arc::clone(&clock) as Arc<dyn Clock>, resolve: Arc::new(system_resolve), route };
    // The first try learns the server's address; with no route to it, no later try starts.
    let mut out = Streaming::start_with(&job, vec![target("a", &closed_url(), "k")], timing, seams).unwrap();
    until(&out, "a", |d| waiting(d) == Some(2));
    // Just short of the longest wait nothing changes; each longest wait is one more failed try.
    clock.advance(Duration::from_secs(10) - Duration::from_millis(1));
    assert_eq!(waiting(&until(&out, "a", |_| true)), Some(2));
    clock.advance(Duration::from_millis(1));
    until(&out, "a", |d| waiting(d) == Some(3));
    clock.advance(Duration::from_secs(10));
    let d = until(&out, "a", |d| matches!(d.status, DestinationStatus::Failed(_)));
    let DestinationStatus::Failed(f) = &d.status else { panic!("{d:?}") };
    assert!(f.detail.contains("gave up after 3 tries"), "{f:?}");
    assert!(out.stop().is_err());
}

#[test]
fn a_server_reached_without_a_default_route_is_still_looked_up() {
    let (url, seen) = server(vec![Plan::Accept]);
    let (resolve, asked) = scripted(vec![Answer::Now(Err(io::Error::other("no answer"))), Answer::Now(Ok(vec![addr(&url)]))]);
    // Only the server's own address has a route, as on a network without a default route.
    let route: Arc<RouteCheck> = Arc::new(|a: &[SocketAddr]| if a.is_empty() { Route::None } else { Route::Selected });
    let clock = TestClock::new();
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let timing = Timing { first: Duration::from_secs(10), longest: Duration::from_secs(10), ..network_timing() };
    let seams = Seams { clock: Arc::clone(&clock) as Arc<dyn Clock>, resolve, route };
    let mut out = Streaming::start_with(&job, vec![named("a")], timing, seams).unwrap();
    let feed = Feed::new(&out);
    assert_eq!([heard(&asked), heard(&asked)], ["asked", "answered"]);
    until(&out, "a", |d| waiting(d) == Some(2));
    // At the usual time the name is looked up again, and the server takes the stream.
    clock.advance(Duration::from_secs(10));
    assert_eq!(heard(&asked), "asked");
    assert_eq!(next(&seen), Seen::Publish("k".into()));
    let d = until(&out, "a", |d| d.status == DestinationStatus::Live);
    assert_eq!(d.reconnects, 0);
    drop(feed);
    out.stop().unwrap();
    until_closed(&seen);
}

#[test]
fn a_named_server_whose_address_lost_its_route_is_looked_up_again() {
    let (live, seen) = server(vec![Plan::Accept]);
    let stale = addr(&closed_url());
    let (resolve, asked) = scripted(vec![Answer::Now(Ok(vec![stale])), Answer::Now(Ok(vec![addr(&live)]))]);
    // The first answer's address has no route; the system has one elsewhere.
    let route: Arc<RouteCheck> = Arc::new(move |a: &[SocketAddr]| if a.contains(&stale) { Route::None } else { Route::Selected });
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let seams = Seams { clock: TestClock::new(), resolve, route };
    let mut out = Streaming::start_with(&job, vec![named("a")], quick(), seams).unwrap();
    let feed = Feed::new(&out);
    // The next try looks the name up again and reaches the server at its new address.
    assert_eq!(next(&seen), Seen::Publish("k".into()));
    until(&out, "a", |d| d.status == DestinationStatus::Live);
    assert_eq!([heard(&asked), heard(&asked), heard(&asked), heard(&asked)], ["asked", "answered", "asked", "answered"]);
    assert_eq!(out.hub.net.threads.load(Ordering::SeqCst), 0, "it never waited for the network");
    drop(feed);
    out.stop().unwrap();
    until_closed(&seen);
}

#[test]
fn a_stop_while_waiting_for_the_network_returns() {
    let (resolve, _asked) = scripted(Vec::new());
    let (route, _set) = switch(Route::None);
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let seams = Seams { clock: TestClock::new(), resolve, route };
    let mut out = Streaming::start_with(&job, vec![named("a")], network_timing(), seams).unwrap();
    until(&out, "a", |d| waiting(d).is_some());
    out.stop_destination("a").unwrap();
    until(&out, "a", |d| d.status == DestinationStatus::Stopped);
    let dest = out.hub.find("a").unwrap();
    // The thread sets the status and ends with its slot locked.
    let slot = lock(&dest.thread);
    assert!(!dest.running(), "the destination's thread ended");
    drop(slot);
    assert!(lock(&out.hub.net.state).0.is_empty(), "it no longer waits for the network");
    out.stop().unwrap();
}

/// A stop and start that reaches the waiting thread of `id`, as `spawn` makes it.
fn restart_waiting(out: &Streaming, id: &str) {
    let dest = out.hub.find(id).unwrap();
    dest.restarted.store(true, Ordering::SeqCst);
    if let Some(t) = lock(&dest.thread).as_ref() {
        t.thread().unpark();
    }
}

#[test]
fn a_restart_while_it_waits_shows_a_first_try() {
    let (url, seen) = server(vec![Plan::Accept]);
    let (release, hold) = mpsc::channel::<()>();
    let (resolve, asked) = scripted(vec![Answer::Now(Err(io::Error::other("no answer"))), Answer::After(hold, vec![addr(&url)])]);
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let seams = Seams { clock: TestClock::new(), resolve, route: routed() };
    let mut out = Streaming::start_with(&job, vec![named("a")], network_timing(), seams).unwrap();
    let feed = Feed::new(&out);
    assert_eq!([heard(&asked), heard(&asked)], ["asked", "answered"]);
    until(&out, "a", |d| matches!(d.status, DestinationStatus::Retrying { attempt: 2, .. }));
    restart_waiting(&out, "a");
    // While the try looks the name up, it is shown as a first try.
    assert_eq!(heard(&asked), "asked");
    until(&out, "a", |d| d.status == DestinationStatus::Connecting);
    drop(release);
    assert_eq!(next(&seen), Seen::Publish("k".into()));
    until(&out, "a", |d| d.status == DestinationStatus::Live);
    drop(feed);
    out.stop().unwrap();
    until_closed(&seen);
}

#[test]
fn a_restart_while_waiting_for_the_network_tries_at_once() {
    let (url, seen) = server(vec![Plan::Accept]);
    let (release, hold) = mpsc::channel::<()>();
    let (resolve, asked) = scripted(vec![Answer::Now(Err(io::Error::other("no network"))), Answer::After(hold, vec![addr(&url)])]);
    let (route, _set) = switch(Route::None);
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let seams = Seams { clock: TestClock::new(), resolve, route };
    let mut out = Streaming::start_with(&job, vec![named("a")], network_timing(), seams).unwrap();
    let feed = Feed::new(&out);
    assert_eq!([heard(&asked), heard(&asked)], ["asked", "answered"]);
    until(&out, "a", |d| waiting(d).is_some());
    restart_waiting(&out, "a");
    // It tries at once though the route has not returned, shown as a first try, and the server
    // takes the stream.
    assert_eq!(heard(&asked), "asked");
    until(&out, "a", |d| d.status == DestinationStatus::Connecting);
    drop(release);
    assert_eq!(next(&seen), Seen::Publish("k".into()));
    let d = until(&out, "a", |d| d.status == DestinationStatus::Live);
    assert_eq!(d.reconnects, 0);
    assert!(lock(&out.hub.net.state).0.is_empty(), "it no longer waits for the network");
    drop(feed);
    out.stop().unwrap();
    until_closed(&seen);
}

#[test]
fn try_now_ends_the_wait_before_a_try_again() {
    let (a, seen) = server(vec![Plan::DropAfter(12), Plan::Accept]);
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    // The wait before a try again outlasts the test.
    let long = GUARD * 10;
    let timing = Timing { first: long, longest: long, max_tries: 5, ..quick() };
    let mut out = Streaming::start_timed(&job, vec![target("a", &a, "k")], timing).unwrap();
    assert_eq!(next(&seen), Seen::Publish("k".into()));
    let d = until(&out, "a", |d| d.status == DestinationStatus::Live);
    assert_eq!((d.next_try, d.max_tries), (None, 5), "{d:?}");
    assert!(!out.retry_destination("a").unwrap(), "a live destination does not wait to try again");
    let feed = Feed::new(&out);
    media(&seen, 12);
    until_closed(&seen);
    // The lost connection lasted less than 30 s, so the next try is the second.
    let d = until(&out, "a", |d| d.next_try.is_some());
    assert!(matches!(d.status, DestinationStatus::Retrying { attempt: 2, .. }), "{d:?}");
    let left = d.next_try.unwrap_or_default();
    assert!(left > long - GUARD && left <= long, "{left:?}");
    assert!(out.retry_destination("a").unwrap());
    assert_eq!(next(&seen), Seen::Publish("k".into()));
    let d = until(&out, "a", |d| d.status == DestinationStatus::Live && d.reconnects == 1);
    assert_eq!(d.next_try, None, "{d:?}");
    assert!(out.retry_destination("nope").is_err(), "an unknown destination is an error");
    drop(feed);
    out.stop().unwrap();
    until_closed(&seen);
    assert!(out.retry_destination("a").is_err(), "a stream that ended has nothing to try");
}

#[test]
fn an_encoder_end_after_a_stop_shows_the_destination_stopped() {
    let (a, _seen) = server(vec![Plan::Accept]);
    let mut out = Streaming::start_timed(&job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 }), vec![target("a", &a, "k")], quick()).unwrap();
    let d = Dest::new(named("b"), Arc::default(), None, Arc::clone(&out.hub.main));
    let ended = || (Arc::clone(&out.hub.main), Failure::new(Cause::Encoder, "its video encoder stopped"));
    // Asked to stop while its thread read the encoder's end: the stop wins.
    d.stop.store(true, Ordering::SeqCst);
    assert!(d.fail_ended(&out.hub, ended()));
    assert_eq!(d.status(), DestinationStatus::Stopped);
    assert!(!d.running());
    // Neither stopped nor started again: the encoder's end is its failure.
    d.stop.store(false, Ordering::SeqCst);
    assert!(d.fail_ended(&out.hub, ended()));
    assert!(matches!(d.status(), DestinationStatus::Failed(f) if f.detail.contains("video encoder")), "{:?}", d.status());
    let _ = out.stop();
}

#[test]
fn the_next_try_and_try_now_are_one_atomic_word() {
    // A running stream gives the destination its encoder.
    let (a, _seen) = server(vec![Plan::Accept]);
    let mut out = Streaming::start_timed(&job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 }), vec![target("a", &a, "k")], quick()).unwrap();
    let d = Dest::new(named("b"), Arc::default(), None, Arc::clone(&out.hub.main));
    // The window reads it without waiting on the destination's thread (AGENTS.md › Performance).
    let _: &AtomicU64 = &d.next;
    assert!(!d.try_now(), "a destination that does not wait has nothing to end");
    let at = Instant::now() + Duration::from_secs(3);
    d.wait_until(NextTry::At(at));
    assert_eq!(d.next_try(), NextTry::At(at));
    assert!(d.try_now());
    // A new deadline after the wait for the network keeps the request, so that wait ends at once too.
    let later = at + Duration::from_secs(1);
    d.wait_until(NextTry::At(later));
    assert!(d.trying_now());
    assert_eq!(d.next_try(), NextTry::At(later));
    // A non-waiting status takes the next try away and keeps the request until the try starts.
    d.set(DestinationStatus::Connecting);
    assert_eq!(d.next_try(), NextTry::None);
    assert!(d.trying_now());
    d.begin_try();
    assert!(!d.trying_now());
    // A time of the stream's clock from before the destination was made reads back the same.
    if let Some(early) = Instant::now().checked_sub(Duration::from_secs(5)) {
        d.wait_until(NextTry::Clocked(early));
        assert_eq!(d.next_try(), NextTry::Clocked(early));
    }
    d.wait_until(NextTry::Route);
    assert_eq!(d.next_try(), NextTry::Route);
    d.begin_try();
    assert_eq!(d.next_try(), NextTry::None);
    assert!(!d.try_now());
    out.stop().unwrap();
}

#[test]
fn a_try_started_by_try_now_counts_toward_the_tries() {
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let long = GUARD * 10;
    let timing = Timing { first: long, longest: long, max_tries: 2, ..quick() };
    let mut out = Streaming::start_timed(&job, vec![target("a", &closed_url(), "k")], timing).unwrap();
    until(&out, "a", |d| matches!(d.status, DestinationStatus::Retrying { attempt: 2, .. }) && d.next_try.is_some());
    // The second refused connection is the last of the two tries, as it would be at the usual time.
    assert!(out.retry_destination("a").unwrap());
    let d = until(&out, "a", |d| matches!(d.status, DestinationStatus::Failed(_)));
    let DestinationStatus::Failed(f) = &d.status else { panic!("{d:?}") };
    assert!(f.cause == Cause::Refused && f.detail.contains("gave up after 2 tries"), "{f:?}");
    assert_eq!(d.next_try, None);
    assert!(!out.retry_destination("a").unwrap(), "a destination that gave up does not wait");
    assert!(out.stop().is_err());
}

#[test]
fn try_now_during_a_try_returns_at_once_and_changes_nothing() {
    let (url, seen) = server(vec![Plan::Accept]);
    let (release, hold) = mpsc::channel::<()>();
    let (resolve, asked) = scripted(vec![Answer::Now(Err(io::Error::other("no answer"))), Answer::After(hold, vec![addr(&url)])]);
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let seams = Seams { clock: TestClock::new(), resolve, route: routed() };
    let mut out = Streaming::start_with(&job, vec![named("a")], network_timing(), seams).unwrap();
    let feed = Feed::new(&out);
    assert_eq!([heard(&asked), heard(&asked)], ["asked", "answered"]);
    until(&out, "a", |d| matches!(d.status, DestinationStatus::Retrying { attempt: 2, .. }) && d.next_try.is_some());
    assert!(out.retry_destination("a").unwrap());
    // The try's lookup does not answer yet: the try runs, with no next try, and a second Try Now
    // returns without waiting for it.
    assert_eq!(heard(&asked), "asked");
    let d = until(&out, "a", |d| d.next_try.is_none());
    assert!(matches!(d.status, DestinationStatus::Retrying { attempt: 2, .. }), "{d:?}");
    assert!(!out.retry_destination("a").unwrap());
    drop(release);
    assert_eq!(next(&seen), Seen::Publish("k".into()));
    until(&out, "a", |d| d.status == DestinationStatus::Live);
    drop(feed);
    out.stop().unwrap();
    until_closed(&seen);
}

#[test]
fn try_now_while_waiting_for_the_network_tries_without_a_route() {
    // A server given as an IP address waits for a route with no time set.
    let (url, seen) = server(vec![Plan::DropAfter(5), Plan::Accept]);
    let (route, set) = switch(Route::Selected);
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let seams = Seams { clock: TestClock::new(), resolve: Arc::new(system_resolve), route };
    let mut out = Streaming::start_with(&job, vec![target("a", &url, "k")], network_timing(), seams).unwrap();
    assert_eq!(next(&seen), Seen::Publish("k".into()));
    *lock(&set) = Route::None;
    let feed = Feed::new(&out);
    let d = until(&out, "a", |d| waiting(d).is_some());
    assert_eq!((d.next_try, d.max_tries), (None, 3), "it tries when a route returns: {d:?}");
    until_closed(&seen);
    assert!(out.retry_destination("a").unwrap());
    assert_eq!(next(&seen), Seen::Publish("k".into()));
    let d = until(&out, "a", |d| d.status == DestinationStatus::Live && d.reconnects == 1);
    assert_eq!(d.next_try, None);
    assert!(lock(&out.hub.net.state).0.is_empty(), "it no longer waits for the network");
    drop(feed);
    out.stop().unwrap();
    until_closed(&seen);

    // A named server with no default route waits the usual time, on the stream's clock.
    let (url, seen) = server(vec![Plan::Accept]);
    let (resolve, asked) = scripted(vec![Answer::Now(Err(io::Error::other("no network"))), Answer::Now(Ok(vec![addr(&url)]))]);
    let (route, _set) = switch(Route::None);
    let clock = TestClock::new();
    let seams = Seams { clock: Arc::clone(&clock) as Arc<dyn Clock>, resolve, route };
    let mut out = Streaming::start_with(&job, vec![named("a")], network_timing(), seams).unwrap();
    let feed = Feed::new(&out);
    assert_eq!([heard(&asked), heard(&asked)], ["asked", "answered"]);
    let d = until(&out, "a", |d| waiting(d).is_some() && d.next_try.is_some());
    assert_eq!(d.next_try, Some(GUARD), "{d:?}");
    clock.advance(Duration::from_secs(1));
    assert_eq!(until(&out, "a", |_| true).next_try, Some(GUARD - Duration::from_secs(1)));
    assert!(out.retry_destination("a").unwrap());
    assert_eq!(heard(&asked), "asked");
    assert_eq!(next(&seen), Seen::Publish("k".into()));
    until(&out, "a", |d| d.status == DestinationStatus::Live);
    drop(feed);
    out.stop().unwrap();
    until_closed(&seen);
}

#[test]
fn a_route_that_returns_soon_after_a_refusal_keeps_the_refusal_wait() {
    let timing = Timing { refused: Duration::from_secs(5), ..quick() };
    let at = Instant::now();
    // A route back 2 s after the refusal leaves 3 s of its wait; one back after 6 s leaves none.
    assert_eq!(refusal_left(&timing, Some(at), at + Duration::from_secs(2)), Duration::from_secs(3));
    assert_eq!(refusal_left(&timing, Some(at), at + Duration::from_secs(6)), Duration::ZERO);
    // A wait for the network after any other failure ends at once when the route returns.
    assert_eq!(refusal_left(&timing, None, at + Duration::from_secs(2)), Duration::ZERO);
}

#[test]
fn sixteen_destinations_share_one_route_thread() {
    let (resolve, _asked) = scripted(Vec::new());
    let (route, _set) = switch(Route::None);
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let seams = Seams { clock: TestClock::new(), resolve, route };
    let ids: Vec<String> = (0..16).map(|i| format!("d{i}")).collect();
    let mut out = Streaming::start_with(&job, ids.iter().map(|id| named(id)).collect(), network_timing(), seams).unwrap();
    for id in &ids {
        until(&out, id, |d| waiting(d).is_some());
    }
    assert_eq!(lock(&out.hub.net.state).0.len(), 16);
    assert_eq!(out.hub.net.threads.load(Ordering::SeqCst), 1);
    out.stop().unwrap();
}

#[test]
fn a_selected_or_unknown_route_never_blocks_a_try() {
    for route in [Route::Selected, Route::Unknown] {
        let (url, seen) = server(vec![Plan::Accept]);
        let (resolve, _asked) = scripted(vec![Answer::Now(Err(io::Error::other("no answer"))), Answer::Now(Ok(vec![addr(&url)]))]);
        let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
        let seams = Seams { clock: TestClock::new(), resolve, route: switch(route).0 };
        let mut out = Streaming::start_with(&job, vec![named("a")], quick(), seams).unwrap();
        let feed = Feed::new(&out);
        until(&out, "a", |d| d.status == DestinationStatus::Live);
        assert_eq!(out.hub.net.threads.load(Ordering::SeqCst), 0, "{route:?}: it never waited for the network");
        drop(feed);
        out.stop().unwrap();
        until_closed(&seen);
    }
}

#[test]
fn flapping_routes_do_not_reset_the_count() {
    let (release, hold) = mpsc::channel::<()>();
    let (resolve, asked) = scripted(vec![Answer::Now(Err(io::Error::other("no network"))), Answer::After(hold, vec![addr(&closed_url())])]);
    let (route, set) = switch(Route::None);
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let timing = Timing { max_tries: 10, ..network_timing() };
    let seams = Seams { clock: TestClock::new(), resolve, route };
    let mut out = Streaming::start_with(&job, vec![named("a")], timing, seams).unwrap();
    until(&out, "a", |d| waiting(d) == Some(2));
    // The route comes back long enough for one try, then goes again before that try fails.
    *lock(&set) = Route::Selected;
    assert_eq!([heard(&asked), heard(&asked), heard(&asked)], ["asked", "answered", "asked"]);
    *lock(&set) = Route::None;
    drop(release);
    // The count goes on from the try that failed: 2 failed, so the next is 3.
    until(&out, "a", |d| waiting(d) == Some(3));
    out.stop().unwrap();
}

/// Checks on probes that find the server and every route, and record each call.
fn auto_checks(service_hosts: bool) -> (Arc<diagnose::Checks>, Arc<diagnose::tests::Fake>, AutoCheck) {
    auto_checks_on(diagnose::tests::Fake::new(), service_hosts)
}

/// Checks on `fake`, for destination `a` of the profile `Show`, set up for a service.
fn auto_checks_on(fake: diagnose::tests::Fake, service_hosts: bool) -> (Arc<diagnose::Checks>, Arc<diagnose::tests::Fake>, AutoCheck) {
    let (checks, fake) = diagnose::tests::checks_with(fake, diagnose::tests::TestClock::new());
    let Some(twitch) = studiocraft_model::services::service("twitch") else { panic!("no twitch preset") };
    (checks, fake, AutoCheck { profile: "Show".into(), services: vec![("a".into(), twitch)], service_hosts })
}

/// How many checks ran on `fake`: each asks for the route to any address once.
fn checks_run(fake: &diagnose::tests::Fake) -> usize {
    fake.calls().iter().filter(|c| *c == "route any").count()
}

/// Seams whose lookups answer with a closed port on this computer, and the server's address. The
/// `nth` lookup (from 1) sends on the returned receiver's channel and waits until the returned
/// sender is dropped, so the test looks while `nth - 1` tries have failed.
fn held_lookup(nth: u32) -> (Seams, String, mpsc::Receiver<()>, mpsc::Sender<()>) {
    let closed = closed_url();
    let port: u16 = closed.rsplit(':').next().and_then(|p| p.split('/').next()).and_then(|p| p.parse().ok()).unwrap();
    let (reached, at_nth) = mpsc::channel::<()>();
    let (go_on, hold) = mpsc::channel::<()>();
    let (reached, hold) = (Mutex::new(reached), Mutex::new(hold));
    let lookups = AtomicU32::new(0);
    let resolve = move |_: &str, _: u16| {
        if lookups.fetch_add(1, Ordering::SeqCst) + 1 == nth {
            let _ = lock(&reached).send(());
            let _ = lock(&hold).recv();
        }
        Ok(vec![SocketAddr::from(([127, 0, 0, 1], port))])
    };
    (Seams { resolve: Arc::new(resolve), ..Seams::default() }, format!("rtmp://held.example:{port}/live"), at_nth, go_on)
}

#[test]
fn a_destination_is_checked_once_after_its_third_failed_try() {
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let (checks, fake, auto) = auto_checks(false);
    let timing = Timing { max_tries: 6, ..quick() };
    // Two failed tries start no check.
    let (seams, server, at_third, go_on) = held_lookup(3);
    let mut out = Streaming::start_full(&job, vec![target("a", &server, "k")], timing, seams, Some((Arc::clone(&checks), auto))).unwrap();
    at_third.recv_timeout(GUARD).unwrap();
    assert_eq!(checks.state("Show", "a"), None, "{:?}", fake.calls());
    drop(go_on);
    let d = until(&out, "a", |d| matches!(d.status, DestinationStatus::Failed(_)));
    let report = diagnose::tests::done_in(&checks, "Show", "a");
    assert!(report.automatic, "{report:?}");
    assert_eq!(report.verdict, diagnose::Verdict::Reachable, "the probes reach the server, and the refusal came before a connection");
    let DestinationStatus::Failed(f) = &d.status else { panic!("{d:?}") };
    assert_eq!(f.cause, Cause::Refused, "{f:?}");
    diagnose::tests::until_checks(&checks, |c| (c.running() == 0).then_some(()));
    assert_eq!(checks_run(&fake), 1, "three more failures and giving up start no other check");
    // The service's other hosts are off by default.
    assert!(fake.calls().iter().all(|c| !c.starts_with("fetch")), "{:?}", fake.calls());
    assert!(out.stop().is_err());
}

#[test]
fn a_destination_that_gives_up_is_checked_and_its_check_outlives_the_stream() {
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    // The check's connections wait until the stream has stopped.
    let (release, hold) = mpsc::channel::<()>();
    let hold = Mutex::new(hold);
    let mut fake = diagnose::tests::Fake::new();
    fake.reach = Box::new(move |_, _, _| {
        let _ = lock(&hold).recv();
        Ok(())
    });
    let (checks, fake, auto) = auto_checks_on(fake, true);
    let timing = Timing { reconnect: false, ..quick() };
    let mut out =
        Streaming::start_full(&job, vec![target("a", &closed_url(), "k")], timing, Seams::default(), Some((Arc::clone(&checks), auto))).unwrap();
    until(&out, "a", |d| matches!(d.status, DestinationStatus::Failed(_)));
    assert_eq!(checks.state("Show", "a"), Some(diagnose::CheckState::Running { automatic: true }));
    assert!(out.stop().is_err());
    drop(release);
    let report = diagnose::tests::done_in(&checks, "Show", "a");
    let name = studiocraft_model::services::service("twitch").map(|s| s.name);
    assert_eq!((report.automatic, report.service.as_deref()), (true, name), "{report:?}");
    assert!(fake.calls().iter().any(|c| c.starts_with("fetch")), "the profile turned the service's hosts on");
    diagnose::tests::until_checks(&checks, |c| (c.running() == 0).then_some(()));
    assert_eq!(checks_run(&fake), 1);
}

#[test]
fn a_check_after_a_failure_checks_the_server_of_the_try_that_failed() {
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let (checks, _fake, auto) = auto_checks(false);
    // The stream's own destination waits in its lookup, so it starts no check.
    let (release, hold) = mpsc::channel::<()>();
    let (resolve, _asked) = scripted(vec![Answer::After(hold, Vec::new())]);
    let seams = Seams { clock: TestClock::new(), resolve, route: routed() };
    let mut out = Streaming::start_full(&job, vec![named("x")], quick(), seams, Some((Arc::clone(&checks), auto))).unwrap();
    let d = Dest::new(target("a", "rtmp://127.0.0.1:1111/live", "k"), Arc::new(Counts::default()), None, Arc::clone(&out.hub.main));
    let tried = d.setup();
    assert!(set_up(&d, Given { key: Some("k2"), server: Some(("rtmp://127.0.0.1:2222/live", None)), ..Given::default() }));
    let f = Failure::new(Cause::Refused, "refused");
    // Started again since the try: its failure is not checked.
    d.restarted.store(true, Ordering::SeqCst);
    check_after(&out.hub, &d, &f, &tried);
    assert_eq!(checks.state("Show", "a"), None);
    // Not started again: the check is of the server the try used, not the one given since.
    d.restarted.store(false, Ordering::SeqCst);
    check_after(&out.hub, &d, &f, &tried);
    let report = diagnose::tests::done_in(&checks, "Show", "a");
    assert_eq!(report.port, 1111, "{report:?}");
    drop(release);
    let _ = out.stop();
}

#[test]
fn a_stopped_destination_that_gives_its_place_keeps_no_check() {
    // The check's lookup waits until the test lets it answer, so the check is still running.
    let (release, hold) = mpsc::channel::<()>();
    let (looking, looked) = mpsc::channel::<()>();
    let (hold, looking) = (Mutex::new(hold), Mutex::new(looking));
    let lookup = move |_: &str, _: u16| {
        let _ = lock(&looking).send(());
        let _ = lock(&hold).recv();
        Ok(Vec::new())
    };
    let fake = diagnose::tests::Fake { lookup: Box::new(lookup), ..diagnose::tests::Fake::new() };
    let (checks, _fake, auto) = auto_checks_on(fake, false);
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    // The stream's own destination waits in its lookup, so it starts no check.
    let (release_x, hold_x) = mpsc::channel::<()>();
    let (resolve, _asked) = scripted(vec![Answer::After(hold_x, Vec::new())]);
    let seams = Seams { clock: TestClock::new(), resolve, route: routed() };
    let mut out = Streaming::start_full(&job, vec![named("x")], quick(), seams, Some((Arc::clone(&checks), auto))).unwrap();
    for i in 1..MAX_SUBSCRIBERS {
        out.hub.find_or_add(&format!("d{i}"), None, &out.hub.main).unwrap();
    }
    // d1 has not started and shows as stopped; its check runs.
    let d1 = out.hub.find("d1").unwrap();
    assert!(set_up(&d1, Given { key: Some("k"), server: Some(("rtmp://check.example.test/live", None)), ..Given::default() }));
    check_after(&out.hub, &d1, &Failure::new(Cause::Refused, "refused"), &d1.setup());
    looked.recv().unwrap();
    assert!(matches!(checks.state("Show", "d1"), Some(diagnose::CheckState::Running { .. })));
    // An added destination takes d1's place, and d1's check ends with it.
    out.hub.find_or_add("new", None, &out.hub.main).unwrap();
    assert!(out.hub.find("d1").is_err());
    assert_eq!(checks.state("Show", "d1"), None);
    drop(release);
    drop(release_x);
    let _ = out.stop();
}

#[test]
fn a_restart_after_a_check_is_checked_again_after_its_third_failed_try() {
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let (checks, fake, auto) = auto_checks(false);
    let timing = Timing { max_tries: 6, ..quick() };
    // Three failed tries start a check, and the fourth try waits in its lookup.
    let (seams, server, at_fourth, go_on) = held_lookup(4);
    let mut out = Streaming::start_full(&job, vec![target("a", &server, "k")], timing, seams, Some((Arc::clone(&checks), auto))).unwrap();
    at_fourth.recv_timeout(GUARD).unwrap();
    assert!(diagnose::tests::done_in(&checks, "Show", "a").automatic);
    // Started again while it tries: the fourth try is its first, and the sixth its third.
    restart_waiting(&out, "a");
    drop(go_on);
    until(&out, "a", |d| matches!(d.status, DestinationStatus::Failed(_)));
    diagnose::tests::until_checks(&checks, |c| (c.running() == 0).then_some(()));
    assert_eq!(checks_run(&fake), 2, "{:?}", fake.calls());
    assert!(out.stop().is_err());
}

#[test]
fn a_check_that_finds_every_place_taken_waits_for_one() {
    // Four checks of another server hold every place until released.
    checked_after_held(&["w", "x", "y", "z"]);
}

#[test]
fn a_check_that_finds_its_destination_being_checked_is_started_at_a_later_failure() {
    // Check Connection of the destination itself runs until released.
    checked_after_held(&["a"]);
}

/// Holds checks of `held` (destinations of the profile `Show`) until the destination `a` has
/// failed three times, and shows that `a` is then checked by itself: when a place is free, or at
/// a later failure after its own check ends.
fn checked_after_held(held: &[&str]) {
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let (release, hold) = mpsc::channel::<()>();
    let hold = Mutex::new(hold);
    let mut fake = diagnose::tests::Fake::new();
    fake.reach = Box::new(move |host, _, _| {
        if host == "203.0.113.9" || host == "held.example" {
            let _ = lock(&hold).recv();
        }
        Ok(())
    });
    let (checks, fake, auto) = auto_checks_on(fake, false);
    // The third failure starts no check of its own.
    let (seams, server, at_fourth, go_on) = held_lookup(4);
    for id in held {
        // A check of `a` at its own server and service; the others are of another server.
        let own = *id == "a";
        let s = diagnose::Subject {
            profile: "Show".into(),
            id: (*id).into(),
            server: if own { server.clone() } else { "rtmps://203.0.113.9/app".into() },
            service: if own { studiocraft_model::services::service("twitch") } else { None },
            last: None,
            service_hosts: false,
            automatic: false,
            checked: None,
            wanted: None,
        };
        checks.start(s).unwrap();
    }
    let timing = Timing { max_tries: 20, ..quick() };
    let mut out = Streaming::start_full(&job, vec![target("a", &server, "k")], timing, seams, Some((Arc::clone(&checks), auto))).unwrap();
    at_fourth.recv_timeout(GUARD).unwrap();
    let state = checks.state("Show", "a");
    assert!(matches!(state, None | Some(diagnose::CheckState::Running { automatic: false })), "{state:?}");
    drop(release);
    drop(go_on);
    diagnose::tests::until_checks(&checks, |c| match c.state("Show", "a") {
        Some(diagnose::CheckState::Done(d)) if d.automatic => Some(()),
        _ => None,
    });
    diagnose::tests::until_checks(&checks, |c| (c.running() == 0).then_some(()));
    assert_eq!(checks_run(&fake), held.len() + 1, "the held checks and the destination's");
    let _ = out.stop();
}

#[test]
fn a_stream_without_failures_checks_nothing_and_a_refused_stream_is_checked_at_once() {
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let (checks, fake, auto) = auto_checks(false);
    let (url, seen) = server(vec![Plan::Accept]);
    let mut out =
        Streaming::start_full(&job, vec![target("a", &url, "k")], quick(), Seams::default(), Some((Arc::clone(&checks), auto.clone()))).unwrap();
    let feed = Feed::new(&out);
    until(&out, "a", |d| d.status == DestinationStatus::Live);
    drop(feed);
    out.stop().unwrap();
    until_closed(&seen);
    assert_eq!((checks.state("Show", "a"), checks_run(&fake)), (None, 0));
    // A refusal of the stream is final: the destination fails on its first try and is checked.
    let (url, _seen) = server(vec![Plan::Refuse]);
    let mut out = Streaming::start_full(&job, vec![target("a", &url, "k")], quick(), Seams::default(), Some((Arc::clone(&checks), auto))).unwrap();
    let feed = Feed::new(&out);
    let d = until(&out, "a", |d| matches!(d.status, DestinationStatus::Failed(_)));
    drop(feed);
    let DestinationStatus::Failed(f) = &d.status else { panic!("{d:?}") };
    assert_eq!(f.cause, Cause::RejectedPublish, "{f:?}");
    let report = diagnose::tests::done_in(&checks, "Show", "a");
    assert_eq!(report.verdict, diagnose::Verdict::Rejected, "{report:?}");
    assert!(out.stop().is_err());
    diagnose::tests::until_checks(&checks, |c| (c.running() == 0).then_some(()));
    assert_eq!(checks_run(&fake), 1);
}

/// The media messages `rx` sees until the connection ends, after those already taken.
fn rest_of_media(rx: &mpsc::Receiver<Seen>, mut got: Vec<Seen>) -> Vec<Seen> {
    loop {
        match next(rx) {
            m @ Seen::Media { .. } => got.push(m),
            Seen::Closed => return got,
            Seen::Publish(_) => {}
        }
    }
}

/// The timestamps of the coded audio or video messages in `m`, leaving out the sequence headers.
fn coded(m: &[Seen], want_video: bool) -> Vec<u32> {
    m.iter()
        .filter_map(|s| match s {
            Seen::Media { video, head, timestamp, .. } if *video == want_video && head[1] == 1 => Some(*timestamp),
            _ => None,
        })
        .collect()
}

/// A copy test's directory. It starts empty, and it is removed with what it holds when the guard
/// is dropped, also when the test panics. Declare it before the stream, so the stream and its
/// threads have stopped when it is dropped.
struct TestDir(PathBuf);

impl TestDir {
    fn new(name: &str) -> Self {
        let dir = temp_file(name).parent().map(Path::to_path_buf).unwrap_or_default();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    /// The copy's file in the directory.
    fn file(&self) -> PathBuf {
        self.0.join("take.mkv")
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn a_copy_test_directory_starts_empty_and_is_removed_with_its_guard() {
    let dir = TestDir::new("stream-copy-dir");
    std::fs::write(dir.file(), b"left by a test that panicked").unwrap();
    let path = dir.file();
    // A run that panicked before its guard was dropped left its file; the next run starts empty.
    std::mem::forget(dir);
    let dir = TestDir::new("stream-copy-dir");
    assert!(!path.exists(), "the directory starts empty");
    std::fs::write(dir.file(), b"data").unwrap();
    let held = std::panic::catch_unwind(move || {
        let _dir = dir;
        panic!("a failing assertion");
    });
    assert!(held.is_err());
    assert!(!path.parent().is_some_and(Path::exists), "the guard removed the directory as the test panicked");
}

#[test]
fn the_copy_holds_the_pictures_and_sound_the_destination_got() {
    let dir = TestDir::new("stream-copy");
    let path = dir.file();
    let (url, seen) = server(vec![Plan::Accept]);
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let mut out = Streaming::start_copying(&job, vec![target("a", &url, "k")], quick(), path.clone(), copy::create(&path)).unwrap();
    until(&out, "a", |d| d.status == DestinationStatus::Live);
    // 1.5 s of media fits in the rings, so neither the destination nor the copy loses a packet.
    let frames = Feeder::default().feed(&out.video(), &out.audio(), 1.5);
    out.stop().unwrap();
    let sent = rest_of_media(&seen, Vec::new());
    let state = out.stream_copy().unwrap();
    let file = std::fs::read(&path).unwrap();
    assert!(state.finished && state.failure.is_none() && state.lost_packets == 0, "{state:?}");
    assert_eq!(state.start_ns, 0, "the file starts at the stream's first frame");
    assert_eq!((state.path.as_path(), state.bytes), (path.as_path(), file.len() as u64));
    let (tracks, [video, audio]) = packets(&file);
    assert_eq!((tracks[0].codec_id.as_str(), tracks[1].codec_id.as_str()), ("V_MPEG4/ISO/AVC", "A_AAC"));
    assert_eq!(tracks[1].codec_delay_ns, 0, "the copy starts after the encoder's delay");
    assert_eq!(video.len(), coded(&sent, true).len(), "every picture the destination got");
    assert_eq!(video.len() as u64, frames);
    assert!(video[0].1, "the copy starts with a keyframe");
    let mut shown: Vec<i64> = video.iter().map(|v| v.0).collect();
    shown.sort_unstable();
    assert!(shown[0] >= 0 && shown.windows(2).all(|w| w[1] - w[0] == 100_000_000), "one picture every 100 ms: {shown:?}");
    // The file starts at the stream's first frame, so its times are the stream's.
    assert_eq!(state.last_video_ns, shown.last().and_then(|&t| u64::try_from(t).ok()), "the copy knows its last picture");
    assert_eq!(decode(&tracks[0].codec_private, &video).len(), video.len(), "every picture decodes");
    // The sound is the destination's, at the same times (the file keeps milliseconds).
    let ms: Vec<i64> = audio.iter().map(|a| a.0.div_euclid(NS_PER_MS)).collect();
    let flv: Vec<i64> = coded(&sent, false).into_iter().map(i64::from).collect();
    assert_eq!(ms.len(), flv.len());
    assert!(ms.iter().zip(&flv).all(|(a, b)| a.abs_diff(*b) <= 1), "{ms:?} {flv:?}");
}

/// A file whose first write waits until its gate's sender is dropped.
struct Gated {
    file: std::fs::File,
    gate: Option<mpsc::Receiver<()>>,
}

impl Write for Gated {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Some(gate) = self.gate.take() {
            let _ = gate.recv();
        }
        self.file.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl std::io::Seek for Gated {
    fn seek(&mut self, to: std::io::SeekFrom) -> io::Result<u64> {
        self.file.seek(to)
    }
}

#[test]
fn a_copy_whose_disk_stalls_holds_back_neither_the_stream_nor_the_destination() {
    let dir = TestDir::new("stream-copy-stall");
    let path = dir.file();
    let (url, seen) = server(vec![Plan::Accept]);
    let (release, gate) = mpsc::channel::<()>();
    let file = std::fs::File::create(&path).unwrap();
    let open: copy::Open = Box::new(move || Ok(Box::new(Gated { file, gate: Some(gate) }) as Box<dyn copy::CopyFile>));
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let mut out = Streaming::start_copying(&job, vec![target("a", &url, "k")], quick(), path.clone(), open).unwrap();
    let feed = Feed::new(&out);
    assert_eq!(next(&seen), Seen::Publish("k".into()));
    // The copy's first write waits the whole time the destination gets these.
    starts_with_headers_and_a_keyframe(&media(&seen, 60));
    let stalled = out.stream_copy().unwrap();
    assert_eq!((stalled.bytes, stalled.finished, stalled.failure.as_deref()), (0, false, None), "{stalled:?}");
    assert!(matches!(out.status(), OutputStatus::Running), "{:?}", out.status());
    // The packets the copy loses while its first write waits are counted before that write
    // returns. GUARD only ends a test that would otherwise hang.
    let waited = Instant::now();
    loop {
        let s = out.stream_copy().unwrap();
        if s.lost_packets > 0 {
            break;
        }
        assert!(waited.elapsed() < GUARD, "the stalled copy counted no lost packets: {s:?}");
        std::thread::yield_now();
    }
    drop(release);
    drop(feed);
    out.stop().unwrap();
    until_closed(&seen);
    let state = out.stream_copy().unwrap();
    let bytes = std::fs::read(&path).unwrap();
    assert!(state.finished && state.failure.is_none(), "{state:?}");
    let (_, [video, _]) = packets(&bytes);
    assert!(video.first().is_some_and(|v| v.1), "after the packets it lost, the copy goes on from a keyframe");
}

#[test]
fn a_copy_whose_disk_stalls_before_its_first_packet_says_where_in_the_stream_its_file_starts() {
    let path = temp_file("stream-copy-late");
    let (url, seen) = server(vec![Plan::Accept]);
    let (release, gate) = mpsc::channel::<()>();
    let file = std::fs::File::create(&path).unwrap();
    let open: copy::Open = Box::new(move || Ok(Box::new(Gated { file, gate: Some(gate) }) as Box<dyn copy::CopyFile>));
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let mut out = Streaming::start_copying(&job, vec![target("a", &url, "k")], quick(), path.clone(), open).unwrap();
    let feed = Feed::new(&out);
    assert_eq!(next(&seen), Seen::Publish("k".into()));
    // The copy's first write waits until its ring is full and the destination has gone on past
    // the ring's span, so the first packets the ring holds are stale once the write returns.
    let waited = Instant::now();
    while out.stream_copy().unwrap().lost_packets == 0 {
        assert!(waited.elapsed() < GUARD, "the stalled copy counted no lost packets");
        std::thread::yield_now();
    }
    let span = u64::from(RING_SECONDS) * 1000;
    loop {
        match next(&seen) {
            Seen::Media { timestamp, .. } if u64::from(timestamp) > 2 * span => break,
            Seen::Closed => panic!("the destination's connection ended"),
            _ => {}
        }
    }
    drop(release);
    // The stream goes on while the copy writes again. GUARD only ends a test that would otherwise
    // hang.
    let waited = Instant::now();
    while out.stream_copy().unwrap().start_ns == 0 {
        assert!(waited.elapsed() < GUARD, "the copy never wrote a packet: {:?}", out.stream_copy());
        std::thread::yield_now();
    }
    drop(feed);
    out.stop().unwrap();
    until_closed(&seen);
    let state = out.stream_copy().unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
    assert!(state.finished && state.failure.is_none(), "{state:?}");
    // The file starts at a keyframe after the opening the copy lost, at its own time 0.
    assert!(state.start_ns >= span * 1_000_000 && state.start_ns.is_multiple_of(1_000_000_000), "{state:?}");
    let (_, [video, _]) = packets(&bytes);
    assert!(video.first().is_some_and(|v| v.1 && v.0 == 0), "{:?}", video.first());
}

#[test]
fn a_copy_that_cannot_be_written_stops_alone() {
    let (url, seen) = server(vec![Plan::Accept]);
    let open: copy::Open = Box::new(|| Err(io::Error::new(io::ErrorKind::PermissionDenied, "the folder is read-only")));
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let mut out = Streaming::start_copying(&job, vec![target("a", &url, "k")], quick(), "/videos/Stream.mkv".into(), open).unwrap();
    let feed = Feed::new(&out);
    assert_eq!(next(&seen), Seen::Publish("k".into()));
    media(&seen, 30);
    // The copy fails on its own thread; GUARD only ends a test that would otherwise hang.
    let waited = Instant::now();
    let state = loop {
        let s = out.stream_copy().unwrap();
        if s.failure.is_some() {
            break s;
        }
        assert!(waited.elapsed() < GUARD, "the copy never failed: {s:?}");
        std::thread::yield_now();
    };
    let failure = state.failure.unwrap_or_default();
    assert!(failure.contains("could not be written") && failure.contains("read-only"), "{failure}");
    assert!(!state.finished);
    assert_eq!(until(&out, "a", |_| true).status, DestinationStatus::Live);
    drop(feed);
    out.stop().unwrap();
    until_closed(&seen);
}

#[test]
fn a_copy_leaves_a_file_that_already_holds_data_as_it_is() {
    let dir = TestDir::new("stream-copy-taken");
    let path = dir.file();
    // The reserved empty file opens for writing.
    std::fs::File::create(&path).unwrap();
    assert!(copy::create(&path)().is_ok());
    // A file put in its place that holds data is not opened, and keeps its data.
    std::fs::write(&path, b"someone else's file").unwrap();
    let err = copy::create(&path)().err().map(|e| e.to_string()).unwrap_or_default();
    let kept = std::fs::read(&path).unwrap();
    assert!(err.contains("already holds data"), "{err}");
    assert_eq!(kept, b"someone else's file");
}

/// A file whose first flush says it was reached and then waits until its gate's sender is
/// dropped. Dropping the file drops `dropped`.
struct FlushGated {
    file: std::fs::File,
    reached: Option<mpsc::Sender<()>>,
    gate: Option<mpsc::Receiver<()>>,
    _dropped: mpsc::Sender<()>,
}

impl Write for FlushGated {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        if let Some(reached) = self.reached.take() {
            let _ = reached.send(());
        }
        if let Some(gate) = self.gate.take() {
            let _ = gate.recv();
        }
        self.file.flush()
    }
}

impl std::io::Seek for FlushGated {
    fn seek(&mut self, to: std::io::SeekFrom) -> io::Result<u64> {
        self.file.seek(to)
    }
}

/// A stream's packets for a copy without encoders: the fanout, the copy's subscription and an H.264
/// and AAC track.
fn copy_feed() -> (Fanout, Receiver, Vec<TrackSpec>) {
    let mut video = TrackSpec::new(craft_media::matroska::TrackKind::Video, "V_MPEG4/ISO/AVC");
    video.video_size = Some((64, 36));
    let mut audio = TrackSpec::new(craft_media::matroska::TrackKind::Audio, "A_AAC");
    audio.audio = Some((48_000.0, 2, None));
    let rates = StreamRates { video_packets_per_second: 10, audio_packets_per_second: 50, bits_per_second: 500_000, ticks_per_second: 1_000_000_000 };
    let fanout = Fanout::new();
    let rx = fanout.subscribe(RingConfig::for_stream(&rates, RING_SECONDS)).unwrap();
    (fanout, rx, vec![video, audio])
}

#[test]
fn a_copy_whose_file_opens_after_a_stop_gave_up_writes_nothing() {
    let dir = TestDir::new("stream-copy-slow-open");
    let path = dir.file();
    let (reached_tx, reached) = mpsc::channel::<()>();
    let (release, gate) = mpsc::channel::<()>();
    let (dropped_tx, dropped) = mpsc::channel::<()>();
    let file = std::fs::File::create(&path).unwrap();
    let open: copy::Open = Box::new(move || {
        let _ = reached_tx.send(());
        let _ = gate.recv();
        let f = FlushGated { file, reached: None, gate: None, _dropped: dropped_tx };
        Ok(Box::new(f) as Box<dyn copy::CopyFile>)
    });
    let (fanout, rx, tracks) = copy_feed();
    let mut c = copy::StreamCopy::start(path.clone(), open, tracks, rx);
    // The file opens only after the stream has ended and the stop has given up on the copy.
    reached.recv().unwrap();
    drop(fanout);
    c.finish(Duration::ZERO);
    drop(release);
    // The copy's thread drops the file as it ends.
    assert_eq!(dropped.recv(), Err(mpsc::RecvError));
    let state = c.state();
    let len = std::fs::metadata(&path).map(|m| m.len());
    assert_eq!((state.bytes, len.ok()), (0, Some(0)), "{state:?}");
    assert!(!state.finished, "{state:?}");
}

#[test]
fn a_copy_that_a_stop_left_unfinished_stays_unfinished() {
    let dir = TestDir::new("stream-copy-unfinished");
    let path = dir.file();
    let (reached_tx, reached) = mpsc::channel::<()>();
    let (release, gate) = mpsc::channel::<()>();
    let (dropped_tx, dropped) = mpsc::channel::<()>();
    let file = std::fs::File::create(&path).unwrap();
    let open: copy::Open = Box::new(move || {
        let f = FlushGated { file, reached: Some(reached_tx), gate: Some(gate), _dropped: dropped_tx };
        Ok(Box::new(f) as Box<dyn copy::CopyFile>)
    });
    let (fanout, rx, tracks) = copy_feed();
    let mut c = copy::StreamCopy::start(path.clone(), open, tracks, rx);
    // The stream ends, and the copy's last flush waits while the stop gives up on it. A copy that
    // fails before its flush drops the file, which ends these waits too.
    drop(fanout);
    reached.recv().unwrap();
    c.finish(Duration::ZERO);
    drop(release);
    // The copy's thread drops the file as it ends.
    assert_eq!(dropped.recv(), Err(mpsc::RecvError));
    let state = c.state();
    assert!(!state.finished, "{state:?}");
    assert!(state.failure.as_deref().is_some_and(|f| f.contains("was not finished")), "{state:?}");
}

#[test]
fn a_copy_needs_the_keyframe_interval_of_a_recording() {
    let (url, seen) = server(vec![Plan::Accept]);
    let opened = Arc::new(AtomicBool::new(false));
    let o = Arc::clone(&opened);
    let open: copy::Open = Box::new(move || {
        o.store(true, Ordering::SeqCst);
        Err(io::Error::other("opened"))
    });
    let keyframe_s = studiocraft_encode::h264::RECORDING_KEYFRAME_S + 1;
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s });
    let mut out = Streaming::start_copying(&job, vec![target("a", &url, "k")], quick(), "/videos/Stream.mkv".into(), open).unwrap();
    let feed = Feed::new(&out);
    // The copy is refused before it starts, so its state says so at once.
    let state = out.stream_copy().unwrap();
    let failure = state.failure.unwrap_or_default();
    assert!(failure.contains("keyframe") && failure.contains(&format!("every {keyframe_s}")), "{failure}");
    assert!(!state.finished);
    assert_eq!(next(&seen), Seen::Publish("k".into()));
    media(&seen, 30);
    assert_eq!(until(&out, "a", |_| true).status, DestinationStatus::Live);
    drop(feed);
    out.stop().unwrap();
    until_closed(&seen);
    assert!(!opened.load(Ordering::SeqCst), "the refused copy made no file");
}

/// What a CEA-608 decoder shows after the caption pairs of `video`, one message per frame.
fn captions_shown(video: &[Vec<[u8; 2]>]) -> String {
    let pairs = video.iter().enumerate().flat_map(|(f, p)| p.iter().map(move |p| (f as i64, p[0], p[1])));
    craft_captions::scc::decode_pairs(pairs, craft_captions::FrameRate::FPS_29_97).last().map(|c| c.text.clone()).unwrap_or_default()
}

#[test]
fn finished_phrases_go_out_as_closed_captions_and_a_late_phrase_holds_nothing_up() {
    let (url, seen) = server(vec![Plan::Accept]);
    let mut job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let phrases = Arc::new(Fifo::new(8));
    job.captions = Some(Arc::clone(&phrases));
    let mut out = Streaming::start_timed(&job, vec![target("a", &url, "k")], quick()).unwrap();
    let feed = Feed::new(&out);
    // No phrase comes for a while, and the stream goes on without it.
    let video = |m: Vec<Seen>| -> Vec<Vec<[u8; 2]>> {
        m.into_iter()
            .filter_map(|s| match s {
                Seen::Media { video: true, head: [_, 1], captions, .. } => Some(captions),
                _ => None,
            })
            .collect()
    };
    let before = video(media(&seen, 60));
    assert!(before.len() >= 5, "{}", before.len());
    assert!(before.iter().all(Vec::is_empty), "{before:?}");
    phrases.push("hello from the stream".to_string());
    let mut after = Vec::new();
    for _ in 0..2000 {
        after.extend(video(media(&seen, 1)));
        if captions_shown(&after) == "hello from the stream" {
            break;
        }
    }
    assert_eq!(captions_shown(&after), "hello from the stream");
    drop(feed);
    out.stop().unwrap();
    until_closed(&seen);
}

#[test]
fn a_destination_with_its_own_encoder_gets_the_closed_captions_too() {
    let (a, seen_a) = server(vec![Plan::Accept]);
    let (b, seen_b) = server(vec![Plan::Accept]);
    let mut job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let phrases = Arc::new(Fifo::new(8));
    job.captions = Some(Arc::clone(&phrases));
    let targets = vec![target("a", &a, "ka"), with_own("b", &b, "kb", own(1200, "speed"))];
    let mut out = Streaming::start_timed(&job, targets, quick()).unwrap();
    let feed = Feed::new(&out);
    let video = |m: Vec<Seen>| -> Vec<Vec<[u8; 2]>> {
        m.into_iter()
            .filter_map(|s| match s {
                Seen::Media { video: true, head: [_, 1], captions, .. } => Some(captions),
                _ => None,
            })
            .collect()
    };
    // Both destinations send before the phrase comes.
    assert!(!video(media(&seen_a, 20)).is_empty());
    assert!(!video(media(&seen_b, 20)).is_empty());
    phrases.push("for every destination".to_string());
    for seen in [&seen_a, &seen_b] {
        let mut after = Vec::new();
        for _ in 0..2000 {
            after.extend(video(media(seen, 1)));
            if captions_shown(&after) == "for every destination" {
                break;
            }
        }
        assert_eq!(captions_shown(&after), "for every destination");
    }
    drop(feed);
    out.stop().unwrap();
    until_closed(&seen_a);
    until_closed(&seen_b);
}

#[test]
fn a_stream_with_a_copy_leaves_a_subscription_for_it() {
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let targets: Vec<Target> = (0..MAX_SUBSCRIBERS).map(|i| target(&format!("d{i}"), "rtmp://127.0.0.1:1/live", "k")).collect();
    let open: copy::Open = Box::new(|| Err(io::Error::other("not opened")));
    let e = Streaming::start_copying(&job, targets.clone(), quick(), "/videos/Stream.mkv".into(), open).err().unwrap();
    assert!(e.to_string().contains(&format!("at most {} destinations", MAX_SUBSCRIBERS - 1)), "{e}");
    let open: copy::Open = Box::new(|| Err(io::Error::other("not opened")));
    let mut out = Streaming::start_copying(&job, targets[1..].to_vec(), quick(), "/videos/Stream.mkv".into(), open).unwrap();
    assert!(out.stream_copy().is_some_and(|c| c.failure.is_none_or(|f| f.contains("not opened"))));
    let _ = out.stop();
    // A copy refused for its keyframe interval takes no subscription, so all the destinations start.
    let long = crate::native::tests::job(RateControl::Bitrate { kbps: 400, keyframe_s: studiocraft_encode::h264::RECORDING_KEYFRAME_S + 1 });
    let open: copy::Open = Box::new(|| Err(io::Error::other("not opened")));
    let mut out = Streaming::start_copying(&long, targets, quick(), "/videos/Stream.mkv".into(), open).unwrap();
    assert!(out.stream_copy().and_then(|c| c.failure).is_some_and(|f| f.contains("keyframe")));
    let _ = out.stop();
}

#[test]
fn a_copy_that_took_no_subscription_leaves_its_place_to_a_destination() {
    // Each destination waits long before it tries again, so every one keeps its place.
    let slow = Timing { first: Duration::from_secs(600), longest: Duration::from_secs(600), max_tries: 100, ..quick() };
    let long = crate::native::tests::job(RateControl::Bitrate { kbps: 400, keyframe_s: studiocraft_encode::h264::RECORDING_KEYFRAME_S + 1 });
    let targets: Vec<Target> = (1..MAX_SUBSCRIBERS).map(|i| target(&format!("d{i}"), "rtmp://127.0.0.1:1/live", "k")).collect();
    let open: copy::Open = Box::new(|| Err(io::Error::other("not opened")));
    let mut out = Streaming::start_copying(&long, targets, slow, "/videos/Stream.mkv".into(), open).unwrap();
    assert!(out.stream_copy().and_then(|c| c.failure).is_some_and(|f| f.contains("keyframe")));
    assert_eq!(lock(&out.hub.dests).iter().filter(|d| d.running()).count(), MAX_SUBSCRIBERS - 1);
    // The refused copy holds no subscription, so the last one is free.
    assert!(out.start_destination_as(&setup("d0", "rtmp://127.0.0.1:1/live", "k")).unwrap());
    assert_eq!(out.destinations().len(), MAX_SUBSCRIBERS);
    let _ = out.stop();
}

#[test]
fn a_copy_whose_file_failed_leaves_its_place_to_a_destination() {
    // Each destination waits long before it tries again, so every one keeps its place.
    let slow = Timing { first: Duration::from_secs(600), longest: Duration::from_secs(600), max_tries: 100, ..quick() };
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let targets: Vec<Target> = (1..MAX_SUBSCRIBERS).map(|i| target(&format!("d{i}"), "rtmp://127.0.0.1:1/live", "k")).collect();
    let open: copy::Open = Box::new(|| Err(io::Error::other("not opened")));
    let mut out = Streaming::start_copying(&job, targets, slow, "/videos/Stream.mkv".into(), open).unwrap();
    // The copy's thread ends after its file fails to open, and its receiver goes with it.
    while out.copy.as_ref().is_some_and(StreamCopy::subscribed) {
        std::thread::yield_now();
    }
    assert!(out.stream_copy().and_then(|c| c.failure).is_some_and(|f| f.contains("not opened")));
    assert_eq!(lock(&out.hub.dests).iter().filter(|d| d.running()).count(), MAX_SUBSCRIBERS - 1);
    assert!(out.start_destination_as(&setup("d0", "rtmp://127.0.0.1:1/live", "k")).unwrap());
    assert_eq!(out.destinations().len(), MAX_SUBSCRIBERS);
    let _ = out.stop();
}

#[test]
fn destinations_that_are_off_do_not_count_against_the_copy() {
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let mut targets: Vec<Target> = (0..MAX_SUBSCRIBERS).map(|i| target(&format!("d{i}"), "rtmp://127.0.0.1:1/live", "k")).collect();
    targets[0].enabled = false;
    let open: copy::Open = Box::new(|| Err(io::Error::other("not opened")));
    let mut out = Streaming::start_copying(&job, targets, quick(), "/videos/Stream.mkv".into(), open).unwrap();
    assert!(out.stream_copy().is_some_and(|c| c.failure.is_none_or(|f| f.contains("not opened"))));
    let _ = out.stop();
}

#[test]
fn phrases_from_before_the_stream_are_not_sent() {
    let phrases = Arc::new(Fifo::new(8));
    phrases.push("said before the stream started".to_string());
    let mut c = Captions::new(Arc::clone(&phrases), None, 30, 1).unwrap();
    assert!(phrases.is_empty());
    let slice = [0, 0, 0, 2, 0x65, 0x88];
    let mut au = slice.to_vec();
    c.add(&mut au);
    assert_eq!(au, slice, "nothing was due, so the access unit is unchanged");
    // A phrase given after the start goes out with the next picture.
    phrases.push("now".to_string());
    c.add(&mut au);
    assert!(!sei::read_sei(Codec::H264, &au).is_empty());
    assert!(au.ends_with(&slice), "the SEI goes before the slice");
}

#[test]
fn a_stream_at_a_frame_rate_of_zero_is_refused_with_captions() {
    let phrases = Arc::new(Fifo::new(8));
    let Err(e) = Captions::new(phrases, None, 0, 1) else { panic!("a zero frame rate was taken") };
    assert!(e.to_string().contains("frame rate"), "{e}");
}

/// The built-in H.264 encoder at `kbps` with `preset`, as a destination runs it for itself.
fn own(kbps: u32, preset: &str) -> OwnEncoder {
    OwnEncoder { encoder: "h264".into(), settings: Settings::new().with("preset", preset), kbps }
}

/// The destination `id` with the encoder `own` of its own.
fn with_own(id: &str, server: &str, key: &str, own: OwnEncoder) -> Target {
    Target { encoder: Some(own), ..target(id, server, key) }
}

/// The encoder whose packets the destination `id` sends.
fn feed_of(out: &Streaming, id: &str) -> Arc<EncoderFeed> {
    out.hub.find(id).unwrap().feed()
}

#[test]
fn destinations_run_their_own_encoders_beside_the_streams() {
    let (a, seen_a) = server(vec![Plan::Accept]);
    let (b, seen_b) = server(vec![Plan::Accept]);
    let (c, seen_c) = server(vec![Plan::Accept]);
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let targets = vec![target("a", &a, "ka"), with_own("b", &b, "kb", own(1200, "quality")), with_own("c", &c, "kc", own(1200, "quality"))];
    let mut out = Streaming::start_timed(&job, targets, quick()).unwrap();
    // The input goes to every encoder, not to the stream's alone.
    assert!(!Arc::ptr_eq(&out.video(), &out.native.video()));
    assert!(!Arc::ptr_eq(&out.audio(), &out.native.audio()));
    let feed = Feed::new(&out);
    for (seen, key) in [(&seen_a, "ka"), (&seen_b, "kb"), (&seen_c, "kc")] {
        assert_eq!(next(seen), Seen::Publish(key.into()));
        starts_with_headers_and_a_keyframe(&media(seen, 3));
        media(seen, 20);
    }
    // b and c name the same encoder, settings and bitrate, so they share one encoder of their own.
    assert_eq!(lock(&out.own).len(), 1);
    assert!(Arc::ptr_eq(&feed_of(&out, "a"), &out.hub.main));
    assert!(Arc::ptr_eq(&feed_of(&out, "b"), &feed_of(&out, "c")));
    assert!(!Arc::ptr_eq(&feed_of(&out, "b"), &out.hub.main));
    assert_eq!(feed_of(&out, "b").own, Some(own(1200, "quality")));
    // The two encoders each have their bitrate in their headers.
    assert_ne!(feed_of(&out, "a").headers.script, feed_of(&out, "b").headers.script);
    for id in ["a", "b", "c"] {
        until(&out, id, |d| d.status == DestinationStatus::Live);
    }
    drop(feed);
    out.stop().unwrap();
    for seen in [&seen_a, &seen_b, &seen_c] {
        until_closed(seen);
    }
    for id in ["a", "b", "c"] {
        assert_eq!(until(&out, id, |_| true).status, DestinationStatus::Stopped, "{id}");
    }
}

#[test]
fn an_encoder_no_destination_holds_gives_up_its_place() {
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let mut out = Streaming::start_timed(&job, vec![with_own("b", &closed_url(), "kb", own(1200, "quality"))], quick()).unwrap();
    let held = feed_for_test(&out, &own(800, "quality"));
    // Started for a destination that has since taken another encoder: nothing holds it.
    drop(feed_for_test(&out, &own(700, "quality")));
    assert_eq!(lock(&out.own).len(), 3);
    {
        let mut groups = lock(&out.own);
        out.drop_unused(&mut groups);
    }
    let left: Vec<Option<u32>> = lock(&out.own).iter().map(|(f, _)| f.own.as_ref().map(|o| o.kbps)).collect();
    assert_eq!(left, vec![Some(1200), Some(800)], "b's encoder and the held one stay, in order");
    drop(held);
    // b has no server to reach, so the stream may have failed by now.
    let _ = out.stop();
}

/// The encoder `own` names, started or shared as a destination starting with it gets it.
fn feed_for_test(out: &Streaming, own: &OwnEncoder) -> Arc<EncoderFeed> {
    out.feed_for(Some(own)).unwrap_or_else(|e| panic!("no encoder for {own:?}: {e}"))
}

#[test]
fn a_destination_that_names_the_streams_encoder_shares_it() {
    let (a, seen) = server(vec![Plan::Accept]);
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let same = OwnEncoder { encoder: "h264".into(), settings: Settings::new(), kbps: 400 };
    let mut out = Streaming::start_timed(&job, vec![with_own("a", &a, "k", same)], quick()).unwrap();
    let feed = Feed::new(&out);
    assert_eq!(next(&seen), Seen::Publish("k".into()));
    starts_with_headers_and_a_keyframe(&media(&seen, 3));
    assert!(Arc::ptr_eq(&feed_of(&out, "a"), &out.hub.main));
    assert!(lock(&out.own).is_empty(), "no second encoder runs");
    assert!(out.split.is_none(), "the stream does not split its input");
    drop(feed);
    out.stop().unwrap();
    until_closed(&seen);
}

#[test]
fn a_destination_whose_encoder_cannot_start_fails_alone() {
    let (a, seen) = server(vec![Plan::Accept]);
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let missing = OwnEncoder { encoder: "none-such".into(), settings: Settings::new(), kbps: 900 };
    let slow = own(50, "speed");
    let targets = vec![target("a", &a, "ka"), with_own("b", &closed_url(), "kb", missing), with_own("c", &closed_url(), "kc", slow)];
    let mut out = Streaming::start_timed(&job, targets, quick()).unwrap();
    let feed = Feed::new(&out);
    assert_eq!(next(&seen), Seen::Publish("ka".into()));
    starts_with_headers_and_a_keyframe(&media(&seen, 3));
    let DestinationStatus::Failed(b) = until(&out, "b", |d| matches!(d.status, DestinationStatus::Failed(_))).status else { unreachable_state() };
    assert_eq!(b.detail, "its video encoder could not start: there is no video encoder `none-such` for a destination to run");
    assert_eq!(b.cause, Cause::Encoder);
    let DestinationStatus::Failed(c) = until(&out, "c", |d| matches!(d.status, DestinationStatus::Failed(_))).status else { unreachable_state() };
    assert!(c.detail.contains("must be 100 to 200000 kbps, not 50"), "{}", c.detail);
    assert_eq!(c.cause, Cause::Encoder);
    assert!(lock(&out.own).is_empty());
    media(&seen, 20);
    assert_eq!(until(&out, "a", |d| d.status == DestinationStatus::Live).status, DestinationStatus::Live);
    drop(feed);
    out.stop().unwrap();
    until_closed(&seen);
}

/// Ends a test that reached a state its `until` excluded.
fn unreachable_state() -> ! {
    panic!("the destination is in a state its wait excluded")
}

#[test]
fn a_destination_fails_when_its_own_encoder_stops_and_the_stream_goes_on() {
    let (a, seen_a) = server(vec![Plan::Accept]);
    let (b, seen_b) = server(vec![Plan::Accept, Plan::Accept]);
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let mut out = Streaming::start_timed(&job, vec![target("a", &a, "ka"), with_own("b", &b, "kb", own(800, "speed"))], quick()).unwrap();
    let feed = Feed::new(&out);
    for seen in [&seen_a, &seen_b] {
        let _ = next(seen);
        media(seen, 10);
    }
    // Its encoder ends while the stream runs; how the encoder ends does not matter here.
    let _ = lock(&out.own)[0].1.stop();
    let DestinationStatus::Failed(f) = until(&out, "b", |d| matches!(d.status, DestinationStatus::Failed(_))).status else { unreachable_state() };
    assert!(f.detail.starts_with("its video encoder stopped"), "{}", f.detail);
    until_closed(&seen_b);
    // The stream's encoder and its destination go on.
    media(&seen_a, 20);
    assert_eq!(until(&out, "a", |_| true).status, DestinationStatus::Live);
    // Started again, it gets a new encoder of its own, and the one that ended is dropped, so it
    // no longer counts toward the encoders the stream can run.
    let old = feed_of(&out, "b");
    out.start_destination("b", None).unwrap();
    feed.pace(&out);
    assert_eq!(lock(&out.own).len(), 1);
    assert!(!Arc::ptr_eq(&feed_of(&out, "b"), &old));
    assert!(!feed_of(&out, "b").control.is_ended());
    // It sends again, from the new encoder's headers and a keyframe on.
    assert_eq!(next(&seen_b), Seen::Publish("kb".into()));
    starts_with_headers_and_a_keyframe(&media(&seen_b, 3));
    media(&seen_b, 10);
    drop(feed);
    out.stop().unwrap();
    until_closed(&seen_a);
    until_closed(&seen_b);
}

/// A stream sending to `a` with its own encoder and to `b` with an encoder of `b`'s own, which is
/// then stopped, with the servers' views of `a` and `b` and the encoder `b` sent with.
fn with_b_stopped(b_plans: Vec<Plan>) -> (Streaming, Feed, mpsc::Receiver<Seen>, mpsc::Receiver<Seen>, Arc<EncoderFeed>) {
    let (a, seen_a) = server(vec![Plan::Accept]);
    let (b, seen_b) = server(b_plans);
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let out = Streaming::start_timed(&job, vec![target("a", &a, "ka"), with_own("b", &b, "kb", own(800, "speed"))], quick()).unwrap();
    let feed = Feed::new(&out);
    for seen in [&seen_a, &seen_b] {
        let _ = next(seen);
        media(seen, 10);
    }
    out.stop_destination("b").unwrap();
    until(&out, "b", |d| d.status == DestinationStatus::Stopped);
    until_ended(&out, "b");
    until_closed(&seen_b);
    let old = feed_of(&out, "b");
    assert!(old.own.is_some());
    (out, feed, seen_a, seen_b, old)
}

/// An encoder that cannot start, for a destination.
fn missing_encoder() -> OwnEncoder {
    OwnEncoder { encoder: "none-such".into(), settings: Settings::new(), kbps: 900 }
}

#[test]
fn a_stopped_destination_started_with_another_encoder_lets_go_of_its_own_first() {
    let (mut out, feed, seen_a, seen_b, old) = with_b_stopped(vec![Plan::Accept, Plan::Accept]);
    // Before its new encoder is looked for, it lets go of the one it held, so at the limit of
    // encoders that one can be dropped to make room. This new one cannot start.
    let e = out.start_destination_as(&DestinationSetup { encoder: Some(missing_encoder()), ..setup("b", &closed_url(), "kb") }).unwrap_err();
    assert!(e.to_string().contains("none-such"), "{e}");
    assert!(!Arc::ptr_eq(&feed_of(&out, "b"), &old));
    assert_eq!(until(&out, "b", |_| true).status, DestinationStatus::Stopped);
    // Started again as it was, it sends with the encoder it held, which still runs.
    out.start_destination("b", None).unwrap();
    assert!(Arc::ptr_eq(&feed_of(&out, "b"), &old));
    assert_eq!(next(&seen_b), Seen::Publish("kb".into()));
    media(&seen_b, 10);
    drop(feed);
    out.stop().unwrap();
    until_closed(&seen_a);
    until_closed(&seen_b);
}

#[test]
fn the_destination_an_added_one_replaces_lets_go_of_its_encoder_first() {
    let (mut out, feed, seen_a, _seen_b, old) = with_b_stopped(vec![Plan::Accept]);
    // The list fills with destinations that fail, so an added one takes the place of `b`, the
    // only stopped one.
    let (refusing, _refused) = server(vec![Plan::RefuseConnect; MAX_SUBSCRIBERS]);
    for i in 2..MAX_SUBSCRIBERS {
        let id = format!("d{i}");
        out.start_destination_as(&setup(&id, &refusing, "k")).unwrap();
        until(&out, &id, |d| matches!(d.status, DestinationStatus::Failed(_)));
        until_ended(&out, &id);
    }
    assert_eq!(out.destinations().len(), MAX_SUBSCRIBERS);
    let e = out.start_destination_as(&DestinationSetup { encoder: Some(missing_encoder()), ..setup("new", &closed_url(), "k") }).unwrap_err();
    assert!(e.to_string().contains("none-such"), "{e}");
    assert!(!Arc::ptr_eq(&feed_of(&out, "b"), &old));
    drop(feed);
    out.stop().unwrap();
    until_closed(&seen_a);
}

#[test]
fn an_encoder_that_ended_is_dropped_without_another_start() {
    let (a, seen_a) = server(vec![Plan::Accept]);
    let (b, seen_b) = server(vec![Plan::Accept]);
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let mut out = Streaming::start_timed(&job, vec![target("a", &a, "ka"), with_own("b", &b, "kb", own(800, "speed"))], quick()).unwrap();
    let feed = Feed::new(&out);
    for seen in [&seen_a, &seen_b] {
        let _ = next(seen);
        media(seen, 10);
    }
    let queue = Arc::downgrade(&lock(&out.own)[0].1.video());
    let _ = lock(&out.own)[0].1.stop();
    let DestinationStatus::Failed(f) = until(&out, "b", |d| matches!(d.status, DestinationStatus::Failed(_))).status else { unreachable_state() };
    until_closed(&seen_b);
    let _ = out.status();
    assert!(lock(&out.own).is_empty(), "the encoder that ended is dropped");
    // The feed no longer waits for it either.
    feed.pace(&out);
    // So the split no longer hands it input: it holds the queue only while it hands on an item.
    let dropped = Instant::now();
    while queue.upgrade().is_some() {
        assert!(dropped.elapsed() < GUARD, "the split still hands the encoder input");
        std::thread::yield_now();
    }
    // Its destination keeps the failure.
    assert_eq!(until(&out, "b", |_| true).status, DestinationStatus::Failed(f));
    media(&seen_a, 10);
    drop(feed);
    out.stop().unwrap();
    until_closed(&seen_a);
}

#[test]
fn a_destination_waiting_to_try_again_fails_when_its_own_encoder_stops() {
    let (url, seen) = server(vec![Plan::DropAfter(5)]);
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let seams = Seams { clock: TestClock::new(), resolve: Arc::new(system_resolve), route: routed() };
    // The wait before a try again outlasts the test's waits.
    let timing = Timing { first: GUARD * 10, longest: GUARD * 10, ..network_timing() };
    let mut out = Streaming::start_with(&job, vec![with_own("b", &url, "k", own(800, "speed"))], timing, seams).unwrap();
    let feed = Feed::new(&out);
    assert_eq!(next(&seen), Seen::Publish("k".into()));
    until(&out, "b", |d| matches!(d.status, DestinationStatus::Retrying { .. }));
    let _ = lock(&out.own)[0].1.stop();
    let DestinationStatus::Failed(f) = until(&out, "b", |d| matches!(d.status, DestinationStatus::Failed(_))).status else { unreachable_state() };
    assert!(f.detail.starts_with("its video encoder stopped"), "{}", f.detail);
    assert_eq!(f.cause, Cause::Encoder);
    drop(feed);
    let _ = out.stop();
    until_closed(&seen);
}

#[test]
fn a_destination_whose_own_encoder_ends_as_the_stream_stops_does_not_connect_again() {
    let (a, seen_a) = server(vec![Plan::Accept]);
    let (b, seen_b) = server(vec![Plan::Accept, Plan::Accept]);
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let mut out = Streaming::start_timed(&job, vec![target("a", &a, "ka"), with_own("b", &b, "kb", own(800, "speed"))], quick()).unwrap();
    let feed = Feed::new(&out);
    for seen in [&seen_a, &seen_b] {
        let _ = next(seen);
        media(seen, 10);
    }
    // The stream is stopping and the stream's encoder still drains when b's encoder ends.
    out.hub.ending.store(true, Ordering::SeqCst);
    let _ = lock(&out.own)[0].1.stop();
    assert_eq!(until(&out, "b", |d| d.status == DestinationStatus::Stopped).status, DestinationStatus::Stopped);
    until_closed(&seen_b);
    assert!(seen_b.recv_timeout(Duration::from_millis(200)).is_err(), "b connected again");
    drop(feed);
    let _ = out.stop();
    until_closed(&seen_a);
}

#[test]
fn a_destination_that_names_the_hardware_encoder_without_the_stream_fails_alone() {
    let (a, seen) = server(vec![Plan::Accept]);
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let hardware = OwnEncoder { encoder: crate::encoders::HARDWARE.into(), settings: Settings::new(), kbps: 900 };
    let mut out = Streaming::start_timed(&job, vec![target("a", &a, "ka"), with_own("h", &closed_url(), "kh", hardware)], quick()).unwrap();
    let feed = Feed::new(&out);
    assert_eq!(next(&seen), Seen::Publish("ka".into()));
    let DestinationStatus::Failed(h) = until(&out, "h", |d| matches!(d.status, DestinationStatus::Failed(_))).status else { unreachable_state() };
    assert!(h.detail.contains("can run the hardware encoder only when the stream does"), "{}", h.detail);
    media(&seen, 10);
    drop(feed);
    out.stop().unwrap();
    until_closed(&seen);
}

#[test]
fn a_destination_waiting_for_the_network_fails_when_its_own_encoder_stops() {
    let (url, seen) = server(vec![Plan::DropAfter(5)]);
    let (route, set) = switch(Route::Selected);
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let seams = Seams { clock: TestClock::new(), resolve: Arc::new(system_resolve), route };
    let mut out = Streaming::start_with(&job, vec![with_own("b", &url, "k", own(800, "speed"))], network_timing(), seams).unwrap();
    assert_eq!(next(&seen), Seen::Publish("k".into()));
    // The network goes before the server drops the connection, which happens once media flows.
    *lock(&set) = Route::None;
    let feed = Feed::new(&out);
    until(&out, "b", |d| waiting(d).is_some());
    // Its encoder ends while it waits, and no route returns.
    let _ = lock(&out.own)[0].1.stop();
    let DestinationStatus::Failed(f) = until(&out, "b", |d| matches!(d.status, DestinationStatus::Failed(_))).status else { unreachable_state() };
    assert!(f.detail.starts_with("its video encoder stopped"), "{}", f.detail);
    assert_eq!(f.cause, Cause::Encoder);
    drop(feed);
    // Its only destination failed, so the stop reports why.
    let Err(PluginError::Failed(e)) = out.stop() else { panic!("the stream stopped without the failure") };
    assert!(e.contains("its video encoder stopped"), "{e}");
    until_closed(&seen);
}

#[test]
fn a_destination_added_while_the_stream_runs_can_run_its_own_encoder() {
    let (a, seen_a) = server(vec![Plan::Accept]);
    let (b, seen_b) = server(vec![Plan::Accept]);
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    // A destination with an encoder of its own that is off: the stream splits its input from the start.
    let off = Target { enabled: false, ..with_own("z", &closed_url(), "kz", own(900, "speed")) };
    let mut out = Streaming::start_timed(&job, vec![target("a", &a, "ka"), off], quick()).unwrap();
    assert!(lock(&out.own).is_empty(), "an encoder starts with its first destination");
    let feed = Feed::new(&out);
    let _ = next(&seen_a);
    media(&seen_a, 10);
    assert!(out.split.as_ref().and_then(Split::newest).is_some(), "the split saw the stream's frames");
    let setup = DestinationSetup { encoder: Some(own(1500, "speed")), ..setup("b", &b, "kb") };
    assert!(out.start_destination_as(&setup).unwrap());
    feed.pace(&out);
    assert_eq!(next(&seen_b), Seen::Publish("kb".into()));
    starts_with_headers_and_a_keyframe(&media(&seen_b, 3));
    media(&seen_b, 10);
    assert_eq!(lock(&out.own).len(), 1);
    assert_eq!(feed_of(&out, "b").own, Some(own(1500, "speed")));
    drop(feed);
    out.stop().unwrap();
    until_closed(&seen_a);
    until_closed(&seen_b);
}

#[test]
fn a_stream_started_without_a_split_refuses_a_destination_with_its_own_encoder() {
    let (a, seen) = server(vec![Plan::Accept]);
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let mut out = Streaming::start_timed(&job, vec![target("a", &a, "ka")], quick()).unwrap();
    assert!(out.split.is_none());
    assert!(Arc::ptr_eq(&out.video(), &out.native.video()), "the stream's encoder reads the input itself");
    let setup = DestinationSetup { encoder: Some(own(1500, "speed")), ..setup("b", &closed_url(), "kb") };
    let Err(PluginError::Failed(e)) = out.start_destination_as(&setup) else { panic!("it was taken") };
    assert!(e.contains("stop the stream and start it again"), "{e}");
    assert!(out.hub.find("b").is_err(), "nothing was added");
    // One that names the stream's encoder joins it.
    let same = DestinationSetup { encoder: Some(OwnEncoder { encoder: "h264".into(), settings: Settings::new(), kbps: 400 }), ..setup };
    let (b, seen_b) = server(vec![Plan::Accept]);
    assert!(out.start_destination_as(&DestinationSetup { server: b, ..same }).unwrap());
    let feed = Feed::new(&out);
    let _ = next(&seen);
    assert_eq!(next(&seen_b), Seen::Publish("kb".into()));
    media(&seen_b, 3);
    drop(feed);
    out.stop().unwrap();
    until_closed(&seen);
    until_closed(&seen_b);
}

#[test]
fn frames_the_split_drops_count_among_the_streams_dropped_frames() {
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 1 });
    let off = Target { enabled: false, ..with_own("z", &closed_url(), "kz", own(900, "speed")) };
    let mut out = Streaming::start_timed(&job, vec![target("a", &closed_url(), "ka"), off], quick()).unwrap();
    assert!(out.split.is_some());
    let _ = out.stop();
    // The split's thread has ended, so its queue keeps the newest frames and drops the rest.
    let queue = out.video();
    let frames = Pool::new(queue.capacity() + 5, VideoFrame::default);
    for _ in 0..queue.capacity() + 5 {
        queue.push(Arc::new(frames.take()));
    }
    assert_eq!(queue.dropped(), 5);
    let _ = out.status();
    assert_eq!(job.stats.dropped.load(Ordering::Relaxed), out.native.video().dropped() + 5);
}

#[test]
fn an_encoder_of_a_destination_runs_the_streams_frames_at_its_own_bitrate() {
    let job = job(RateControl::Bitrate { kbps: 400, keyframe_s: 3 });
    let j = encoder_job(&job, Some(&own(2500, "quality")), Tick(77)).unwrap();
    assert_eq!(j.rate, RateControl::Bitrate { kbps: 2500, keyframe_s: 3 });
    assert_eq!(j.start, Tick(77));
    assert_eq!(j.video, job.video);
    assert_eq!(j.audio_bitrate_kbps, job.audio_bitrate_kbps);
    assert_eq!(j.video_encoder_settings, Settings::new().with("preset", "quality"));
    assert!(!Arc::ptr_eq(&j.stats, &job.stats), "its frame counts are its own");
    assert_eq!(encoder_job(&job, None, Tick(77)).unwrap().start, job.start);
    for (bad, says) in [
        (OwnEncoder { encoder: "x".into(), ..own(500, "speed") }, "no video encoder `x`"),
        (own(99, "speed"), "not 99"),
        (own(200_001, "speed"), "not 200001"),
    ] {
        let Err(PluginError::Failed(e)) = encoder_job(&job, Some(&bad), Tick(0)) else { panic!("{bad:?} was taken") };
        assert!(e.contains(says), "{e}");
    }
    let Err(PluginError::Failed(e)) = encoder_job(&OutputJob { rate: RateControl::Quality(20), ..job.clone() }, Some(&own(500, "speed")), Tick(0))
    else {
        panic!("a quality rate was taken")
    };
    assert!(e.contains("bitrate"), "{e}");
}
