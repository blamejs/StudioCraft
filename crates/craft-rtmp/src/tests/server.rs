//! A loopback RTMP server for the tests: the server side of the handshake, the chunk stream, and
//! the answers a publishing server gives.

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread::JoinHandle;

use craft_flv::amf0::{self, Value};

use crate::chunk::{ChunkReader, ChunkWriter, MessageHeader, csid, msg_type, user_control};
use crate::handshake::PACKET;
use crate::{Error, MAX_MESSAGE, Message};

/// The window the server sets with Window Acknowledgement Size and Set Peer Bandwidth.
pub const WINDOW: u32 = 2_500_000;
/// The stream id the server gives in its answer to createStream.
pub const STREAM_ID: u32 = 1;

/// One server connection.
pub struct Conn<S: Read + Write> {
    pub s: S,
    pub reader: ChunkReader,
    pub writer: ChunkWriter,
    /// Every message received, in order.
    pub log: Vec<Message>,
}

impl<S: Read + Write> Conn<S> {
    pub fn new(s: S) -> Self {
        Conn { s, reader: ChunkReader::new(MAX_MESSAGE), writer: ChunkWriter::new(), log: Vec::new() }
    }

    /// Reads C0 and C1, sends S0, S1 and S2 (the echo of C1), and reads C2. Returns C1.
    pub fn handshake(&mut self) -> io::Result<Vec<u8>> {
        self.handshake_with_version(3)
    }

    pub fn handshake_with_version(&mut self, version: u8) -> io::Result<Vec<u8>> {
        let mut c0c1 = vec![0u8; 1 + PACKET];
        self.s.read_exact(&mut c0c1)?;
        let mut reply = vec![version];
        reply.extend_from_slice(&[0, 0, 0, 7, 0, 0, 0, 0]);
        reply.extend((0..PACKET - 8).map(|i| (i * 7) as u8));
        reply.extend_from_slice(&c0c1[1..]);
        self.s.write_all(&reply)?;
        self.s.flush()?;
        let mut c2 = vec![0u8; PACKET];
        self.s.read_exact(&mut c2)?;
        // C2 echoes S1's time and random bytes.
        assert_eq!(&c2[..4], &[0, 0, 0, 7]);
        assert_eq!(&c2[8..], &reply[9..1 + PACKET]);
        Ok(c0c1[1..].to_vec())
    }

    /// The next message, blocking.
    pub fn recv(&mut self) -> Result<Message, Error> {
        let mut buf = [0u8; 16 * 1024];
        loop {
            if let Some(m) = self.reader.next_message()? {
                self.log.push(m.clone());
                return Ok(m);
            }
            match self.s.read(&mut buf) {
                Ok(0) => return Err(Error::Closed),
                Ok(n) => self.reader.push(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(Error::Io(e)),
            }
        }
    }

    /// The next command message's values, skipping other messages.
    pub fn recv_command(&mut self) -> Result<(Message, Vec<Value>), Error> {
        loop {
            let m = self.recv()?;
            if m.header.type_id == msg_type::COMMAND {
                let values = amf0::read_all(&m.payload).unwrap();
                return Ok((m, values));
            }
        }
    }

    pub fn send(&mut self, csid: u32, type_id: u8, stream_id: u32, payload: &[u8]) -> io::Result<()> {
        self.writer.write_message(&mut self.s, &MessageHeader { csid, type_id, stream_id, timestamp: 0 }, &[payload])?;
        self.s.flush()
    }

    pub fn control(&mut self, type_id: u8, payload: &[u8]) -> io::Result<()> {
        self.send(csid::CONTROL, type_id, 0, payload)
    }

    pub fn command(&mut self, stream_id: u32, values: &[Value]) -> io::Result<()> {
        let mut b = Vec::new();
        for v in values {
            v.write(&mut b).unwrap();
        }
        self.send(csid::COMMAND, msg_type::COMMAND, stream_id, &b)
    }

    pub fn status(&mut self, stream_id: u32, level: &str, code: &str, description: &str) -> io::Result<()> {
        self.command(stream_id, &[s("onStatus"), Value::Number(0.0), Value::Null, info(level, code, description)])
    }

    /// Answers everything a publisher sends up to the command named `last`, and returns that
    /// command without answering it.
    pub fn serve_until(&mut self, last: &str) -> Result<(Message, Vec<Value>), Error> {
        loop {
            let (m, values) = self.recv_command()?;
            let name = values[0].as_str().unwrap().to_owned();
            if name == last {
                return Ok((m, values));
            }
            let txn = values[1].clone();
            match name.as_str() {
                "connect" => {
                    self.control(msg_type::WINDOW_ACK_SIZE, &WINDOW.to_be_bytes()).map_err(Error::Io)?;
                    let mut bw = WINDOW.to_be_bytes().to_vec();
                    bw.push(2);
                    self.control(msg_type::SET_PEER_BANDWIDTH, &bw).map_err(Error::Io)?;
                    let mut begin = user_control::STREAM_BEGIN.to_be_bytes().to_vec();
                    begin.extend_from_slice(&0u32.to_be_bytes());
                    self.control(msg_type::USER_CONTROL, &begin).map_err(Error::Io)?;
                    let props = Value::Object(vec![("fmsVer".into(), s("test/1,0"))]);
                    self.command(0, &[s("_result"), txn, props, info("status", "NetConnection.Connect.Success", "Connected.")]).map_err(Error::Io)?;
                }
                "releaseStream" => self.command(0, &[s("_result"), txn, Value::Null, Value::Undefined]).map_err(Error::Io)?,
                "FCPublish" => {}
                "createStream" => self.command(0, &[s("_result"), txn, Value::Null, Value::Number(f64::from(STREAM_ID))]).map_err(Error::Io)?,
                _ => {}
            }
        }
    }

    /// Answers a whole publish, and returns the publish command's values.
    pub fn accept(&mut self) -> Result<Vec<Value>, Error> {
        let (_, values) = self.serve_until("publish")?;
        self.status(STREAM_ID, "status", "NetStream.Publish.Start", "Publishing.").map_err(Error::Io)?;
        Ok(values)
    }
}

pub fn s(v: &str) -> Value {
    Value::String(v.to_owned())
}

pub fn info(level: &str, code: &str, description: &str) -> Value {
    Value::Object(vec![("level".into(), s(level)), ("code".into(), s(code)), ("description".into(), s(description))])
}

/// Starts a server on a loopback port that runs `serve` on the first connection. Returns the
/// `rtmp://` URL with the application name `live` and the server's thread.
pub fn spawn<T: Send + 'static>(serve: impl FnOnce(TcpStream) -> T + Send + 'static) -> (String, JoinHandle<T>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        serve(stream)
    });
    (format!("rtmp://127.0.0.1:{port}/live"), handle)
}

/// A server that runs `serve` on the first connection after a TLS handshake with the test
/// certificate for 127.0.0.1. Returns the `rtmps://` URL, the root certificate, and the thread.
#[cfg(feature = "rtmps")]
pub fn spawn_tls<T: Send + 'static>(
    serve: impl FnOnce(Conn<rustls::StreamOwned<rustls::ServerConnection, TcpStream>>) -> T + Send + 'static,
) -> (String, rustls::pki_types::CertificateDer<'static>, JoinHandle<T>) {
    use std::sync::Arc;

    let pki = super::certs::Pki::new();
    let root = pki.root.clone();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![pki.leaf, pki.root], pki.leaf_key)
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let conn = rustls::ServerConnection::new(Arc::new(config)).unwrap();
        serve(Conn::new(rustls::StreamOwned::new(conn, stream)))
    });
    (format!("rtmps://127.0.0.1:{port}/live"), root, handle)
}
