//! A client of a running app's control channel (`docs/control-protocol.md`).

use std::io::{BufReader, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::time::Duration;

use craft_control_protocol::ReadFailure;
use serde_json::{Value, json};

use crate::backend::{Backend, McpError};

/// The environment variable holding the session token, as the app reads it.
pub const TOKEN_ENV: &str = "STUDIOCRAFT_CONTROL_TOKEN";
/// Longest reply line read from the app (window screenshots are large).
pub const MAX_REPLY: usize = 256 << 20;
/// How long connecting may take.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(800);
/// How long a reply may take. The app answers within 30 seconds or reports that it did not.
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// The app's control address for `target`: a port (`9876`) or a loopback address with a port
/// (`127.0.0.1:9876`, `localhost:9876`, `[::1]:9876`). Other hosts are refused.
pub fn parse_target(target: &str) -> Result<SocketAddr, McpError> {
    let t = target.trim();
    let addr = if !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit()) {
        let port = t.parse::<u16>().map_err(|_| McpError::BadTarget(format!("`{t}` is not a port from 1 to 65535")))?;
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
    } else if let Some(port) = t.strip_prefix("localhost:") {
        let port = port.parse::<u16>().map_err(|_| McpError::BadTarget(format!("`{port}` is not a port from 1 to 65535")))?;
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
    } else {
        let addr: SocketAddr =
            t.parse().map_err(|_| McpError::BadTarget(format!("`{t}` is not a port or a loopback address such as 127.0.0.1:9876")))?;
        if !addr.ip().is_loopback() {
            return Err(McpError::BadTarget(format!("the app is reached on this computer only (127.0.0.1), not at `{t}`")));
        }
        addr
    };
    if addr.port() == 0 {
        return Err(McpError::BadTarget("the port must be from 1 to 65535".into()));
    }
    Ok(addr)
}

/// Methods that only read, so sending one again after a lost reply is harmless.
fn is_read_only(method: &str, params: &Value) -> bool {
    match method {
        "document.inspect" | "engine.commands" | "commands" | "state" | "render.preview" | "ui.inspect" | "ui.menu.list" | "ui.screenshot" => true,
        "engine.execute" | "run" => {
            matches!(
                params.get("command").and_then(Value::as_str),
                Some(
                    "app.links"
                        | "app.info"
                        | "plugin.list"
                        | "source.schema"
                        | "app_audio.list"
                        | "app_audio.status"
                        | "app_audio.lanes"
                        | "video.displays"
                        | "recordings.list"
                )
            )
        }
        _ => false,
    }
}

/// Why one exchange failed.
/// Whether the app still holds the connection open and has sent nothing unasked on it.
fn open_and_idle(stream: &TcpStream, reader: &BufReader<TcpStream>) -> bool {
    if !reader.buffer().is_empty() || stream.set_nonblocking(true).is_err() {
        return false;
    }
    let idle = matches!(stream.peek(&mut [0u8; 1]), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock);
    stream.set_nonblocking(false).is_ok() && idle
}

enum Failure {
    /// The request did not reach the app.
    NotSent(McpError),
    /// The request was sent and no reply came back.
    Lost(String),
    /// The app answered, with an error or something unreadable.
    Answered(McpError),
}

/// A running app's control channel. Connects on the first call, and again after a lost
/// connection.
pub struct Remote {
    addr: SocketAddr,
    token: String,
    conn: Option<(TcpStream, BufReader<TcpStream>)>,
    next: u64,
}

impl Remote {
    /// A client for the app at `target` (see [`parse_target`]) with the session `token`.
    pub fn new(target: &str, token: &str) -> Result<Remote, McpError> {
        let addr = parse_target(target)?;
        if !craft_control_protocol::valid_token(token) {
            return Err(McpError::Token(format!("the session token must be 64 hexadecimal characters ({TOKEN_ENV})")));
        }
        Ok(Remote { addr, token: token.to_owned(), conn: None, next: 1 })
    }

    /// A client with the token from [`TOKEN_ENV`].
    pub fn from_env(target: &str) -> Result<Remote, McpError> {
        let token = std::env::var(TOKEN_ENV).map_err(|_| McpError::Token(format!("set {TOKEN_ENV} to the token the app was started with")))?;
        Remote::new(target, &token)
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    fn connect(&mut self) -> Result<(), McpError> {
        if self.conn.as_ref().is_some_and(|(w, r)| !open_and_idle(w, r)) {
            // A connection the app closed, or one with an unread line, is replaced before
            // anything is sent on it.
            self.conn = None;
        }
        if self.conn.is_some() {
            return Ok(());
        }
        let stream =
            TcpStream::connect_timeout(&self.addr, CONNECT_TIMEOUT).map_err(|source| McpError::Connect { addr: self.addr.to_string(), source })?;
        stream.set_read_timeout(Some(READ_TIMEOUT)).map_err(|e| McpError::Io(e.to_string()))?;
        let reader = stream.try_clone().map_err(|e| McpError::Io(e.to_string()))?;
        self.conn = Some((stream, BufReader::new(reader)));
        Ok(())
    }

    /// One request and its reply.
    fn exchange(&mut self, method: &str, params: &Value) -> Result<Value, Failure> {
        self.connect().map_err(Failure::NotSent)?;
        let line = craft_control_protocol::request_line(self.next, Some(&self.token), method, params);
        self.next = self.next.wrapping_add(1);
        let Some((writer, reader)) = self.conn.as_mut() else {
            return Err(Failure::NotSent(McpError::Io("not connected".into())));
        };
        writer.write_all(line.as_bytes()).and_then(|()| writer.flush()).map_err(|e| Failure::NotSent(McpError::Io(e.to_string())))?;
        let mut buf = Vec::new();
        match craft_control_protocol::read_bounded_line(reader, &mut buf, MAX_REPLY) {
            Ok(true) => {}
            Ok(false) => return Err(Failure::Lost("the app closed the connection".into())),
            Err(ReadFailure::Timeout) => return Err(Failure::Lost(format!("no reply within {} s", READ_TIMEOUT.as_secs()))),
            Err(ReadFailure::Rejected(e)) => return Err(Failure::Lost(format!("the reply could not be read: {e}"))),
        }
        let reply: Value = match serde_json::from_slice(&buf) {
            Ok(v) => v,
            Err(e) => {
                // Later replies on this connection can't be matched to their requests.
                self.conn = None;
                return Err(Failure::Answered(McpError::App(format!("the app's reply is not JSON: {e}"))));
            }
        };
        craft_control_protocol::reply_result(&reply).map_err(|e| Failure::Answered(McpError::App(e)))
    }

    fn send(&mut self, method: &str, params: &Value) -> Result<Value, McpError> {
        match self.exchange(method, params) {
            Ok(v) => Ok(v),
            Err(Failure::Answered(e)) => Err(e),
            Err(Failure::NotSent(e)) => {
                // A write that failed may still have reached the app, so only a request that
                // changes nothing is sent again.
                let reused = self.conn.take().is_some();
                if !reused {
                    return Err(e);
                }
                if !is_read_only(method, params) {
                    return Err(McpError::Lost { method: method.to_owned(), reason: e.to_string() });
                }
                self.exchange(method, params).map_err(Remote::final_error(method))
            }
            Err(Failure::Lost(reason)) => {
                self.conn = None;
                if !is_read_only(method, params) {
                    return Err(McpError::Lost { method: method.to_owned(), reason });
                }
                self.exchange(method, params).map_err(Remote::final_error(method))
            }
        }
    }

    fn final_error(method: &str) -> impl Fn(Failure) -> McpError + '_ {
        move |f| match f {
            Failure::NotSent(e) | Failure::Answered(e) => e,
            Failure::Lost(reason) => McpError::Lost { method: method.to_owned(), reason },
        }
    }
}

impl Backend for Remote {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, McpError> {
        // The window imports and exports collection files with its own commands.
        let (method, params) = match method {
            "collection.open" => ("engine.execute", json!({"command": "collection.import", "params": params})),
            "collection.save" => ("engine.execute", json!({"command": "collection.export", "params": params})),
            m => (m, params),
        };
        self.send(method, &params)
    }

    fn has_ui(&self) -> bool {
        true
    }

    fn describe(&self) -> String {
        format!("StudioCraft app at {}", self.addr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_are_loopback_ports() {
        assert_eq!(parse_target("9876").unwrap().to_string(), "127.0.0.1:9876");
        assert_eq!(parse_target("127.0.0.1:9876").unwrap().to_string(), "127.0.0.1:9876");
        assert_eq!(parse_target("localhost:9876").unwrap().to_string(), "127.0.0.1:9876");
        assert_eq!(parse_target("[::1]:9876").unwrap().to_string(), "[::1]:9876");
        for bad in ["0", "70000", "", "10.0.0.5:9876", "192.168.1.2:80", "example.com:9876", "127.0.0.1:0", "127.0.0.1"] {
            assert!(matches!(parse_target(bad), Err(McpError::BadTarget(_))), "{bad}");
        }
    }

    #[test]
    fn read_only_methods_are_listed() {
        assert!(is_read_only("document.inspect", &json!({})));
        assert!(is_read_only("engine.execute", &json!({"command": "app.links"})));
        assert!(is_read_only("engine.execute", &json!({"command": "source.schema"})));
        assert!(is_read_only("engine.execute", &json!({"command": "app_audio.list"})));
        assert!(is_read_only("run", &json!({"command": "app_audio.status"})));
        assert!(is_read_only("run", &json!({"command": "app_audio.lanes"})));
        assert!(!is_read_only("run", &json!({"command": "app_audio.route"})), "routing changes the collection");
        assert!(is_read_only("engine.execute", &json!({"command": "video.displays"})));
        assert!(!is_read_only("engine.execute", &json!({"command": "video.canvas_from_display"})));
        assert!(is_read_only("run", &json!({"command": "recordings.list"})));
        assert!(!is_read_only("engine.execute", &json!({"command": "scene.add"})));
        assert!(!is_read_only("ui.click", &json!({})));
    }
}
