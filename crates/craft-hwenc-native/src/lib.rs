//! Hardware video encoders called through their drivers' own APIs, without the ffmpeg program.
//!
//! - [`nvenc`] encodes H.264 and HEVC on NVIDIA GPUs through NVENC, on Windows and 64-bit Linux.
//! - [`annexb`] splits Annex B byte streams into NAL units and writes the 4-byte length-prefixed
//!   samples a container stores.
//! - [`avc`] and [`hevc`] build the `avcC` and `hvcC` records from the parameter sets an encoder
//!   writes.
//!
//! The driver calls are in one private module with `unsafe` allowed; every item this crate exports
//! is safe.

#![deny(unsafe_op_in_unsafe_fn)]

pub mod annexb;
pub mod avc;
mod bits;
pub mod hevc;
pub mod nvenc;
#[allow(unsafe_code)]
mod nvenc_ffi;

/// The codec of a stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Codec {
    H264,
    Hevc,
}
