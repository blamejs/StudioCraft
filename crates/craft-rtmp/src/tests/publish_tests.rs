use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use craft_flv::amf0::{self, Value};
use craft_flv::{Body, Metadata};

#[cfg(feature = "rtmps")]
use super::server::spawn_tls;
use super::server::{Conn, STREAM_ID, WINDOW, info, s, spawn};
use crate::chunk::{csid, msg_type, user_control};
use crate::error::redact;
use crate::publisher::{Acks, acked_total, candidates};
use crate::transport::{Socket, set_test_clock};
use crate::{Config, Error, Kind, Message, Publisher};
#[cfg(feature = "rtmps")]
use crate::{TlsKind, TlsRoots};

const KEY: &str = "live_s3cret_key";
const AVCC: [u8; 7] = [1, 0x64, 0, 0x1F, 0xFF, 0xE0, 0];
const ASC: [u8; 2] = [0x11, 0x90];

fn config(url: &str) -> Config {
    Config::new(url, KEY).unwrap()
}

fn command_names(log: &[Message]) -> Vec<String> {
    log.iter()
        .filter(|m| m.header.type_id == msg_type::COMMAND)
        .map(|m| amf0::read_all(&m.payload).unwrap()[0].as_str().unwrap().to_owned())
        .collect()
}

fn assert_no_key(e: &Error) {
    assert!(!e.to_string().contains(KEY), "{e}");
    assert!(!format!("{e:?}").contains(KEY), "{e:?}");
}

/// Counts the bytes the server writes.
struct Counting<S> {
    inner: S,
    written: u64,
}

impl<S: Read> Read for Counting<S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl<S: Write> Write for Counting<S> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[test]
fn a_stream_is_published_and_closed() {
    let (url, server) = spawn(|stream| {
        let mut c = Conn::new(stream);
        c.handshake().unwrap();
        let publish = c.accept().unwrap();
        loop {
            let (_, values) = c.recv_command().unwrap();
            if values[0].as_str() == Some("deleteStream") {
                return (publish, values, c.log);
            }
        }
    });
    let mut p = Publisher::connect(&config(&url)).unwrap();
    assert_eq!(p.stream_id(), STREAM_ID);
    let nalus = [0, 0, 0, 2, 0x65, 0x88];
    let mut script = Vec::new();
    Metadata::h264_aac(1280, 720, 30.0, 3000, 128, 48000, true).encode(&mut script).unwrap();
    p.set_data_frame(&script).unwrap();
    let bodies = [
        (0, Body::AvcSequenceHeader(&AVCC)),
        (0, Body::AacSequenceHeader(&ASC)),
        (0, Body::AvcNalus { key: true, composition_time: 0, data: &nalus }),
        (21, Body::AacRaw(&[0x21, 0x10])),
        (33, Body::AvcNalus { key: false, composition_time: 33, data: &nalus }),
    ];
    for (t, b) in &bodies {
        p.send_flv(*t, &b.check().unwrap()).unwrap();
    }
    let sent = p.stats().bytes_sent;
    p.close().unwrap();
    let (publish, delete, log) = server.join().unwrap();

    // The chunk size comes first, then the commands in order.
    assert_eq!((log[0].header.type_id, log[0].payload.as_slice()), (msg_type::SET_CHUNK_SIZE, &4096u32.to_be_bytes()[..]));
    assert_eq!(command_names(&log), ["connect", "releaseStream", "FCPublish", "createStream", "publish", "deleteStream"]);
    let connect = amf0::read_all(&log[1].payload).unwrap();
    assert_eq!(connect[1].as_number(), Some(1.0));
    assert_eq!(connect[2].get("app").and_then(Value::as_str), Some("live"));
    assert_eq!(connect[2].get("tcUrl").and_then(Value::as_str), Some(url.as_str()));
    assert_eq!(publish, [s("publish"), Value::Number(0.0), Value::Null, s(KEY), s("live")]);
    assert_eq!(delete[3], Value::Number(f64::from(STREAM_ID)));
    // The client announced the server's window after Set Peer Bandwidth.
    assert!(log.iter().any(|m| m.header.type_id == msg_type::WINDOW_ACK_SIZE && m.payload == WINDOW.to_be_bytes()));

    let media: Vec<_> = log.iter().filter(|m| matches!(m.header.type_id, msg_type::AUDIO | msg_type::VIDEO | msg_type::DATA)).collect();
    assert_eq!(media.len(), 1 + bodies.len());
    let mut frame = Vec::new();
    amf0::write_string(&mut frame, "@setDataFrame").unwrap();
    frame.extend_from_slice(&script);
    assert_eq!((media[0].header.type_id, &media[0].payload), (msg_type::DATA, &frame));
    for (m, (t, b)) in media[1..].iter().zip(&bodies) {
        let mut bytes = Vec::new();
        b.check().unwrap().write_to(&mut bytes).unwrap();
        assert_eq!((m.header.timestamp, m.header.stream_id, &m.payload), (*t, STREAM_ID, &bytes));
        assert_eq!(m.header.type_id, b.tag_type() as u8);
    }
    // Every byte the client counted arrived: the handshake's 3073 and the chunk stream.
    assert!(sent > 3073);
}

#[test]
fn the_client_acknowledges_answers_pings_and_counts_acks() {
    let (done_tx, done_rx) = mpsc::channel();
    let (url, server) = spawn(move |stream| {
        let mut c = Conn::new(Counting { inner: stream, written: 0 });
        c.handshake().unwrap();
        c.accept().unwrap();
        c.control(msg_type::WINDOW_ACK_SIZE, &5000u32.to_be_bytes()).unwrap();
        c.send(csid::DATA, msg_type::DATA, STREAM_ID, &[0x05; 6000]).unwrap();
        let mut ping = user_control::PING_REQUEST.to_be_bytes().to_vec();
        ping.extend_from_slice(&0xDEAD_BEEFu32.to_be_bytes());
        c.control(msg_type::USER_CONTROL, &ping).unwrap();
        // The client has sent at least its 3073 handshake bytes. The second acknowledgement goes
        // back, and the client ignores it.
        for seq in [2000u32, 1000, 2500] {
            c.control(msg_type::ACK, &seq.to_be_bytes()).unwrap();
        }
        let (mut ack, mut pong) = (None, None);
        while ack.is_none() || pong.is_none() {
            let m = c.recv().unwrap();
            match m.header.type_id {
                msg_type::ACK => ack = Some(u32::from_be_bytes(m.payload[..4].try_into().unwrap())),
                msg_type::USER_CONTROL => pong = Some(m.payload.clone()),
                _ => {}
            }
        }
        done_tx.send(()).unwrap();
        (ack.unwrap(), pong.unwrap(), c.s.written)
    });
    let mut p = Publisher::connect(&config(&url)).unwrap();
    while done_rx.try_recv().is_err() {
        p.poll().unwrap();
        std::thread::yield_now();
    }
    let (ack, pong, written) = server.join().unwrap();
    assert!(u64::from(ack) <= written && ack >= 5000, "ack {ack} of {written}");
    assert_eq!(pong, [0, 7, 0xDE, 0xAD, 0xBE, 0xEF]);
    // The server's last acknowledgement, the last thing it sent, is recorded once read. By then the
    // client has counted every byte the server wrote, the handshake's included. The server has
    // closed the connection, so the poll that reads it may also report that.
    while p.stats().bytes_acked < 2500 {
        if let Err(e) = p.poll() {
            assert!(matches!(e, Error::Closed), "{e:?}");
            break;
        }
    }
    assert_eq!(p.stats().bytes_acked, 2500);
    assert_eq!(p.stats().bytes_received, written);
    assert_eq!(p.stats().peer_bandwidth, WINDOW);
}

#[test]
fn one_poll_reads_a_bounded_amount() {
    let (written_tx, written_rx) = mpsc::channel();
    let (url, server) = spawn(move |stream| {
        let mut c = Conn::new(Counting { inner: stream, written: 0 });
        c.handshake().unwrap();
        c.accept().unwrap();
        // Fill the socket buffers without blocking, then stop part way through a message.
        c.s.inner.set_nonblocking(true).unwrap();
        while c.send(csid::DATA, msg_type::DATA, STREAM_ID, &[0x05; 60_000]).is_ok() {}
        c.s.inner.set_nonblocking(false).unwrap();
        written_tx.send(c.s.written).unwrap();
        while c.recv().is_ok() {}
    });
    let mut p = Publisher::connect(&config(&url)).unwrap();
    let after = written_rx.recv().unwrap();
    assert!(after - p.stats().bytes_received > 2 * crate::publisher::POLL_READ_LIMIT as u64, "{after}");
    let start = p.stats().bytes_received;
    p.poll().unwrap();
    let first = p.stats().bytes_received - start;
    assert!(first > 0 && first < crate::publisher::POLL_READ_LIMIT as u64 + 4096, "{first}");
    while p.stats().bytes_received < after {
        p.poll().unwrap();
    }
    assert_eq!(p.stats().bytes_received, after);
    drop(p);
    server.join().unwrap();
}

#[test]
fn peer_bandwidth_limits_follow_their_types() {
    let (done_tx, done_rx) = mpsc::channel();
    let (url, server) = spawn(move |stream| {
        let mut c = Conn::new(stream);
        c.handshake().unwrap();
        c.accept().unwrap();
        // Soft and smaller, soft and larger (kept), dynamic after soft (ignored), hard.
        for (size, limit) in [(1_000_000u32, 1u8), (3_000_000, 1), (4_000_000, 2), (500_000, 0)] {
            let mut p = size.to_be_bytes().to_vec();
            p.push(limit);
            c.control(msg_type::SET_PEER_BANDWIDTH, &p).unwrap();
        }
        loop {
            let m = c.recv().unwrap();
            if m.header.type_id == msg_type::WINDOW_ACK_SIZE && m.payload == 500_000u32.to_be_bytes() {
                break;
            }
        }
        done_tx.send(()).unwrap();
        c.log
            .iter()
            .filter(|m| m.header.type_id == msg_type::WINDOW_ACK_SIZE)
            .map(|m| u32::from_be_bytes(m.payload[..4].try_into().unwrap()))
            .collect::<Vec<_>>()
    });
    let mut p = Publisher::connect(&config(&url)).unwrap();
    while done_rx.try_recv().is_err() {
        p.poll().unwrap();
        std::thread::yield_now();
    }
    // The first, dynamic, limit during setup counts as hard.
    assert_eq!(server.join().unwrap(), [WINDOW, 1_000_000, 500_000]);
    assert_eq!(p.stats().peer_bandwidth, 500_000);
}

#[test]
fn a_refused_publish_names_the_code_and_hides_the_key() {
    let (url, server) = spawn(|stream| {
        let mut c = Conn::new(stream);
        c.handshake().unwrap();
        c.serve_until("publish").unwrap();
        c.status(STREAM_ID, "error", "NetStream.Publish.BadName", &format!("Stream {KEY} is already\u{7} publishing")).unwrap();
        c.recv().ok();
    });
    let e = Publisher::connect(&config(&url)).unwrap_err();
    match &e {
        Error::Rejected { command, code, description } => {
            assert_eq!((*command, code.as_str()), ("publish", "NetStream.Publish.BadName"));
            assert_eq!(description, "Stream <stream key> is already publishing");
        }
        e => panic!("{e:?}"),
    }
    assert_no_key(&e);
    server.join().unwrap();
}

#[test]
fn refused_connect_and_create_stream_are_errors() {
    for step in ["connect", "createStream"] {
        let (url, server) = spawn(move |stream| {
            let mut c = Conn::new(stream);
            c.handshake().unwrap();
            let (_, values) = c.serve_until(step).unwrap();
            c.command(0, &[s("_error"), values[1].clone(), Value::Null, info("error", "NetConnection.Call.Failed", &format!("no {KEY}"))]).unwrap();
            c.recv().ok();
        });
        let e = Publisher::connect(&config(&url)).unwrap_err();
        assert!(matches!(&e, Error::Rejected { command, code, .. } if *command == step && code == "NetConnection.Call.Failed"), "{e:?}");
        assert_no_key(&e);
        server.join().unwrap();
    }
}

#[test]
fn a_refusal_that_repeats_the_url_hides_its_query() {
    const TOKEN: &str = "token=t0ken_abc";
    let (url, server) = spawn(|stream| {
        let mut c = Conn::new(stream);
        c.handshake().unwrap();
        let (_, values) = c.recv_command().unwrap();
        let tc_url = values[2].get("tcUrl").and_then(Value::as_str).unwrap().to_owned();
        assert!(tc_url.ends_with(TOKEN), "{tc_url}");
        c.status(0, "error", "NetConnection.Connect.Rejected", &format!("Bad {tc_url} for {KEY}")).unwrap();
        c.recv().ok();
    });
    let e = Publisher::connect(&config(&format!("{url}?{TOKEN}"))).unwrap_err();
    match &e {
        Error::Rejected { description, .. } => assert_eq!(description, &format!("Bad {url}?<redacted> for <stream key>")),
        e => panic!("{e:?}"),
    }
    assert!(!e.to_string().contains(TOKEN), "{e}");
    assert_no_key(&e);
    server.join().unwrap();
}

#[test]
fn a_short_query_leaves_status_codes_alone() {
    let code = "NetConnection.Connect.Rejected";
    assert_eq!(redact(code, KEY, Some("on")), code);
    assert_eq!(redact("rtmp://h/app?on rejected", KEY, Some("on")), "rtmp://h/app?<redacted> rejected");
    assert_eq!(redact("why? no", KEY, Some("")), "why? no");
}

#[test]
fn a_query_without_the_url_is_hidden() {
    let query = Some("token=t0ken_abc&user=me");
    assert_eq!(redact("bad token=t0ken_abc&user=me", KEY, query), "bad <redacted>");
    assert_eq!(redact("Bad rtmp://h/app?token=t0ken_abc&user=me.", KEY, query), "Bad rtmp://h/app?<redacted>.");
    assert_eq!(redact("(token=t0ken_abc&user=me), again", KEY, query), "(<redacted>), again");
    assert_eq!(redact("xtoken=t0ken_abc&user=me", KEY, query), "xtoken=t0ken_abc&user=me");
    assert_eq!(redact("token=t0ken_abc&user=me_2", KEY, query), "token=t0ken_abc&user=me_2");
}

#[test]
fn redaction_sees_through_control_characters_and_keeps_256_characters() {
    // A control character inside the key is removed before the key is looked for.
    assert_eq!(redact("live_s3cret\u{1}_key!", KEY, None), "<stream key>!");
    // A long text with secrets in it: the result stops at 256 characters.
    let text = format!("{} t=1 {KEY} {}", "x".repeat(10), "y ".repeat(60_000));
    let out = redact(&text, KEY, Some("t=1"));
    assert!(out.starts_with("xxxxxxxxxx <redacted> <stream key> y y"), "{out}");
    assert_eq!(out.chars().count(), 256);
}

#[test]
fn a_status_error_during_setup_is_a_refusal() {
    let (url, server) = spawn(|stream| {
        let mut c = Conn::new(stream);
        c.handshake().unwrap();
        c.recv_command().unwrap();
        c.status(0, "error", "NetConnection.Connect.Rejected", "No.").unwrap();
        c.recv().ok();
    });
    let e = Publisher::connect(&config(&url)).unwrap_err();
    assert!(matches!(&e, Error::Rejected { command, code, .. } if *command == "connect" && code == "NetConnection.Connect.Rejected"), "{e:?}");
    server.join().unwrap();
}

#[test]
fn timeouts_are_checked() {
    for t in [Duration::ZERO, crate::MAX_TIMEOUT + Duration::from_nanos(1), Duration::MAX] {
        let mut c = config("rtmp://127.0.0.1:1/live");
        c.setup_timeout = t;
        assert!(matches!(Publisher::connect(&c), Err(Error::BadConfig(_))), "{t:?}");
        let mut c = config("rtmp://127.0.0.1:1/live");
        c.connect_timeout = t;
        assert!(matches!(Publisher::connect(&c), Err(Error::BadConfig(_))), "{t:?}");
    }
}

#[test]
fn a_status_error_while_publishing_ends_the_stream() {
    let (done_tx, done_rx) = mpsc::channel();
    let (url, server) = spawn(move |stream| {
        let mut c = Conn::new(stream);
        c.handshake().unwrap();
        c.accept().unwrap();
        c.status(STREAM_ID, "error", "NetStream.Publish.Idle", "Stopped.").unwrap();
        // A ping after the status: its answer shows the client read past the status.
        let mut ping = user_control::PING_REQUEST.to_be_bytes().to_vec();
        ping.extend_from_slice(&1u32.to_be_bytes());
        c.control(msg_type::USER_CONTROL, &ping).unwrap();
        while let Ok(m) = c.recv() {
            if m.header.type_id == msg_type::USER_CONTROL {
                done_tx.send(()).ok();
            }
        }
    });
    let mut p = Publisher::connect(&config(&url)).unwrap();
    let e = loop {
        match p.poll() {
            Err(e) => break e,
            Ok(()) if done_rx.try_recv().is_ok() => panic!("the client answered the ping without reporting the status"),
            Ok(()) => std::thread::yield_now(),
        }
    };
    assert!(matches!(&e, Error::Rejected { code, .. } if code == "NetStream.Publish.Idle"), "{e:?}");
    assert!(matches!(p.send(Kind::Audio, 0, &[&[0xAF, 1, 0x21]]), Err(Error::Broken)));
    drop(p);
    server.join().unwrap();
}

#[test]
fn a_server_that_never_answers_times_out() {
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let (url, server) = spawn(move |stream| {
        release_rx.recv().ok();
        drop(stream);
    });
    let mut c = config(&url);
    c.setup_timeout = Duration::from_millis(300);
    let e = Publisher::connect(&c).unwrap_err();
    assert!(matches!(e, Error::Timeout("handshake")), "{e:?}");
    release_tx.send(()).unwrap();
    server.join().unwrap();
}

#[test]
fn a_server_that_trickles_the_handshake_times_out() {
    let (tick_tx, tick_rx) = mpsc::channel::<()>();
    let (url, server) = spawn(move |mut stream: TcpStream| {
        let mut c0c1 = vec![0u8; 1537];
        stream.read_exact(&mut c0c1).unwrap();
        // One byte of S0 and S1 for each read or write the client starts, until the client drops
        // its clock. Returns how many bytes went out.
        let mut sent = 0;
        while tick_rx.recv().is_ok() {
            if sent < 1537 && stream.write_all(&[if sent == 0 { 3 } else { 0 }]).is_ok() {
                sent += 1;
            }
        }
        sent
    });
    // The client's clock moves 100 ms on at each read or write, so each read waits well within
    // the setup timeout, and each tick lets the server send the next byte.
    let start = Instant::now();
    let mut ticks = 0;
    set_test_clock(Some(Box::new(move || {
        ticks += 1;
        let _ = tick_tx.send(());
        start + Duration::from_millis(100) * ticks
    })));
    let mut c = config(&url);
    c.setup_timeout = Duration::from_secs(1);
    let result = Publisher::connect(&c);
    set_test_clock(None);
    let e = result.unwrap_err();
    assert!(matches!(e, Error::Timeout("handshake")), "{e:?}");
    // Without the overall deadline, every byte would restart the read timeout and the client would
    // read all of S0 and S1.
    let sent = server.join().unwrap();
    assert!(sent < 1537, "the server sent {sent} bytes before the client gave up");
}

#[test]
fn a_passed_deadline_stops_reads_and_writes_with_data_waiting() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (stream, _) = listener.accept().unwrap();
    peer.write_all(&[3; 16]).unwrap();
    let mut buf = [0u8; 16];
    let mut open = Socket::new(stream.try_clone().unwrap(), None);
    assert!(open.read(&mut buf).unwrap() > 0);
    // Bytes are waiting, but the deadline has passed, so neither call reaches the socket.
    peer.write_all(&[3; 16]).unwrap();
    let mut late = Socket::new(stream, Some(Instant::now()));
    assert_eq!(late.read(&mut buf).unwrap_err().kind(), io::ErrorKind::TimedOut);
    assert_eq!(late.write(&[0]).unwrap_err().kind(), io::ErrorKind::TimedOut);
}

#[test]
fn acknowledgements_count_on_past_4_gib() {
    const WRAP: u64 = 1 << 32;
    const W: u32 = 2_500_000;
    // Before the first wrap.
    assert_eq!(acked_total(0, 5000, 3000, W), 3000);
    // A window larger than 2 GiB allows a step of that size; without it the step is stale.
    assert_eq!(acked_total(0, 3 << 30, 3 << 30, 3 << 30), 3 << 30);
    assert_eq!(acked_total(0, 3 << 30, 3 << 30, W), 0);
    // After a wrap, a small number counts from the wrap.
    assert_eq!(acked_total(WRAP - 100, WRAP + 500, 200, W), WRAP + 200);
    assert_eq!(acked_total(3 * WRAP - 100, 3 * WRAP + 500, 200, W), 3 * WRAP + 200);
    // A number from just before the wrap, read after it, counts before the wrap.
    assert_eq!(acked_total(WRAP - 500, WRAP + 500, u32::MAX - 99, W), WRAP - 100);
    // A number that would count more bytes than were sent is ignored, a stale one included.
    assert_eq!(acked_total(3000, 5000, 6000, W), 3000);
    assert_eq!(acked_total(WRAP + 2000, WRAP + 3000, 4000, W), WRAP + 2000);
    assert_eq!(acked_total(WRAP + 2000, WRAP + 3000, 1000, W), WRAP + 2000);
}

#[test]
fn a_stale_acknowledgement_is_not_a_wrap_when_more_than_4_gib_is_unacknowledged() {
    const WRAP: u64 = 1 << 32;
    const W: u32 = 2_500_000;
    // More than 4 GiB sent past the last acknowledgement, so the count cannot rule out a wrap.
    let sent = 3 * WRAP;
    // A number 1000 bytes behind the last one is stale, not a step of almost 4 GiB.
    assert_eq!(acked_total(WRAP + 2000, sent, 1000, W), WRAP + 2000);
    assert_eq!(acked_total(WRAP + 2000, sent, 2000u32.wrapping_sub(W), W), WRAP + 2000);
    // With a 3 GiB window the step may be up to 3.5 GiB, and a stale number is still not a wrap.
    assert_eq!(acked_total(WRAP + 2000, sent, 1000, 3 << 30), WRAP + 2000);
    assert_eq!(acked_total(WRAP + 2000, sent, 2000 + (7 << 29), 3 << 30), WRAP + 2000 + (7 << 29));
    assert_eq!(acked_total(WRAP + 2000, sent, 2000 + (7 << 29) + 1, 3 << 30), WRAP + 2000);
    // The next real acknowledgement still counts, across a wrap too.
    assert_eq!(acked_total(WRAP + 2000, sent, 2000 + W, W), WRAP + 2000 + u64::from(W));
    assert_eq!(acked_total(2 * WRAP - 100, sent, W, W), 2 * WRAP + u64::from(W));
}

#[test]
fn an_acknowledgement_due_under_a_larger_window_counts_after_the_window_shrinks() {
    const BIG: u32 = 3 << 30;
    const SMALL: u32 = 1 << 20;
    let sent = 4u64 << 32;
    let mut acks = Acks::default();
    acks.announce(BIG, 100);
    acks.ack(1000, sent);
    assert_eq!(acks.total, 1000);
    // The smaller window goes out after the server's next acknowledgement was already due.
    let shrunk_at = 1000 + u64::from(BIG) + 50;
    acks.announce(SMALL, shrunk_at);
    acks.ack(1000 + BIG, sent);
    assert_eq!(acks.total, 1000 + u64::from(BIG));
    // Once the server has counted the announcement, steps are judged by the smaller window.
    acks.ack(1000 + BIG + 100 + SMALL, sent);
    assert_eq!(acks.total, shrunk_at + 50 + u64::from(SMALL));
    acks.ack((acks.total as u32).wrapping_sub(1000), sent);
    assert_eq!(acks.total, shrunk_at + 50 + u64::from(SMALL));
    acks.ack((acks.total as u32).wrapping_add(3 << 30), sent);
    assert_eq!(acks.total, shrunk_at + 50 + u64::from(SMALL));
}

#[test]
fn only_the_start_status_completes_the_publish() {
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let (url, server) = spawn(move |stream| {
        let mut c = Conn::new(stream);
        c.handshake().unwrap();
        c.serve_until("publish").unwrap();
        c.status(STREAM_ID, "status", "NetStream.Publish.Idle", "Idle.").unwrap();
        release_rx.recv().ok();
    });
    let mut c = config(&url);
    c.setup_timeout = Duration::from_millis(300);
    let e = Publisher::connect(&c).unwrap_err();
    assert!(matches!(e, Error::Timeout("publish")), "{e:?}");
    release_tx.send(()).unwrap();
    server.join().unwrap();
}

#[test]
fn a_server_that_closes_during_setup_is_reported() {
    let (url, server) = spawn(|stream| {
        let mut c = Conn::new(stream);
        c.handshake().unwrap();
        c.recv_command().unwrap();
    });
    let e = Publisher::connect(&config(&url)).unwrap_err();
    assert!(matches!(e, Error::Closed), "{e:?}");
    server.join().unwrap();
}

#[test]
fn a_server_with_another_version_is_refused() {
    let (url, server) = spawn(|mut stream: TcpStream| {
        let mut c0c1 = vec![0u8; 1537];
        stream.read_exact(&mut c0c1).unwrap();
        let mut reply = vec![6u8];
        reply.extend(vec![0u8; 1536 * 2]);
        stream.write_all(&reply).unwrap();
        let mut rest = Vec::new();
        stream.read_to_end(&mut rest).ok();
    });
    let e = Publisher::connect(&config(&url)).unwrap_err();
    assert!(matches!(e, Error::Protocol(_)), "{e:?}");
    server.join().unwrap();
}

#[test]
fn a_write_that_makes_no_progress_is_a_stall() {
    let (stopped_tx, stopped_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let (url, server) = spawn(move |stream| {
        let mut c = Conn::new(stream);
        c.handshake().unwrap();
        c.accept().unwrap();
        // The server reads nothing more until the client has seen the stall.
        stopped_tx.send(()).unwrap();
        release_rx.recv().ok();
    });
    let mut c = config(&url);
    c.stall_timeout = Duration::from_millis(300);
    let mut p = Publisher::connect(&c).unwrap();
    stopped_rx.recv().unwrap();
    let frame = vec![0x41u8; 64 * 1024];
    let mut result = Ok(());
    // Socket buffers hold a few megabytes at most; 64 MiB is far past them.
    for i in 0..1024u32 {
        result = p.send(Kind::Video, i, &[&frame]);
        if result.is_err() {
            break;
        }
    }
    assert!(matches!(result, Err(Error::Stalled(d)) if d == Duration::from_millis(300)), "{result:?}");
    assert!(matches!(p.send(Kind::Video, 0, &[&frame]), Err(Error::Broken)));
    release_tx.send(()).unwrap();
    server.join().unwrap();
}

#[test]
fn a_refused_connection_is_reported() {
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let e = Publisher::connect(&config(&format!("rtmp://127.0.0.1:{port}/live"))).unwrap_err();
    let Error::Connect(tried) = &e else { panic!("{e:?}") };
    assert_eq!(tried.len(), 1, "{e:?}");
    assert_eq!(tried[0].1.kind(), io::ErrorKind::ConnectionRefused, "{e:?}");
}

/// A loopback port that nothing listens on.
fn closed_port() -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap()
}

#[test]
fn every_address_tried_is_reported() {
    let (a, b) = (closed_port(), closed_port());
    let e = Publisher::connect_to(&config(&format!("rtmp://127.0.0.1:{}/live", a.port())), &[a, b]).unwrap_err();
    let Error::Connect(tried) = &e else { panic!("{e:?}") };
    let addresses: Vec<SocketAddr> = tried.iter().map(|(addr, _)| *addr).collect();
    assert_eq!(addresses, [a, b], "both addresses, in the order they were tried");
    assert!(tried.iter().all(|(_, e)| e.kind() == io::ErrorKind::ConnectionRefused), "{e:?}");
    assert!(e.to_string().ends_with("; 1 other address also failed"), "{e}");
}

#[test]
fn the_first_open_address_is_used_after_a_refused_one() {
    let (url, server) = spawn(|stream| {
        let mut c = Conn::new(stream);
        c.handshake().unwrap();
        c.accept().unwrap();
        while c.recv().is_ok() {}
    });
    let open: SocketAddr = url.trim_start_matches("rtmp://").trim_end_matches("/live").parse().unwrap();
    let p = Publisher::connect_to(&config(&url), &[closed_port(), open]).unwrap();
    assert_eq!(p.stream_id(), STREAM_ID);
    p.close().unwrap();
    server.join().unwrap();
}

#[test]
fn no_more_than_four_addresses_are_tried() {
    let addrs: Vec<SocketAddr> = (0..5).map(|_| closed_port()).collect();
    let e = Publisher::connect_to(&config(&format!("rtmp://127.0.0.1:{}/live", addrs[0].port())), &addrs).unwrap_err();
    let Error::Connect(tried) = &e else { panic!("{e:?}") };
    let addresses: Vec<SocketAddr> = tried.iter().map(|(addr, _)| *addr).collect();
    assert_eq!(addresses, addrs[..crate::MAX_ADDRESSES], "the fifth address is not tried");
    // No address at all is a connect error with an empty list.
    let e = Publisher::connect_to(&config("rtmp://127.0.0.1:1/live"), &[]).unwrap_err();
    assert!(matches!(&e, Error::Connect(tried) if tried.is_empty()), "{e:?}");
    assert!(e.to_string().contains("resolved to no address"), "{e}");
}

#[test]
fn the_addresses_tried_alternate_between_ipv6_and_ipv4() {
    let v4 = |n: u8| SocketAddr::from(([192, 0, 2, n], 1935));
    let v6 = |n: u16| SocketAddr::from(([0x2001, 0xdb8, 0, 0, 0, 0, 0, n], 1935));
    // More addresses of the first family than are tried still leave room for the other family.
    assert_eq!(candidates(&[v4(1), v4(2), v4(3), v4(4), v6(1)]), [v4(1), v6(1), v4(2), v4(3)]);
    assert_eq!(candidates(&[v6(1), v6(2), v4(1), v4(2), v6(3)]), [v6(1), v4(1), v6(2), v4(2)]);
    // One family keeps its order.
    assert_eq!(candidates(&[v4(1), v4(2), v4(3), v4(4), v4(5)]), [v4(1), v4(2), v4(3), v4(4)]);
    assert_eq!(candidates(&[v6(1)]), [v6(1)]);
    assert!(candidates(&[]).is_empty());
}

#[test]
fn bad_messages_and_settings_are_refused() {
    let (url, server) = spawn(|stream| {
        let mut c = Conn::new(stream);
        c.handshake().unwrap();
        c.accept().unwrap();
        while c.recv().is_ok() {}
        c.log
    });
    let mut p = Publisher::connect(&config(&url)).unwrap();
    let au = [0, 0, 0, 1, 0x65, 0x88];
    let annexb = Body::AvcAnnexB { key: true, composition_time: 0, data: &au }.check().unwrap();
    assert!(matches!(p.send_flv(0, &annexb), Err(Error::BadInput(_))));
    let big = vec![0u8; crate::MAX_MESSAGE + 1];
    assert!(matches!(p.send(Kind::Video, 0, &[&big]), Err(Error::BadInput(_))));
    // Neither breaks the connection.
    p.send(Kind::Audio, 5, &[&[0xAF, 1, 0x21]]).unwrap();
    assert!(!format!("{p:?}").contains(KEY));
    p.close().unwrap();
    let log = server.join().unwrap();
    assert_eq!(log.iter().filter(|m| m.header.type_id == msg_type::AUDIO).count(), 1);
    assert!(!log.iter().any(|m| m.header.type_id == msg_type::VIDEO));

    let mut c = config(&url);
    assert!(!format!("{c:?}").contains(KEY));
    c.stall_timeout = Duration::ZERO;
    assert!(matches!(Publisher::connect(&c), Err(Error::BadConfig(_))));
    let mut c = config(&url);
    c.chunk_size = 127;
    assert!(matches!(Publisher::connect(&c), Err(Error::BadConfig(_))));
}

#[cfg(feature = "rtmps")]
#[test]
fn rtmps_publishes_to_a_server_whose_root_is_given() {
    let (url, root, server) = spawn_tls(|mut c| {
        c.handshake().unwrap();
        c.accept().unwrap();
        loop {
            let (_, values) = c.recv_command().unwrap();
            if values[0].as_str() == Some("deleteStream") {
                return c.log;
            }
        }
    });
    let mut c = config(&url);
    c.tls_roots = TlsRoots::Only(vec![root]);
    let mut p = Publisher::connect(&c).unwrap();
    p.send_flv(0, &Body::AacSequenceHeader(&ASC).check().unwrap()).unwrap();
    p.close().unwrap();
    let log = server.join().unwrap();
    assert!(log.iter().any(|m| m.header.type_id == msg_type::AUDIO && m.payload == [0xAF, 0, 0x11, 0x90]));
}

#[cfg(not(feature = "rtmps"))]
#[test]
fn rtmps_needs_the_feature() {
    let e = Publisher::connect(&config("rtmps://127.0.0.1:1/live")).unwrap_err();
    assert!(matches!(e, Error::BadConfig(_)), "{e:?}");
}

#[cfg(feature = "rtmps")]
#[test]
fn rtmps_refuses_a_certificate_the_system_does_not_trust() {
    let (url, _root, server) = spawn_tls(|mut c| c.handshake().is_err());
    let e = Publisher::connect(&config(&url)).unwrap_err();
    assert!(matches!(e, Error::Tls(_)), "{e:?}");
    assert!(server.join().unwrap());
}

#[cfg(feature = "rtmps")]
#[test]
fn an_untrusted_certificate_keeps_its_kind() {
    // The server's certificate chains to a different root from the one the client trusts.
    let (url, _root, server) = spawn_tls(|mut c| c.handshake().is_err());
    let mut c = config(&url);
    c.tls_roots = TlsRoots::Only(vec![super::certs::Pki::new().root]);
    let e = Publisher::connect(&c).unwrap_err();
    let Error::Tls(failure) = &e else { panic!("{e:?}") };
    // The rustls error was read from inside the I/O error the handshake gave.
    assert!(matches!(failure.kind, TlsKind::UntrustedCertificate | TlsKind::CertificateNotAccepted), "{failure:?}");
    assert!(server.join().unwrap());
    // The system's check refuses the test root with one of the certificate kinds too.
    let (url, _root, server) = spawn_tls(|mut c| c.handshake().is_err());
    let e = Publisher::connect(&config(&url)).unwrap_err();
    let Error::Tls(failure) = &e else { panic!("{e:?}") };
    assert!(matches!(failure.kind, TlsKind::UntrustedCertificate | TlsKind::CertificateNotAccepted), "{failure:?}");
    assert!(server.join().unwrap());
}

#[test]
fn reach_reports_a_closed_port_and_sends_nothing_to_an_open_one() {
    let closed = closed_port();
    let e = crate::reach(&crate::Reach::new("127.0.0.1", false), closed).unwrap_err();
    let Error::Connect(tried) = &e else { panic!("{e:?}") };
    assert_eq!(tried.len(), 1, "{e:?}");
    assert_eq!((tried[0].0, tried[0].1.kind()), (closed, io::ErrorKind::ConnectionRefused), "{e:?}");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let open = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let mut got = Vec::new();
        s.read_to_end(&mut got).unwrap();
        got
    });
    crate::reach(&crate::Reach::new("127.0.0.1", false), open).unwrap();
    assert!(server.join().unwrap().is_empty(), "the connection closes without a byte sent");
    let mut zero = crate::Reach::new("127.0.0.1", false);
    zero.connect_timeout = Duration::ZERO;
    assert!(matches!(crate::reach(&zero, open), Err(Error::BadConfig(_))));
}

fn addr_of(url: &str) -> SocketAddr {
    url.trim_start_matches("rtmps://").trim_start_matches("rtmp://").trim_end_matches("/live").parse().unwrap()
}

#[test]
fn reach_rtmp_runs_the_handshake_and_sends_no_command() {
    let (url, server) = spawn(|stream| {
        let mut c = Conn::new(stream);
        c.handshake().unwrap();
        // What the client sends after C2, up to its close.
        let mut rest = Vec::new();
        c.s.read_to_end(&mut rest).unwrap();
        rest
    });
    let opened = std::cell::Cell::new(0);
    let answer = crate::reach_rtmp(&crate::Reach::new("127.0.0.1", false), addr_of(&url), || opened.set(opened.get() + 1)).unwrap();
    assert!(matches!(answer, crate::Answer::Rtmp), "{answer:?}");
    assert_eq!(opened.get(), 1, "the open connection is reported once, before the handshake's answer");
    assert!(server.join().unwrap().is_empty(), "no command and no key follow the handshake");
    // A refused connection is an error, as for `reach`, and is not reported as open.
    let closed = closed_port();
    let e = crate::reach_rtmp(&crate::Reach::new("127.0.0.1", false), closed, || opened.set(opened.get() + 1)).unwrap_err();
    assert!(matches!(&e, Error::Connect(tried) if tried.len() == 1), "{e:?}");
    assert_eq!(opened.get(), 1);
    let mut zero = crate::Reach::new("127.0.0.1", false);
    zero.handshake_timeout = Duration::ZERO;
    assert!(matches!(crate::reach_rtmp(&zero, closed, || {}), Err(Error::BadConfig(_))));
}

#[test]
fn reach_rtmp_tells_a_port_that_is_not_an_rtmp_server() {
    // A web server reads the request and answers with an HTTP error, then closes.
    let (url, server) = spawn(|mut s: TcpStream| {
        let mut c0c1 = [0u8; 1537];
        s.read_exact(&mut c0c1).unwrap();
        s.write_all(b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n").unwrap();
    });
    let answer = crate::reach_rtmp(&crate::Reach::new("127.0.0.1", false), addr_of(&url), || {}).unwrap();
    assert!(matches!(answer, crate::Answer::NotRtmp(b'H')), "{answer:?}");
    server.join().unwrap();
    // A server that answers with another RTMP version.
    let (url, server) = spawn(|stream| Conn::new(stream).handshake_with_version(6).is_err());
    let answer = crate::reach_rtmp(&crate::Reach::new("127.0.0.1", false), addr_of(&url), || {}).unwrap();
    assert!(matches!(answer, crate::Answer::NotRtmp(6)), "{answer:?}");
    assert!(server.join().unwrap(), "the client sent no C2 after the wrong version");
    // A server that closes without a byte.
    let (url, server) = spawn(|mut s: TcpStream| {
        let mut c0c1 = [0u8; 1537];
        s.read_exact(&mut c0c1).unwrap();
    });
    let answer = crate::reach_rtmp(&crate::Reach::new("127.0.0.1", false), addr_of(&url), || {}).unwrap();
    assert!(matches!(answer, crate::Answer::Closed { received: 0 }), "{answer:?}");
    server.join().unwrap();
}

#[test]
fn reach_rtmp_reports_a_server_that_does_not_answer_within_the_timeout() {
    // The server reads C0 and C1 and sends nothing back, until the client closes.
    let (url, server) = spawn(|mut s: TcpStream| {
        let mut c0c1 = [0u8; 1537];
        s.read_exact(&mut c0c1).unwrap();
        let mut rest = Vec::new();
        let _ = s.read_to_end(&mut rest);
        rest.len()
    });
    // The write of C0 and C1 sees the real time; every later look at the clock is past the
    // handshake timeout, so the read of S0 and S1 times out at once.
    let start = Instant::now();
    let mut looks = 0;
    set_test_clock(Some(Box::new(move || {
        looks += 1;
        if looks == 1 { start } else { start + Duration::from_secs(3600) }
    })));
    let result = crate::reach_rtmp(&crate::Reach::new("127.0.0.1", false), addr_of(&url), || {});
    set_test_clock(None);
    let answer = result.unwrap();
    assert!(matches!(answer, crate::Answer::NoAnswer { received: 0 }), "{answer:?}");
    assert_eq!(server.join().unwrap(), 0, "the client sent nothing after C0 and C1");
}

#[cfg(feature = "rtmps")]
#[test]
fn reach_rtmp_runs_the_handshake_over_tls() {
    let (url, root, server) = spawn_tls(|mut c| {
        c.handshake().unwrap();
        let mut rest = Vec::new();
        c.s.read_to_end(&mut rest).map(|_| rest).map_err(|e| e.kind())
    });
    let mut r = crate::Reach::new("127.0.0.1", true);
    r.tls_roots = TlsRoots::Only(vec![root]);
    let answer = crate::reach_rtmp(&r, addr_of(&url), || {}).unwrap();
    assert!(matches!(answer, crate::Answer::Rtmp), "{answer:?}");
    // The client ended the session with close_notify after C2, and sent nothing else.
    assert_eq!(server.join().unwrap(), Ok(Vec::new()));
}

#[cfg(feature = "rtmps")]
#[test]
fn reach_completes_the_tls_handshake_without_rtmp_bytes() {
    // The server reads for the RTMP handshake after the TLS one, and gets no bytes: the client ends
    // the session with close_notify (a read of 0 bytes), not a bare TCP close or a reset.
    let (url, root, server) = spawn_tls(|mut c| c.s.read(&mut [0u8; 1]).map_err(|e| e.kind()));
    let addr: SocketAddr = url.trim_start_matches("rtmps://").trim_end_matches("/live").parse().unwrap();
    let mut r = crate::Reach::new("127.0.0.1", true);
    r.tls_roots = TlsRoots::Only(vec![root]);
    crate::reach(&r, addr).unwrap();
    assert_eq!(server.join().unwrap(), Ok(0));
    // A certificate the client does not trust keeps its kind.
    let (url, _root, server) = spawn_tls(|mut c| c.handshake().is_err());
    let addr: SocketAddr = url.trim_start_matches("rtmps://").trim_end_matches("/live").parse().unwrap();
    r.tls_roots = TlsRoots::Only(vec![super::certs::Pki::new().root]);
    let e = crate::reach(&r, addr).unwrap_err();
    let Error::Tls(failure) = &e else { panic!("{e:?}") };
    assert!(matches!(failure.kind, TlsKind::UntrustedCertificate | TlsKind::CertificateNotAccepted), "{failure:?}");
    assert!(server.join().unwrap());
}
