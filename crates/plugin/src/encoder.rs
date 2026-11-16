//! Encoders: how frames become a compressed stream.
//!
//! Outputs run encoding in a separate `ffmpeg` process the user installs, so an encoder
//! describes itself as the codec arguments that process takes. Hardware encoders (NVENC, AMF,
//! Quick Sync, Media Foundation) are encoders like any other: their arguments name ffmpeg's
//! hardware codec.

use studiocraft_model::Settings;

use crate::{Plugin, PluginError};

/// What an encoder compresses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncoderKind {
    Video,
    Audio,
}

/// What an encoder aims for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RateControl {
    /// Constant quality at this level (lower is better, about 0 to 51).
    Quality(u32),
    /// Constant bitrate for streaming, with a keyframe every `keyframe_s` seconds.
    Bitrate {
        kbps: u32,
        keyframe_s: u32,
    },
    Lossless,
}

/// An encoder type.
pub trait Encoder: Plugin {
    fn kind(&self) -> EncoderKind;
    /// The ffmpeg codec this encoder uses (checked against `ffmpeg -encoders` before a start).
    fn ffmpeg_codec(&self) -> &'static str;
    /// The codec arguments for `rate` at `fps` frames per second.
    fn ffmpeg_args(&self, settings: &Settings, rate: RateControl, fps: f64) -> Result<Vec<String>, PluginError>;
    /// Whether an RTMP stream can carry this encoder's video: streams are FLV, which holds H.264.
    fn streams(&self) -> bool {
        let codec = self.ffmpeg_codec();
        codec == "libx264" || codec.starts_with("h264_")
    }
}
