//! Errors. None of them holds the stream key (see the crate documentation).

use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::time::Duration;

/// An error from connecting or publishing.
#[derive(Debug)]
pub enum Error {
    /// The server URL is not a usable `rtmp://` or `rtmps://` URL.
    BadUrl(&'static str),
    /// The stream key is empty, too long, or holds a space or control character.
    BadKey(&'static str),
    /// A setting in [`crate::Config`] is out of range.
    BadConfig(&'static str),
    /// A message passed in cannot be sent: too long, or Annex B video that was not converted.
    BadInput(&'static str),
    /// The host name did not resolve.
    Resolve(io::Error),
    /// No address of the host accepted the TCP connection: each address tried, in order, with the
    /// error it gave. At most [`crate::MAX_ADDRESSES`] addresses are tried; the list is empty when
    /// the name resolved to no address.
    Connect(Vec<(SocketAddr, io::Error)>),
    /// TLS failed: the handshake, the certificate check or the encryption.
    Tls(TlsFailure),
    /// Reading from or writing to the connection failed.
    Io(io::Error),
    /// A write made no progress for this long, so the connection is treated as broken.
    Stalled(Duration),
    /// The server did not finish this step within the setup timeout.
    Timeout(&'static str),
    /// The server closed the connection.
    Closed,
    /// The server sent something the protocol does not allow here.
    Protocol(&'static str),
    /// The server refused a command, with `_error` or a status message at level `error`.
    Rejected {
        /// The command or step that was refused, such as `publish`.
        command: &'static str,
        /// The status code the server sent, such as `NetStream.Publish.BadName`, or empty.
        code: String,
        /// The server's description, shortened to 256 characters, with control characters
        /// removed, the stream key replaced by `<stream key>`, and the URL's query string replaced
        /// by `<redacted>`.
        description: String,
    },
    /// An earlier write failed or stalled, or the server ended the stream, so the connection is
    /// out of step. Connect again with a new [`crate::Publisher`].
    Broken,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::BadUrl(s) => write!(f, "the server URL is not valid: {s}"),
            Error::BadKey(s) => write!(f, "the stream key is not valid: {s}"),
            Error::BadConfig(s) => write!(f, "{s}"),
            Error::BadInput(s) => write!(f, "{s}"),
            Error::Resolve(e) => write!(f, "the server's name did not resolve: {e}"),
            Error::Connect(tried) => {
                match tried.last() {
                    Some((_, e)) => write!(f, "could not connect to the server: {e}")?,
                    None => write!(f, "could not connect to the server: the name resolved to no address")?,
                }
                match tried.len() {
                    0 | 1 => Ok(()),
                    2 => write!(f, "; 1 other address also failed"),
                    n => write!(f, "; {} other addresses also failed", n - 1),
                }
            }
            Error::Tls(e) => write!(f, "TLS: {}", e.message),
            Error::Io(e) => write!(f, "the connection failed: {e}"),
            Error::Stalled(d) => write!(f, "the server accepted no data for {:.1} s", d.as_secs_f64()),
            Error::Timeout(step) => write!(f, "the server did not finish {step} in time"),
            Error::Closed => write!(f, "the server closed the connection"),
            Error::Protocol(s) => write!(f, "the server broke the RTMP protocol: {s}"),
            Error::Rejected { command, code, description } => {
                write!(f, "the server refused {command}")?;
                if !code.is_empty() {
                    write!(f, ": {code}")?;
                }
                if !description.is_empty() {
                    write!(f, ": {description}")?;
                }
                Ok(())
            }
            Error::Broken => write!(f, "an earlier send failed, so the connection is closed"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Resolve(e) | Error::Io(e) => Some(e),
            Error::Connect(tried) => tried.last().map(|(_, e)| e as &(dyn std::error::Error + 'static)),
            _ => None,
        }
    }
}

/// What kind of TLS failure an [`Error::Tls`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TlsKind {
    /// The server's certificate does not chain to a trusted root.
    UntrustedCertificate,
    /// The server's certificate is not valid for the host name.
    NameMismatch,
    /// The server's certificate has expired.
    Expired,
    /// The certificate check refused the server's certificate for another reason, or for a reason
    /// the system's certificate check does not report.
    CertificateNotAccepted,
    /// The server ended the handshake with a fatal alert.
    HandshakeAlert,
    /// Any other TLS failure.
    Other,
}

/// A TLS failure: its kind and the TLS library's description.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TlsFailure {
    /// What kind of failure it is.
    pub kind: TlsKind,
    /// The TLS library's description.
    pub message: String,
}

#[cfg(feature = "rtmps")]
impl TlsFailure {
    /// The failure a rustls error describes.
    pub(crate) fn of(e: &rustls::Error) -> TlsFailure {
        use rustls::CertificateError as C;
        let kind = match e {
            rustls::Error::InvalidCertificate(c) => match c {
                C::UnknownIssuer => TlsKind::UntrustedCertificate,
                C::NotValidForName | C::NotValidForNameContext { .. } => TlsKind::NameMismatch,
                C::Expired | C::ExpiredContext { .. } => TlsKind::Expired,
                _ => TlsKind::CertificateNotAccepted,
            },
            rustls::Error::AlertReceived(_) => TlsKind::HandshakeAlert,
            _ => TlsKind::Other,
        };
        TlsFailure { kind, message: e.to_string() }
    }

    /// The failure an I/O error from the TLS handshake carries: rustls wraps its own error in the
    /// I/O error, and anything else is [`TlsKind::Other`].
    pub(crate) fn of_io(e: &io::Error) -> TlsFailure {
        match e.get_ref().and_then(|inner| inner.downcast_ref::<rustls::Error>()) {
            Some(tls) => TlsFailure::of(tls),
            None => TlsFailure { kind: TlsKind::Other, message: e.to_string() },
        }
    }
}

/// The longest server description kept in [`Error::Rejected`], in characters.
pub(crate) const MAX_DESCRIPTION: usize = 256;

/// Text from the server made safe for an error: control characters removed, every occurrence of
/// `key` replaced with `<stream key>`, the URL's `query` string replaced with `<redacted>` where it
/// stands apart, and at most [`MAX_DESCRIPTION`] characters kept. The query stands apart when no
/// letter, digit or one of `-_~%` touches it, so a short query such as `on` leaves
/// `NetConnection.Connect.Rejected` as it is, and a query followed by `.` or `,` is still replaced.
///
/// One pass reads the text and stops once [`MAX_DESCRIPTION`] characters are kept. At each
/// position the longer of the two secrets that matches is replaced, so the work is the text's
/// length plus [`MAX_DESCRIPTION`] times the two secrets' length.
pub(crate) fn redact(text: &str, key: &str, query: Option<&str>) -> String {
    let clean = |s: &str| -> String { s.chars().filter(|c| !c.is_control()).collect() };
    let mut secrets = [(clean(key), "<stream key>", false), (clean(query.unwrap_or("")), "<redacted>", true)];
    secrets.sort_by_key(|(secret, _, _)| std::cmp::Reverse(secret.len()));
    let text = clean(text);
    let joins = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || "-_~%".contains(c));
    let mut out = String::new();
    let mut kept = 0;
    let mut i = 0;
    while kept < MAX_DESCRIPTION
        && let Some(c) = text.get(i..).and_then(|rest| rest.chars().next())
    {
        let rest = &text[i..];
        let before = text[..i].chars().next_back();
        let found = secrets.iter().find(|(secret, _, apart)| {
            !secret.is_empty() && rest.starts_with(secret.as_str()) && !(*apart && (joins(before) || joins(rest[secret.len()..].chars().next())))
        });
        if let Some((secret, mark, _)) = found {
            out.push_str(mark);
            kept += mark.chars().count();
            i += secret.len();
        } else {
            out.push(c);
            kept += 1;
            i += c.len_utf8();
        }
    }
    out.chars().take(MAX_DESCRIPTION).collect()
}

/// An error from a write: a timed-out write is a stall.
pub(crate) fn write_error(e: io::Error, stall: Duration) -> Error {
    match e.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => Error::Stalled(stall),
        _ => Error::Io(e),
    }
}

/// An error from a blocking read during `step`: a timed-out read is a timeout, and end of file is
/// [`Error::Closed`].
pub(crate) fn read_error(e: io::Error, step: &'static str) -> Error {
    match e.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => Error::Timeout(step),
        io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted => Error::Closed,
        _ => Error::Io(e),
    }
}
