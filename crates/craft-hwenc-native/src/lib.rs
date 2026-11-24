//! Hardware video encoders called through their drivers' own APIs, without the ffmpeg program.
//!
//! - [`nvenc`] encodes H.264, HEVC and AV1 on NVIDIA GPUs through NVENC, on Windows and 64-bit
//!   Linux, from frames in memory or, on Windows, from Direct3D 11 textures.
//! - [`mf`] encodes H.264, HEVC and AV1 with the hardware encoders that Windows Media Foundation
//!   lists.
//! - [`vt`] encodes H.264 and HEVC with the hardware encoders of macOS VideoToolbox.
//! - [`vaapi`] encodes H.264, HEVC and AV1 with the VA-API hardware encoders on 64-bit Linux.
//! - [`annexb`] splits Annex B byte streams into NAL units and writes the 4-byte length-prefixed
//!   samples a container stores.
//! - [`avc`] and [`hevc`] build the `avcC` and `hvcC` records from the parameter sets an encoder
//!   writes, with `craft-media`'s record writers.
//! - [`av1`] splits AV1 temporal units into OBUs, builds the `av1C` record from the sequence
//!   header, and writes the samples a container stores.
//! - [`hdr`] holds the color signal a stream declares ([`Color`]): BT.709, or BT.2020 with PQ or
//!   HLG, and the SEI messages of the PQ static metadata.
//!
//! The encoders take a [`Config`] and return [`Packet`]s or an [`Error`]. The system calls are in
//! private modules with `unsafe` allowed; every item this crate exports is safe.

#![deny(unsafe_op_in_unsafe_fn)]

pub mod annexb;
pub mod av1;
pub mod avc;
pub mod hdr;
pub mod hevc;
pub mod mf;
#[allow(unsafe_code)]
mod mf_ffi;
pub mod nvenc;
#[allow(unsafe_code)]
mod nvenc_ffi;
pub mod vaapi;
#[allow(unsafe_code)]
mod vaapi_ffi;
pub mod vt;
#[allow(unsafe_code)]
mod vt_ffi;

pub use hdr::{Color, ContentLight, MasteringDisplay};

/// The codec of a stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Codec {
    H264,
    Hevc,
    /// AV1 Main profile. NVENC, Media Foundation and VA-API encode it; VideoToolbox returns
    /// [`Error::Unsupported`].
    Av1,
}

/// The layout of the frames an encoder takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Input {
    /// 8-bit 4:2:0: the luma rows, then the interleaved Cb and Cr rows at half height.
    Nv12,
    /// 10-bit 4:2:0 in 16-bit little-endian samples with the value in the high 10 bits, the
    /// planes laid out as NV12. HEVC Main 10 and 10-bit AV1 Main, on NVENC only.
    P010,
}

impl Input {
    pub(crate) fn bytes_per_sample(self) -> usize {
        match self {
            Input::Nv12 => 1,
            Input::P010 => 2,
        }
    }

    /// The bytes of one `width` × `height` frame.
    pub fn frame_bytes(self, width: u32, height: u32) -> usize {
        (width as usize).saturating_mul(height as usize).saturating_mul(3) / 2 * self.bytes_per_sample()
    }
}

/// How the encoder spends bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rate {
    /// Constant quality at this level, 1 to 51, lower is better.
    Quality(u32),
    /// Constant bitrate in kbit/s, with a one-second buffer.
    Cbr(u32),
}

/// What to encode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    pub codec: Codec,
    pub input: Input,
    pub width: u32,
    pub height: u32,
    /// Frames per second as numerator and denominator.
    pub fps: (u32, u32),
    pub rate: Rate,
    /// Frames from one keyframe to the next.
    pub keyint: u32,
    /// The color signal the stream declares. NVENC writes high dynamic range as HEVC Main 10 from
    /// P010 frames; the other encoders write BT.709 only.
    pub color: Color,
}

/// One encoded frame: 4-byte length-prefixed NAL units without parameter sets for H.264 and HEVC,
/// or AV1 OBUs with their sizes and without temporal delimiters (see [`av1::Stream::sample`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packet {
    pub data: Vec<u8>,
    /// The timestamp the frame went in with; with no B-frames it is also the decoding time.
    pub pts: i64,
    pub keyframe: bool,
}

/// Why an encoder could not open or run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// No GPU, driver or encode library for this encoder on this system.
    NoDevice(String),
    /// Every encoding session of the GPU is taken by other programs; a later open can work.
    Busy(String),
    /// The GPU's encoder does not take this configuration.
    Unsupported(String),
    /// The driver failed.
    Failed(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NoDevice(e) | Error::Busy(e) | Error::Unsupported(e) | Error::Failed(e) => f.write_str(e),
        }
    }
}

impl std::error::Error for Error {}
