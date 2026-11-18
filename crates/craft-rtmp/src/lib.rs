//! A blocking RTMP and RTMPS publish client (Real-Time Messaging Protocol specification 1.0).
//!
//! - [`Publisher::connect`] opens a TCP connection (and TLS for `rtmps://`), runs the handshake,
//!   sets the outgoing chunk size to 4096 bytes, and sends `connect`, `releaseStream`,
//!   `FCPublish`, `createStream` and `publish`, waiting for each answer the protocol defines.
//! - [`Publisher::send_flv`] and [`Publisher::send`] send audio, video and data messages, split
//!   into chunks with the most compact chunk headers the specification allows.
//! - [`Publisher::poll`] reads what the server sent without blocking: acknowledgements, a new
//!   chunk size, ping requests (answered) and status messages. [`Publisher::send`] calls it every
//!   250 ms. The client acknowledges the server's bytes after every window the server sets.
//! - [`Publisher::close`] sends `deleteStream` and closes the connection.
//! - [`ChunkWriter`] and [`ChunkReader`] write and read the chunk stream on their own, for tests
//!   and servers.
//!
//! # Timeouts
//!
//! Each address the host name resolves to gets [`Config::connect_timeout`] to accept the TCP
//! connection. The TLS handshake, the RTMP handshake and the commands up to the server's answer to
//! `publish` share [`Config::setup_timeout`]. A write that makes no progress for
//! [`Config::stall_timeout`] (5 s by default) is reported as [`Error::Stalled`], and the publisher
//! refuses every later send with [`Error::Broken`]. Name resolution uses the system resolver,
//! which these timeouts do not bound.
//!
//! # The stream key
//!
//! [`StreamKey`] prints as `StreamKey(<redacted>)`. No error, `Debug` output or log line of this
//! crate holds the key: text a server sends back, such as a refusal's description, has every
//! occurrence of the key replaced with `<stream key>` before it goes into an [`Error`].
//!
//! # Threads and cost
//!
//! The crate starts no threads and holds no locks; the caller sends from its own thread.
//! [`ChunkWriter::write_message`] builds every chunk header on the stack and writes the header and
//! payload slices with vectored writes of up to 64 slices, so sending a message allocates nothing
//! and does not copy its payload before the socket. Over plain TCP a 12.5 kB video message at the
//! 4096-byte chunk size is one system call. Over TLS, rustls copies and encrypts the bytes and
//! allocates for each record. [`Publisher::send`] also reads the clock once and, every 250 ms,
//! reads what the server sent without blocking.
//!
//! The crate is built only for native targets: on `wasm32` it is empty.

#![warn(missing_docs)]

#[cfg(not(target_arch = "wasm32"))]
mod chunk;
#[cfg(not(target_arch = "wasm32"))]
mod error;
#[cfg(not(target_arch = "wasm32"))]
mod handshake;
#[cfg(not(target_arch = "wasm32"))]
mod publisher;
#[cfg(not(target_arch = "wasm32"))]
mod transport;
#[cfg(not(target_arch = "wasm32"))]
mod url;

#[cfg(test)]
mod tests;

#[cfg(not(target_arch = "wasm32"))]
pub use chunk::{
    ChunkReader, ChunkWriter, DEFAULT_CHUNK_SIZE, MAX_CHUNK_STREAMS, MAX_MESSAGE, Message, MessageHeader, PUBLISH_CHUNK_SIZE, csid, msg_type,
    user_control,
};
#[cfg(not(target_arch = "wasm32"))]
pub use error::Error;
#[cfg(all(feature = "rtmps", not(target_arch = "wasm32")))]
pub use publisher::TlsRoots;
#[cfg(not(target_arch = "wasm32"))]
pub use publisher::{
    Config, DEFAULT_CONNECT_TIMEOUT, DEFAULT_FLASH_VER, DEFAULT_SETUP_TIMEOUT, DEFAULT_STALL_TIMEOUT, Kind, MAX_SERVER_MESSAGE, MAX_TIMEOUT,
    POLL_INTERVAL, POLL_READ_LIMIT, Publisher, Stats,
};
#[cfg(all(feature = "rtmps", not(target_arch = "wasm32")))]
pub use rustls::pki_types::CertificateDer;
#[cfg(not(target_arch = "wasm32"))]
pub use url::{Endpoint, StreamKey};
