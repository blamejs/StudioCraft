//! The built-in encoders. They run in StudioCraft's own process, so recording needs no separate
//! program.
//!
//! - [`h264::H264`] is the built-in H.264 video encoder, registered as the `h264` encoder. Its
//!   sessions take the NV12 frames the compositor converts and return packets in decoding order.
//! - [`aac::AacSession`] encodes the mixer's 48 kHz stereo blocks as AAC-LC.
//! - [`nvenc::Nvenc`] runs NVIDIA's NVENC H.264 and HEVC encoders through the driver, registered
//!   as `nvenc_native_h264` and `nvenc_native_hevc`.
//! - [`mf::MediaFoundation`] runs the hardware H.264 and HEVC encoders Windows Media Foundation
//!   lists, registered as `mf_native_h264` and `mf_native_hevc`.
//! - [`vt::VideoToolbox`] runs the hardware H.264 and HEVC encoders of macOS VideoToolbox,
//!   registered as `vt_native_h264` and `vt_native_hevc`.
//! - [`vaapi::VaApi`] runs the VA-API hardware H.264 and HEVC encoders on 64-bit Linux,
//!   registered as `vaapi_native_h264` and `vaapi_native_hevc`.
//! - [`hardware::for_candidate`] gives the built-in encoder that runs a `craft_hwenc` candidate.
//!
//! The H.264 and AAC encoders are written on the clean-room encoders of `craft-video` and
//! `craft-audio`; NVENC, Media Foundation, VideoToolbox and VA-API run through
//! `craft-hwenc-native`.

pub mod aac;
pub mod h264;
pub mod hardware;
pub mod mf;
pub mod nvenc;
pub mod vaapi;
pub mod vt;

pub use aac::AacSession;
pub use h264::H264;
pub use hardware::{Hardware, SessionError};
pub use mf::MediaFoundation;
pub use nvenc::Nvenc;
pub use vaapi::VaApi;
pub use vt::VideoToolbox;
