//! The built-in encoders. They run in StudioCraft's own process, so recording needs no separate
//! program.
//!
//! - [`h264::H264`] is the built-in H.264 video encoder, registered as the `h264` encoder. Its
//!   sessions take the NV12 frames the compositor converts and return packets in decoding order.
//! - [`aac::AacSession`] encodes the mixer's 48 kHz stereo blocks as AAC-LC.
//!
//! Both are written on the clean-room H.264 and AAC encoders of `craft-video` and `craft-audio`.

pub mod aac;
pub mod h264;

pub use aac::AacSession;
pub use h264::H264;
