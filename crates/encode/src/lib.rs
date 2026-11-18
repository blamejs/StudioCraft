//! The built-in encoders. They run in StudioCraft's own process, so recording needs no separate
//! program.
//!
//! - [`h264::H264`] is the built-in H.264 video encoder, registered as the `h264` encoder. Its
//!   sessions take the NV12 frames the compositor converts and return packets in decoding order.
//! - [`aac::AacSession`] encodes the mixer's 48 kHz stereo blocks as AAC-LC.
//! - [`nvenc::Nvenc`] runs NVIDIA's NVENC H.264 and HEVC encoders through the driver, registered
//!   as `nvenc_native_h264` and `nvenc_native_hevc`.
//!
//! The H.264 and AAC encoders are written on the clean-room encoders of `craft-video` and
//! `craft-audio`; NVENC runs through `craft-hwenc-native`.

pub mod aac;
pub mod h264;
pub mod nvenc;

pub use aac::AacSession;
pub use h264::H264;
pub use nvenc::Nvenc;
