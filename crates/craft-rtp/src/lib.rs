//! RTP and RTCP for receiving media (RFC 3550), with the depacketizers that rebuild coded frames.
//!
//! - [`Packet`] reads and writes an RTP data packet: the fixed header, the contributing sources,
//!   a header extension and padding. [`is_rtcp`] tells RTCP from RTP when both share one port
//!   (RFC 5761).
//! - [`rtcp`] reads a compound RTCP packet item by item: sender and receiver reports, BYE, the
//!   generic NACK and the picture loss indication (RFC 4585), and writes each of them. Its
//!   [`TimeMap`](rtcp::TimeMap) maps RTP timestamps to the sender's wall clock from its sender
//!   reports.
//! - [`JitterBuffer`] puts the packets of one stream back in sequence order and releases each one
//!   after a delay that follows the measured inter-arrival jitter. It lists the missing sequence
//!   numbers to ask for once with a NACK, gives up on a missing packet after the delay, keeps the
//!   counts of a receiver report, and takes the time as an argument, so tests drive it.
//! - [`Depacketizer`] rebuilds H.264 (RFC 6184: single NAL units, STAP-A and FU-A), VP8
//!   (RFC 7741), Opus (RFC 7587) and AAC (RFC 3640, AAC-hbr and AAC-lbr) frames from the payloads
//!   the jitter buffer releases. A video frame with a lost part is dropped, and video then waits
//!   for the next key frame.
//!
//! # Cost
//!
//! Parsing borrows the input and allocates nothing. The jitter buffer copies each payload once,
//! into a buffer it recycles, and holds at most 2 s or 8 MiB of payload by default. A depacketizer
//! copies each payload once into its frame buffer, which stops growing at the largest frame
//! (8 MiB at most by default); Opus packets are passed on without a copy.
//!
//! No sockets, no threads, no dependencies beyond `std`, no `unsafe`, builds for
//! `wasm32-unknown-unknown`.

#![warn(missing_docs)]

mod depacketize;
mod jitter;
mod packet;
pub mod rtcp;

use std::fmt;

pub use depacketize::{AacConfig, Codec, Depacketizer, DepacketizerStats, Frame, MAX_FRAME};
pub use jitter::{Config, Event, JitterBuffer, LastSenderReport, MAX_DROPOUT, MAX_MISORDER, Push, Released, SLOTS, Stats};
pub use packet::{Extension, HEADER_LEN, MAX_CSRC, Packet, is_rtcp};

/// An error from reading or writing RTP or RTCP, or from rebuilding a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The input ends inside this part.
    Truncated(&'static str),
    /// The input breaks the format here.
    Invalid(&'static str),
    /// The input uses a part of the format this crate does not read.
    Unsupported(&'static str),
    /// A value passed in to be written is not valid.
    BadInput(&'static str),
    /// A frame or packet would be longer than the limit.
    TooLarge {
        /// Bytes it would have.
        size: usize,
        /// Most bytes it may have.
        limit: usize,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Truncated(s) => write!(f, "the RTP data ends inside {s}"),
            Error::Invalid(s) => write!(f, "RTP data not valid: {s}"),
            Error::Unsupported(s) => write!(f, "RTP data uses {s}, which is not read"),
            Error::BadInput(s) => write!(f, "{s}"),
            Error::TooLarge { size, limit } => write!(f, "{size} bytes is over the limit of {limit}"),
        }
    }
}

impl std::error::Error for Error {}

/// The big-endian `u16` at `at`, or `None` past the end.
fn be16(b: &[u8], at: usize) -> Option<u16> {
    let s: [u8; 2] = b.get(at..at.checked_add(2)?)?.try_into().ok()?;
    Some(u16::from_be_bytes(s))
}

/// The big-endian `u32` at `at`, or `None` past the end.
fn be32(b: &[u8], at: usize) -> Option<u32> {
    let s: [u8; 4] = b.get(at..at.checked_add(4)?)?.try_into().ok()?;
    Some(u32::from_be_bytes(s))
}

/// The big-endian `u64` at `at`, or `None` past the end.
fn be64(b: &[u8], at: usize) -> Option<u64> {
    let s: [u8; 8] = b.get(at..at.checked_add(8)?)?.try_into().ok()?;
    Some(u64::from_be_bytes(s))
}

#[cfg(test)]
mod tests;
