//! Errors. None of them holds the stream key (see the crate documentation).

use std::fmt;
use std::io;
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
    /// No address of the host accepted the TCP connection; the error is from the last one tried.
    Connect(io::Error),
    /// TLS failed: the handshake, the certificate check or the encryption.
    Tls(String),
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
        /// removed and the stream key replaced by `<stream key>`.
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
            Error::Connect(e) => write!(f, "could not connect to the server: {e}"),
            Error::Tls(e) => write!(f, "TLS: {e}"),
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
            Error::Resolve(e) | Error::Connect(e) | Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

/// The longest server description kept in [`Error::Rejected`], in characters.
pub(crate) const MAX_DESCRIPTION: usize = 256;

/// Text from the server made safe for an error: every occurrence of `key` replaced, control
/// characters removed, and at most [`MAX_DESCRIPTION`] characters kept.
pub(crate) fn redact(text: &str, key: &str) -> String {
    let replaced = if key.is_empty() { text.to_owned() } else { text.replace(key, "<stream key>") };
    replaced.chars().filter(|c| !c.is_control()).take(MAX_DESCRIPTION).collect()
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
