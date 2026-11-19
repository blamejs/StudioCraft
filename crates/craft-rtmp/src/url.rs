//! The server URL and the stream key.

use std::fmt;

use crate::Error;

/// The default port of `rtmp://`.
pub const RTMP_PORT: u16 = 1935;
/// The default port of `rtmps://`.
pub const RTMPS_PORT: u16 = 443;

/// A server URL, `rtmp://host[:port]/app` or `rtmps://host[:port]/app`, taken apart.
///
/// The application name is the whole path without its leading and trailing `/`, with any query
/// string, as `live2`, `rtmp` or `app/instance?token=abc`. The stream key is not part of it: it
/// goes in a [`StreamKey`]. `Debug` output leaves out the query string, which may hold a token.
#[derive(Clone, PartialEq, Eq)]
pub struct Endpoint {
    tls: bool,
    host: String,
    port: u16,
    explicit_port: bool,
    app: String,
}

impl Endpoint {
    /// Parses a server URL. The scheme is `rtmp` or `rtmps` in any case. A host is a name, an IPv4
    /// address or an IPv6 address in brackets. Refused: other schemes, a user name or password
    /// (`user@host`), an empty host, a port of 0 or past 65 535, an empty application name, and
    /// spaces or control characters anywhere.
    pub fn parse(url: &str) -> Result<Endpoint, Error> {
        if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(Error::BadUrl("it holds a space or a control character"));
        }
        let (scheme, rest) = url.split_once("://").ok_or(Error::BadUrl("it does not start with rtmp:// or rtmps://"))?;
        let tls = if scheme.eq_ignore_ascii_case("rtmp") {
            false
        } else if scheme.eq_ignore_ascii_case("rtmps") {
            true
        } else {
            return Err(Error::BadUrl("only rtmp:// and rtmps:// are supported"));
        };
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i + 1..]),
            None => (rest, ""),
        };
        if authority.contains('@') {
            return Err(Error::BadUrl("a user name or password in the URL is not supported"));
        }
        let (host, port) = split_host_port(authority)?;
        if host.is_empty() {
            return Err(Error::BadUrl("the host is empty"));
        }
        let app = path.trim_matches('/');
        if app.is_empty() || app.starts_with('?') {
            return Err(Error::BadUrl("the application name after the host is empty"));
        }
        let explicit_port = port.is_some();
        let port = port.unwrap_or(if tls { RTMPS_PORT } else { RTMP_PORT });
        Ok(Endpoint { tls, host: host.to_owned(), port, explicit_port, app: app.to_owned() })
    }

    /// Whether the URL is `rtmps://`.
    pub fn tls(&self) -> bool {
        self.tls
    }

    /// The host, without brackets around an IPv6 address.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The port, given or the scheme's default.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The application name sent in `connect`.
    pub fn app(&self) -> &str {
        &self.app
    }

    /// The query string after `?` in the application name, which may hold a token, or `None`.
    pub fn query(&self) -> Option<&str> {
        self.app.split_once('?').map(|(_, query)| query)
    }

    /// The `tcUrl` sent in `connect`: the scheme, the host, the port when the URL gave one, and
    /// the application name.
    pub fn tc_url(&self) -> String {
        let scheme = if self.tls { "rtmps" } else { "rtmp" };
        let host = if self.host.contains(':') { format!("[{}]", self.host) } else { self.host.clone() };
        if self.explicit_port { format!("{scheme}://{host}:{}/{}", self.port, self.app) } else { format!("{scheme}://{host}/{}", self.app) }
    }
}

impl fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let app = match self.app.split_once('?') {
            Some((path, _)) => format!("{path}?<redacted>"),
            None => self.app.clone(),
        };
        f.debug_struct("Endpoint").field("tls", &self.tls).field("host", &self.host).field("port", &self.port).field("app", &app).finish()
    }
}

fn split_host_port(authority: &str) -> Result<(&str, Option<u16>), Error> {
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let (host, after) = rest.split_once(']').ok_or(Error::BadUrl("an IPv6 address has no closing bracket"))?;
        match after {
            "" => (host, None),
            p => (host, Some(p.strip_prefix(':').ok_or(Error::BadUrl("text follows the IPv6 address"))?)),
        }
    } else {
        match authority.rsplit_once(':') {
            Some((h, _)) if h.contains(':') => return Err(Error::BadUrl("an IPv6 address must be in brackets")),
            Some((h, p)) => (h, Some(p)),
            None => (authority, None),
        }
    };
    let port = match port {
        None => None,
        Some(p) => match p.parse::<u16>() {
            Ok(0) | Err(_) => return Err(Error::BadUrl("the port is not a number from 1 to 65535")),
            Ok(n) => Some(n),
        },
    };
    Ok((host, port))
}

/// The most bytes a stream key may have: it is sent as an AMF0 string.
pub const MAX_KEY_BYTES: usize = 0xFFFF;

/// A stream key: the name a stream is published under, which the service treats as a password.
/// `Debug` prints `StreamKey(<redacted>)`, and there is no `Display`.
#[derive(Clone, PartialEq, Eq)]
pub struct StreamKey(String);

impl StreamKey {
    /// Takes a key. Refused: an empty key, one over 65 535 bytes, and one holding a space or a
    /// control character. Other characters, such as `?` and `&` in keys that carry options, are
    /// kept.
    pub fn new(key: impl Into<String>) -> Result<StreamKey, Error> {
        let key = key.into();
        if key.is_empty() {
            return Err(Error::BadKey("it is empty"));
        }
        if key.len() > MAX_KEY_BYTES {
            return Err(Error::BadKey("it is longer than 65535 bytes"));
        }
        if key.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(Error::BadKey("it holds a space or a control character"));
        }
        Ok(StreamKey(key))
    }

    /// The key, to send it. Do not log it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for StreamKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("StreamKey(<redacted>)")
    }
}
