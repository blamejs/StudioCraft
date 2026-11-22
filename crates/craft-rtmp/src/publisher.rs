//! Connecting, publishing and closing.

use std::fmt;
use std::io::{self, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use craft_flv::amf0::{self, Value};

use crate::chunk::{ChunkReader, ChunkWriter, MessageHeader, PUBLISH_CHUNK_SIZE, csid, msg_type, user_control};
use crate::error::{read_error, redact, write_error};
use crate::handshake::{self, HandshakeError};
use crate::transport::{Socket, Transport};
use crate::{Endpoint, Error, StreamKey};

/// How long a write may make no progress before the connection counts as broken.
pub const DEFAULT_STALL_TIMEOUT: Duration = Duration::from_secs(5);
/// How long each address gets to accept the TCP connection, by default.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the TLS and RTMP handshakes and the commands up to `publish` may take, by default.
pub const DEFAULT_SETUP_TIMEOUT: Duration = Duration::from_secs(15);
/// How often [`Publisher::send`] reads what the server sent.
pub const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// The longest message the client accepts from the server. Servers send commands, status messages
/// and control messages, which are far shorter.
pub const MAX_SERVER_MESSAGE: usize = 1 << 20;
/// The most bytes one [`Publisher::poll`] reads, so a server that keeps sending cannot hold up a
/// send or fill memory.
pub const POLL_READ_LIMIT: usize = 256 * 1024;
/// The longest timeout a [`Config`] accepts.
pub const MAX_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);
/// The most addresses one connect tries, so a connect waits at most this many times
/// [`Config::connect_timeout`] for TCP. When the name resolved to IPv6 and IPv4 addresses, the
/// addresses tried alternate between the two, starting with the first address's family (RFC 8305
/// section 4), so a host with more addresses of one family than this still gets one of the other.
pub const MAX_ADDRESSES: usize = 4;

/// The `flashVer` value [`Config::new`] sets.
pub const DEFAULT_FLASH_VER: &str = concat!("craft-rtmp/", env!("CARGO_PKG_VERSION"));

const TXN_CONNECT: f64 = 1.0;
const TXN_RELEASE: f64 = 2.0;
const TXN_FC_PUBLISH: f64 = 3.0;
const TXN_CREATE_STREAM: f64 = 4.0;

/// Where to publish and how long each step may take.
#[derive(Clone, Debug)]
pub struct Config {
    /// The server.
    pub endpoint: Endpoint,
    /// The name to publish under.
    pub key: StreamKey,
    /// How long each address the host resolves to gets to accept the TCP connection.
    pub connect_timeout: Duration,
    /// How long the TLS and RTMP handshakes and the commands up to the answer to `publish` may
    /// take in all.
    pub setup_timeout: Duration,
    /// How long a write may make no progress before the connection counts as broken.
    pub stall_timeout: Duration,
    /// The chunk size the client sets for what it sends, from 128 to 16 777 215 bytes.
    pub chunk_size: u32,
    /// The `flashVer` value of the `connect` command, which names the client to the server.
    pub flash_ver: String,
    /// Which certificates an `rtmps://` server's certificate must chain to.
    #[cfg(feature = "rtmps")]
    pub tls_roots: TlsRoots,
}

impl Config {
    /// A configuration for `url` and `key` with the default timeouts (10 s to connect, 15 s for
    /// the setup, 5 s for a stalled write), the 4096-byte chunk size and the system's trust store.
    pub fn new(url: &str, key: &str) -> Result<Config, Error> {
        Ok(Config {
            endpoint: Endpoint::parse(url)?,
            key: StreamKey::new(key)?,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            setup_timeout: DEFAULT_SETUP_TIMEOUT,
            stall_timeout: DEFAULT_STALL_TIMEOUT,
            chunk_size: PUBLISH_CHUNK_SIZE,
            flash_ver: DEFAULT_FLASH_VER.to_owned(),
            #[cfg(feature = "rtmps")]
            tls_roots: TlsRoots::Platform,
        })
    }

    fn check(&self) -> Result<(), Error> {
        let timeouts = [self.connect_timeout, self.setup_timeout, self.stall_timeout];
        if timeouts.iter().any(|t| t.is_zero() || *t > MAX_TIMEOUT) {
            return Err(Error::BadConfig("every timeout must be longer than 0 and at most 24 hours"));
        }
        if !(128..=crate::MAX_MESSAGE as u32).contains(&self.chunk_size) {
            return Err(Error::BadConfig("the chunk size must be from 128 to 16777215 bytes"));
        }
        if self.flash_ver.len() > 0xFFFF {
            return Err(Error::BadConfig("flashVer is longer than 65535 bytes"));
        }
        #[cfg(not(feature = "rtmps"))]
        if self.endpoint.tls() {
            return Err(Error::BadConfig("rtmps:// needs the rtmps feature"));
        }
        Ok(())
    }
}

/// Which certificates an `rtmps://` server's certificate must chain to.
#[cfg(feature = "rtmps")]
#[derive(Clone, Default)]
pub enum TlsRoots {
    /// The system's trust store and certificate checks.
    #[default]
    Platform,
    /// Only these root certificates, such as a private certificate authority's.
    Only(Vec<rustls::pki_types::CertificateDer<'static>>),
}

#[cfg(feature = "rtmps")]
impl fmt::Debug for TlsRoots {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TlsRoots::Platform => f.write_str("Platform"),
            TlsRoots::Only(roots) => write!(f, "Only({} certificates)", roots.len()),
        }
    }
}

/// The kind of media message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// An audio message (type 8).
    Audio,
    /// A video message (type 9).
    Video,
    /// An AMF0 data message (type 18).
    Data,
}

impl Kind {
    fn header(self, stream_id: u32, timestamp: u32) -> MessageHeader {
        let (csid, type_id) = match self {
            Kind::Audio => (csid::AUDIO, msg_type::AUDIO),
            Kind::Video => (csid::VIDEO, msg_type::VIDEO),
            Kind::Data => (csid::DATA, msg_type::DATA),
        };
        MessageHeader { csid, type_id, stream_id, timestamp }
    }

    fn of(tag: craft_flv::TagType) -> Kind {
        match tag {
            craft_flv::TagType::Audio => Kind::Audio,
            craft_flv::TagType::Video => Kind::Video,
            craft_flv::TagType::Script => Kind::Data,
        }
    }
}

/// Counters of one connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stats {
    /// Bytes written to the connection, the handshake included.
    pub bytes_sent: u64,
    /// Bytes read from the connection, the handshake included.
    pub bytes_received: u64,
    /// The byte count of the server's last acknowledgement, or 0 before the first. The server
    /// counts what it received; the counter is 32 bits on the wire, and this value carries on past
    /// 4 GiB. An acknowledgement further ahead of the last one than halfway from the client's
    /// announced window to 4 GiB is taken as stale and not counted; after a smaller window is
    /// announced, the larger one applies until the server has counted the announcement.
    pub bytes_acked: u64,
    /// The window the server asked for with Set Peer Bandwidth, or 0. The client does not hold back
    /// sends to stay within it; `bytes_sent - bytes_acked` shows how far ahead the client is.
    pub peer_bandwidth: u32,
}

/// A connection publishing one stream.
pub struct Publisher {
    transport: Transport,
    out: ChunkWriter,
    inbound: ChunkReader,
    key: StreamKey,
    /// The URL's query string, kept out of errors like the key.
    query: Option<String>,
    stream_id: u32,
    stall_timeout: Duration,
    /// While [`Publisher::connect`] runs, the setup deadline. The transport holds the same deadline
    /// and bounds every read and write by it.
    setup_deadline: Option<Instant>,
    started: Instant,
    last_poll: Instant,
    bytes_sent: u64,
    /// The server's window: the client acknowledges after each this many bytes.
    in_window: u32,
    last_ack_sent: u64,
    /// Bytes of the server's handshake.
    handshake_received: u64,
    /// The last window the client announced with Window Acknowledgement Size.
    out_window_sent: u32,
    peer_bandwidth: u32,
    hard_limit: bool,
    acks: Acks,
    broken: bool,
    /// A reused buffer for commands and control messages.
    scratch: Vec<u8>,
}

impl fmt::Debug for Publisher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Publisher").field("key", &self.key).field("stream_id", &self.stream_id).field("broken", &self.broken).finish()
    }
}

impl Publisher {
    /// Connects and starts publishing: resolves the host, opens TCP (and TLS for `rtmps://`), runs
    /// the handshake, sets the chunk size, and sends `connect`, `releaseStream`, `FCPublish`,
    /// `createStream` and `publish` with the publishing type `live`. It returns once the server
    /// answers `publish` with the status `NetStream.Publish.Start`; an `_error` answer or a status
    /// at level `error` is [`Error::Rejected`], and other statuses are skipped. The answers to
    /// `releaseStream` and `FCPublish` are not waited for, and an `_error` for either is ignored.
    pub fn connect(config: &Config) -> Result<Publisher, Error> {
        config.check()?;
        let host = config.endpoint.host();
        let addrs: Vec<SocketAddr> = (host, config.endpoint.port()).to_socket_addrs().map_err(Error::Resolve)?.collect();
        Publisher::connect_to(config, &addrs)
    }

    /// Connects as [`Publisher::connect`] does, to `addrs` instead of the addresses the host name
    /// resolves to: at most [`MAX_ADDRESSES`] of them, in order within each address family, until
    /// one accepts the TCP connection. TLS still checks the server's certificate against the host name of
    /// [`Config::endpoint`].
    pub fn connect_to(config: &Config, addrs: &[SocketAddr]) -> Result<Publisher, Error> {
        config.check()?;
        let socket = connect_any(addrs, config.connect_timeout)?;
        let started = Instant::now();
        let deadline = started + config.setup_timeout;
        socket.set_nodelay(true).map_err(Error::Io)?;
        let transport = open_transport(socket, config, deadline)?;
        let mut p = Publisher {
            transport,
            out: ChunkWriter::new(),
            inbound: ChunkReader::new(MAX_SERVER_MESSAGE),
            key: config.key.clone(),
            query: config.endpoint.query().map(str::to_owned),
            stream_id: 0,
            stall_timeout: config.stall_timeout,
            setup_deadline: Some(deadline),
            started,
            last_poll: started,
            bytes_sent: 0,
            in_window: 0,
            last_ack_sent: 0,
            handshake_received: 0,
            out_window_sent: 0,
            peer_bandwidth: 0,
            hard_limit: false,
            acks: Acks::default(),
            broken: false,
            scratch: Vec::with_capacity(512),
        };
        p.handshake()?;
        p.send_control(msg_type::SET_CHUNK_SIZE, &config.chunk_size.to_be_bytes())?;
        p.out.set_chunk_size(config.chunk_size)?;
        p.start_publishing(config)?;
        p.setup_deadline = None;
        p.transport.set_deadline(None);
        p.transport.socket().set_write_timeout(Some(config.stall_timeout)).map_err(Error::Io)?;
        p.last_poll = Instant::now();
        Ok(p)
    }

    /// Runs the handshake. The transport's deadline bounds each of its reads and writes.
    fn handshake(&mut self) -> Result<(), Error> {
        let started = self.started;
        let elapsed = || started.elapsed().as_millis() as u32;
        let mut counted = Counted { inner: &mut self.transport, read: 0, written: 0 };
        let result = handshake::client(&mut counted, elapsed);
        let (read, written) = (counted.read, counted.written);
        self.bytes_sent += written;
        // The handshake's bytes count toward acknowledgements and the stats, though the chunk
        // reader never sees them.
        self.handshake_received = read;
        match result {
            Ok(()) => Ok(()),
            Err(HandshakeError::Read(e)) => Err(read_error(e, "handshake")),
            Err(HandshakeError::Write(e)) => Err(read_error(e, "handshake")),
            Err(HandshakeError::Rtmp(e)) => Err(e),
        }
    }

    fn start_publishing(&mut self, config: &Config) -> Result<(), Error> {
        let key = config.key.expose().to_owned();
        // connect (§7.2.1.1).
        let app = config.endpoint.app().to_owned();
        let tc_url = config.endpoint.tc_url();
        let flash_ver = config.flash_ver.clone();
        self.send_command(0, |b| {
            amf0::write_string(b, "connect")?;
            amf0::write_number(b, TXN_CONNECT);
            amf0::write_object_start(b);
            for (name, value) in [("app", app.as_str()), ("type", "nonprivate"), ("flashVer", flash_ver.as_str()), ("tcUrl", tc_url.as_str())] {
                amf0::write_property_name(b, name)?;
                amf0::write_string(b, value)?;
            }
            amf0::write_object_end(b);
            Ok(())
        })?;
        self.wait_for("connect", |p, values| p.result_for(values, TXN_CONNECT, "connect").map(|r| r.map(|_| ())))?;

        // releaseStream and FCPublish, which many servers expect before a publish.
        for (name, txn) in [("releaseStream", TXN_RELEASE), ("FCPublish", TXN_FC_PUBLISH)] {
            self.send_command(0, |b| {
                amf0::write_string(b, name)?;
                amf0::write_number(b, txn);
                amf0::write_null(b);
                amf0::write_string(b, &key)
            })?;
        }

        // createStream (§7.2.1.3): the answer carries the stream id.
        self.send_command(0, |b| {
            amf0::write_string(b, "createStream")?;
            amf0::write_number(b, TXN_CREATE_STREAM);
            amf0::write_null(b);
            Ok(())
        })?;
        let id = self.wait_for("createStream", |p, values| p.result_for(values, TXN_CREATE_STREAM, "createStream"))?;
        let id = match id.get(3).and_then(Value::as_number) {
            Some(n) if n.fract() == 0.0 && (0.0..=f64::from(u32::MAX)).contains(&n) => n as u32,
            _ => return Err(Error::Protocol("the answer to createStream holds no stream id")),
        };
        self.stream_id = id;

        // publish (§7.2.2.6): the server answers with onStatus.
        self.send_command(id, |b| {
            amf0::write_string(b, "publish")?;
            amf0::write_number(b, 0.0);
            amf0::write_null(b);
            amf0::write_string(b, &key)?;
            amf0::write_string(b, "live")
        })?;
        // A status at level error ends every wait as a refusal.
        self.wait_for("publish", |p, values| match status(values) {
            Some(Status { level: "status", code: "NetStream.Publish.Start", .. }) => Some(Ok(())),
            _ => p.result_for(values, 0.0, "publish").and_then(|r| r.err().map(Err)),
        })
    }

    /// The stream id the server gave in its answer to `createStream`.
    pub fn stream_id(&self) -> u32 {
        self.stream_id
    }

    /// The connection's counters.
    pub fn stats(&self) -> Stats {
        Stats {
            bytes_sent: self.bytes_sent,
            bytes_received: self.inbound.received() + self.handshake_received,
            bytes_acked: self.acks.total,
            peer_bandwidth: self.peer_bandwidth,
        }
    }

    /// Sends one checked FLV body as an audio, video or data message. A body of Annex B video is
    /// refused with [`Error::BadInput`]: convert it once with [`craft_flv::annexb_to_nalus`] and
    /// send [`craft_flv::Body::AvcNalus`], which every destination can then share.
    pub fn send_flv(&mut self, timestamp: u32, body: &craft_flv::Checked<'_>) -> Result<(), Error> {
        let Some(parts) = body.parts() else {
            return Err(Error::BadInput("Annex B video must be converted to length-prefixed NAL units before it is sent"));
        };
        self.send(Kind::of(body.tag_type()), timestamp, &[parts.header(), parts.data()])
    }

    /// Sends script data, such as [`craft_flv::Metadata::encode`]'s `onMetaData` bytes, as a data
    /// message with the string `@setDataFrame` before it, which asks the server to keep it and
    /// give it to each viewer that joins.
    pub fn set_data_frame(&mut self, script: &[u8]) -> Result<(), Error> {
        let mut name = Vec::with_capacity(16);
        amf0::write_string(&mut name, "@setDataFrame").map_err(|_| Error::BadInput("the data frame name is not valid"))?;
        self.send(Kind::Data, 0, &[&name, script])
    }

    /// Sends one message whose payload is the concatenation of `parts`, on the published stream.
    /// Every 250 ms it first reads what the server sent ([`Publisher::poll`]).
    ///
    /// A write that fails or makes no progress for the stall timeout gives [`Error::Io`] or
    /// [`Error::Stalled`], and every later call gives [`Error::Broken`].
    pub fn send(&mut self, kind: Kind, timestamp: u32, parts: &[&[u8]]) -> Result<(), Error> {
        if self.broken {
            return Err(Error::Broken);
        }
        if self.last_poll.elapsed() >= POLL_INTERVAL {
            self.poll()?;
        }
        self.write_message(&kind.header(self.stream_id, timestamp), parts)
    }

    /// Reads whatever the server sent, without waiting. It acknowledges the server's bytes once a
    /// window has arrived, answers ping requests, answers Set Peer Bandwidth with Window
    /// Acknowledgement Size when the window changes, and records acknowledgements. A status
    /// message at level `error`, or the server closing the connection, ends the stream
    /// ([`Error::Rejected`], [`Error::Closed`]); every later call then gives [`Error::Broken`].
    pub fn poll(&mut self) -> Result<(), Error> {
        if self.broken {
            return Err(Error::Broken);
        }
        self.last_poll = Instant::now();
        let mut got = Vec::new();
        let inbound = &mut self.inbound;
        let read = self.transport.read_available(POLL_READ_LIMIT, &mut |b| inbound.push(b));
        let closed = match read {
            Ok(c) => c,
            Err(e) => return Err(self.fail(Error::Io(e))),
        };
        loop {
            match self.inbound.next_message() {
                Ok(Some(m)) => got.push(m),
                Ok(None) => break,
                Err(e) => return Err(self.fail(e)),
            }
        }
        for m in got {
            if let Err(e) = self.handle(&m) {
                return Err(self.fail(e));
            }
            if m.header.type_id == msg_type::COMMAND
                && let Ok(values) = amf0::read_all(&m.payload)
                && let Some(Status { level: "error", code, description }) = status(&values)
            {
                let e = self.rejected("the stream", code, description);
                return Err(self.fail(e));
            }
        }
        if let Err(e) = self.ack_if_due() {
            return Err(self.fail(e));
        }
        if closed {
            return Err(self.fail(Error::Closed));
        }
        Ok(())
    }

    /// Ends the stream: sends `deleteStream` (§7.2.2.3), and closes the connection (TLS sends
    /// close_notify first). The server sends no answer, so this does not wait for one.
    pub fn close(mut self) -> Result<(), Error> {
        if self.broken {
            return Err(Error::Broken);
        }
        let id = self.stream_id;
        self.send_command(0, |b| {
            amf0::write_string(b, "deleteStream")?;
            amf0::write_number(b, 0.0);
            amf0::write_null(b);
            amf0::write_number(b, f64::from(id));
            Ok(())
        })?;
        match self.transport.shutdown() {
            // The server may close its side as soon as it reads deleteStream.
            Err(e) if e.kind() == io::ErrorKind::NotConnected => Ok(()),
            r => r.map_err(|e| write_error(e, self.stall_timeout)),
        }
    }

    fn fail(&mut self, e: Error) -> Error {
        self.broken = true;
        e
    }

    fn write_message(&mut self, header: &MessageHeader, parts: &[&[u8]]) -> Result<(), Error> {
        let result = self.out.write_message(&mut self.transport, header, parts).and_then(|n| self.transport.flush().map(|()| n));
        match result {
            Ok(n) => {
                self.bytes_sent += n as u64;
                Ok(())
            }
            Err(e) if e.kind() == io::ErrorKind::InvalidInput => Err(Error::BadInput("the message is longer than 16777215 bytes")),
            Err(e) => {
                let e = if self.setup_deadline.is_some() { read_error(e, "setup") } else { write_error(e, self.stall_timeout) };
                Err(self.fail(e))
            }
        }
    }

    fn send_control(&mut self, type_id: u8, payload: &[u8]) -> Result<(), Error> {
        self.write_message(&MessageHeader { csid: csid::CONTROL, type_id, stream_id: 0, timestamp: 0 }, &[payload])
    }

    fn send_command(&mut self, stream_id: u32, build: impl FnOnce(&mut Vec<u8>) -> Result<(), craft_flv::Error>) -> Result<(), Error> {
        let mut scratch = std::mem::take(&mut self.scratch);
        scratch.clear();
        let built = build(&mut scratch);
        let result = match built {
            Ok(()) => self.write_message(&MessageHeader { csid: csid::COMMAND, type_id: msg_type::COMMAND, stream_id, timestamp: 0 }, &[&scratch]),
            Err(_) => Err(Error::BadInput("a command value is longer than AMF0 allows")),
        };
        self.scratch = scratch;
        result
    }

    /// Reads messages until `pick` returns an answer, handling control messages on the way.
    fn wait_for<T>(&mut self, step: &'static str, mut pick: impl FnMut(&Publisher, &[Value]) -> Option<Result<T, Error>>) -> Result<T, Error> {
        let mut buf = [0u8; 4096];
        loop {
            while let Some(m) = self.inbound.next_message().map_err(|e| self.fail(e))? {
                self.handle(&m).map_err(|e| self.fail(e))?;
                if m.header.type_id == msg_type::COMMAND {
                    let values = amf0::read_all(&m.payload).map_err(|_| self.fail(Error::Protocol("a command message is not valid AMF0")))?;
                    if let Some(Status { level: "error", code, description }) = status(&values) {
                        let e = self.rejected(step, code, description);
                        return Err(self.fail(e));
                    }
                    if let Some(answer) = pick(self, &values) {
                        return answer.map_err(|e| self.fail(e));
                    }
                }
            }
            self.ack_if_due()?;
            match self.transport.read_blocking(&mut buf) {
                Ok(0) => return Err(self.fail(Error::Closed)),
                Ok(n) => self.inbound.push(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    let e = read_error(e, step);
                    return Err(self.fail(e));
                }
            }
        }
    }

    /// The answer `_result` or `_error` with transaction id `txn`, or `None`.
    fn result_for(&self, values: &[Value], txn: f64, command: &'static str) -> Option<Result<Vec<Value>, Error>> {
        let name = values.first()?.as_str()?;
        if values.get(1)?.as_number()? != txn {
            return None;
        }
        match name {
            "_result" => Some(Ok(values.to_vec())),
            "_error" => {
                let info = values.get(3);
                let code = info.and_then(|i| i.get("code")).and_then(Value::as_str).unwrap_or("");
                let description = info.and_then(|i| i.get("description")).and_then(Value::as_str).unwrap_or("");
                Some(Err(self.rejected(command, code, description)))
            }
            _ => None,
        }
    }

    fn rejected(&self, command: &'static str, code: &str, description: &str) -> Error {
        let (key, query) = (self.key.expose(), self.query.as_deref());
        Error::Rejected { command, code: redact(code, key, query), description: redact(description, key, query) }
    }

    /// Acts on a protocol control or user control message. Set Chunk Size and Abort Message were
    /// applied by the chunk reader.
    fn handle(&mut self, m: &crate::Message) -> Result<(), Error> {
        let p = &m.payload;
        let u32_at = |i: usize| p.get(i..i + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
        match m.header.type_id {
            msg_type::ACK => {
                if let Some(seq) = u32_at(0) {
                    self.acks.ack(seq, self.bytes_sent);
                }
            }
            msg_type::WINDOW_ACK_SIZE => {
                if let Some(w) = u32_at(0) {
                    self.in_window = w;
                }
            }
            msg_type::SET_PEER_BANDWIDTH => {
                if let (Some(size), Some(&limit)) = (u32_at(0), p.get(4)) {
                    // §5.4.5: soft keeps the smaller window, and dynamic counts as hard after a
                    // hard limit. A dynamic limit that comes first also counts as hard, since
                    // servers commonly open with one.
                    let first = self.peer_bandwidth == 0;
                    let new = match limit {
                        0 => Some(size),
                        1 if !first => Some(size.min(self.peer_bandwidth)),
                        1 => Some(size),
                        2 if self.hard_limit || first => Some(size),
                        _ => None,
                    };
                    if let Some(w) = new {
                        if limit == 0 || limit == 2 {
                            self.hard_limit = true;
                        } else if limit == 1 {
                            self.hard_limit = false;
                        }
                        self.peer_bandwidth = w;
                        if w != self.out_window_sent {
                            self.send_control(msg_type::WINDOW_ACK_SIZE, &w.to_be_bytes())?;
                            self.out_window_sent = w;
                            self.acks.announce(w, self.bytes_sent);
                        }
                    }
                }
            }
            msg_type::USER_CONTROL => {
                let event = p.get(..2).map(|b| u16::from_be_bytes([b[0], b[1]]));
                if event == Some(user_control::PING_REQUEST)
                    && let Some(ts) = p.get(2..6)
                {
                    let mut reply = [0u8; 6];
                    reply[..2].copy_from_slice(&user_control::PING_RESPONSE.to_be_bytes());
                    reply[2..].copy_from_slice(ts);
                    self.send_control(msg_type::USER_CONTROL, &reply)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Sends an acknowledgement once a window of the server's bytes has arrived since the last.
    fn ack_if_due(&mut self) -> Result<(), Error> {
        let received = self.inbound.received() + self.handshake_received;
        if self.in_window > 0 && received - self.last_ack_sent >= u64::from(self.in_window) {
            self.send_control(msg_type::ACK, &(received as u32).to_be_bytes())?;
            self.last_ack_sent = received;
        }
        Ok(())
    }
}

/// The byte count an Acknowledgement's sequence number `seq` gives, after `acked` bytes were
/// acknowledged and `sent` bytes were sent, with `window` the last window the client announced
/// (0 before the first). The server's count only grows and wraps at 4 GiB, so the number counts
/// forward from the last one. The server acknowledges after each window (§5.4.4), so a step
/// forward of more than halfway from the window to 4 GiB (2 GiB before a window is announced) is a
/// stale number read as a wrap. A stale number, and one that would count more bytes than were
/// sent, leave `acked` as it is.
pub(crate) fn acked_total(acked: u64, sent: u64, seq: u32, window: u32) -> u64 {
    let step = u64::from(seq.wrapping_sub(acked as u32));
    let limit = ((1u64 << 32) + u64::from(window)) / 2;
    let total = acked.saturating_add(step);
    if step <= limit && total <= sent { total } else { acked }
}

/// The server's acknowledgements, counted past 4 GiB.
#[derive(Debug, Default)]
pub(crate) struct Acks {
    /// The byte count of the last acknowledgement accepted.
    pub(crate) total: u64,
    /// The window [`acked_total`] judges a step by: the largest one announced until the server has
    /// counted the bytes that carried the newest one, and then the newest one.
    window: u32,
    /// The newest window announced.
    newest: u32,
    /// The bytes sent up to and including the newest announcement.
    newest_at: u64,
}

impl Acks {
    /// Records a Window Acknowledgement Size sent once `sent` bytes, the message included, were
    /// sent. Acknowledgements due under a larger earlier window may still arrive.
    pub(crate) fn announce(&mut self, window: u32, sent: u64) {
        self.window = self.window.max(window);
        self.newest = window;
        self.newest_at = sent;
    }

    /// Counts an Acknowledgement's sequence number after `sent` bytes were sent.
    pub(crate) fn ack(&mut self, seq: u32, sent: u64) {
        self.total = acked_total(self.total, sent, seq, self.window);
        if self.total >= self.newest_at {
            self.window = self.newest;
        }
    }
}

/// An `onStatus` message's info object.
struct Status<'a> {
    level: &'a str,
    code: &'a str,
    description: &'a str,
}

fn status(values: &[Value]) -> Option<Status<'_>> {
    if values.first()?.as_str()? != "onStatus" {
        return None;
    }
    let info = values.get(3)?;
    Some(Status {
        level: info.get("level").and_then(Value::as_str).unwrap_or(""),
        code: info.get("code").and_then(Value::as_str).unwrap_or(""),
        description: info.get("description").and_then(Value::as_str).unwrap_or(""),
    })
}

/// The addresses a connect tries, at most [`MAX_ADDRESSES`]: `addrs` with the IPv6 and IPv4 ones
/// taken in turn, starting with the family of the first, each family in its own order.
pub(crate) fn candidates(addrs: &[SocketAddr]) -> Vec<SocketAddr> {
    let first_v6 = addrs.first().is_some_and(SocketAddr::is_ipv6);
    let mut same = addrs.iter().filter(|a| a.is_ipv6() == first_v6);
    let mut other = addrs.iter().filter(|a| a.is_ipv6() != first_v6);
    let mut out = Vec::with_capacity(MAX_ADDRESSES.min(addrs.len()));
    while out.len() < MAX_ADDRESSES {
        let a = same.next();
        let b = other.next();
        if a.is_none() && b.is_none() {
            break;
        }
        out.extend(a.into_iter().chain(b).copied());
    }
    out.truncate(MAX_ADDRESSES);
    out
}

fn connect_any(addrs: &[SocketAddr], timeout: Duration) -> Result<TcpStream, Error> {
    let mut tried = Vec::new();
    for a in &candidates(addrs) {
        match TcpStream::connect_timeout(a, timeout) {
            Ok(s) => return Ok(s),
            Err(e) => tried.push((*a, e)),
        }
    }
    Err(Error::Connect(tried))
}

#[cfg(not(feature = "rtmps"))]
fn open_transport(socket: TcpStream, _config: &Config, deadline: Instant) -> Result<Transport, Error> {
    Ok(Transport::Tcp(Socket::new(socket, Some(deadline))))
}

#[cfg(feature = "rtmps")]
fn open_transport(socket: TcpStream, config: &Config, deadline: Instant) -> Result<Transport, Error> {
    use std::sync::Arc;

    use crate::error::TlsFailure;

    let mut sock = Socket::new(socket, Some(deadline));
    if !config.endpoint.tls() {
        return Ok(Transport::Tcp(sock));
    }
    let tls = |e: rustls::Error| Error::Tls(TlsFailure::of(&e));
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone()).with_safe_default_protocol_versions().map_err(tls)?;
    let client = match &config.tls_roots {
        TlsRoots::Platform => {
            let verifier = rustls_platform_verifier::Verifier::new(provider).map_err(tls)?;
            builder.dangerous().with_custom_certificate_verifier(Arc::new(verifier)).with_no_client_auth()
        }
        TlsRoots::Only(certs) => {
            let mut roots = rustls::RootCertStore::empty();
            for c in certs {
                roots.add(c.clone()).map_err(tls)?;
            }
            builder.with_root_certificates(roots).with_no_client_auth()
        }
    };
    let name = rustls::pki_types::ServerName::try_from(config.endpoint.host().to_owned())
        .map_err(|_| Error::BadUrl("the host is not a valid TLS server name"))?;
    let mut conn = rustls::ClientConnection::new(Arc::new(client), name).map_err(tls)?;
    conn.set_buffer_limit(None);
    while conn.is_handshaking() {
        match conn.complete_io(&mut sock) {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::InvalidData => return Err(Error::Tls(TlsFailure::of_io(&e))),
            Err(e) => return Err(read_error(e, "the TLS handshake")),
        }
    }
    Ok(Transport::Tls(Box::new(rustls::StreamOwned::new(conn, sock))))
}

/// Counts the bytes that pass through, for the handshake.
struct Counted<'a> {
    inner: &'a mut Transport,
    read: u64,
    written: u64,
}

impl io::Read for Counted<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read_blocking(buf)?;
        self.read += n as u64;
        Ok(n)
    }
}

impl Write for Counted<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
