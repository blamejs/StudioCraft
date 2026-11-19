//! The client side of the RTMP handshake (§5.2).

use std::io::{Read, Write};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::Error;

/// The RTMP version this crate speaks (§5.2.2).
pub const VERSION: u8 = 3;
/// Bytes in C1, S1, C2 and S2.
pub const PACKET: usize = 1536;

/// Fills the 1528 random bytes of C1. The specification asks for values that tell this side's
/// handshake from the peer's, and says they need not be cryptographically secure, so a small
/// xorshift generator seeded from the clock is enough.
pub fn random_bytes(out: &mut [u8]) {
    let seed = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0x9E37_79B9_7F4A_7C15, |d| d.as_nanos() as u64);
    let mut x = seed | 1;
    for b in out.iter_mut() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *b = (x >> 32) as u8;
    }
}

/// Runs the client's handshake: sends C0 and C1, reads S0 and S1, sends C2 (the echo of S1 with
/// the time it was read), and reads S2. S2 is read but not compared with C1, since servers that
/// use other handshake schemes send different bytes there. A server that picks a version other
/// than 3 is refused. `elapsed_ms` gives the milliseconds since the connection opened, for the
/// time fields. Read errors come back as they are, for the caller to map.
pub fn client<S: Read + Write + ?Sized>(s: &mut S, elapsed_ms: impl Fn() -> u32) -> Result<(), HandshakeError> {
    let mut c0c1 = [0u8; 1 + PACKET];
    c0c1[0] = VERSION;
    c0c1[1..5].copy_from_slice(&elapsed_ms().to_be_bytes());
    // Bytes 5..9 are the zero field.
    random_bytes(&mut c0c1[9..]);
    s.write_all(&c0c1).map_err(HandshakeError::Write)?;
    s.flush().map_err(HandshakeError::Write)?;

    let mut s0s1 = [0u8; 1 + PACKET];
    s.read_exact(&mut s0s1).map_err(HandshakeError::Read)?;
    let read_at = elapsed_ms();
    if s0s1[0] != VERSION {
        return Err(HandshakeError::Rtmp(Error::Protocol("the server chose an RTMP version other than 3")));
    }
    let mut c2 = [0u8; PACKET];
    c2[..4].copy_from_slice(&s0s1[1..5]);
    c2[4..8].copy_from_slice(&read_at.to_be_bytes());
    c2[8..].copy_from_slice(&s0s1[9..]);
    s.write_all(&c2).map_err(HandshakeError::Write)?;
    s.flush().map_err(HandshakeError::Write)?;

    let mut s2 = [0u8; PACKET];
    s.read_exact(&mut s2).map_err(HandshakeError::Read)?;
    Ok(())
}

/// A handshake failure, with I/O errors kept apart so the caller can tell a timeout from a stall.
#[derive(Debug)]
pub enum HandshakeError {
    /// Reading failed.
    Read(std::io::Error),
    /// Writing failed.
    Write(std::io::Error),
    /// The server's answer is not valid.
    Rtmp(Error),
}
