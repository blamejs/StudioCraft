use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use craft_flv::amf0::{self, Value};
use craft_flv::{Body, Metadata};

#[cfg(feature = "rtmps")]
use super::server::spawn_tls;
use super::server::{Conn, STREAM_ID, WINDOW, info, s, spawn};
#[cfg(feature = "rtmps")]
use crate::TlsRoots;
use crate::chunk::{csid, msg_type, user_control};
use crate::transport::Socket;
use crate::{Config, Error, Kind, Message, Publisher};

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
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let (url, server) = spawn(move |mut stream: TcpStream| {
        let mut c0c1 = vec![0u8; 1537];
        stream.read_exact(&mut c0c1).unwrap();
        // One byte of S0 and S1 every 10 ms, each well within the setup timeout, until the client
        // gives up. Returns how many bytes went out first.
        let mut sent = 0;
        loop {
            if sent < 1537 && stream.write_all(&[if sent == 0 { 3 } else { 0 }]).is_ok() {
                sent += 1;
            }
            match done_rx.recv_timeout(Duration::from_millis(10)) {
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                _ => return sent,
            }
        }
    });
    let mut c = config(&url);
    c.setup_timeout = Duration::from_secs(1);
    let e = Publisher::connect(&c).unwrap_err();
    done_tx.send(()).unwrap();
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
    assert!(matches!(e, Error::Connect(_)), "{e:?}");
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
