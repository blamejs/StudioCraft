//! Encoders: how frames become a compressed stream.
//!
//! An encoder names the [`Codec`] it writes and runs in one of two ways. A built-in encoder opens
//! a [`VideoEncoderSession`] that compresses frames inside StudioCraft ([`Encoder::open`]). An
//! ffmpeg encoder runs inside the separate `ffmpeg` program the user installs, and describes
//! itself as the codec arguments that program takes ([`Encoder::ffmpeg`], [`FfmpegEncoder`]).
//! Hardware encoders (NVENC, AMF, Quick Sync, Media Foundation) are ffmpeg encoders whose
//! arguments name ffmpeg's hardware codec.

use studiocraft_media::Tick;
use studiocraft_media::video::VideoFrame;
use studiocraft_model::Settings;

use crate::output::VideoFormat;
use crate::{Plugin, PluginError};

/// What an encoder compresses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncoderKind {
    Video,
    Audio,
}

/// The compressed format an encoder writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    /// H.264 (ITU-T H.264, Advanced Video Coding).
    H264,
    /// HEVC (ITU-T H.265, High Efficiency Video Coding).
    Hevc,
}

impl Codec {
    /// Stable lowercase id, as `plugin.list` reports it.
    pub fn id(self) -> &'static str {
        match self {
            Codec::H264 => "h264",
            Codec::Hevc => "hevc",
        }
    }
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

/// One compressed frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packet {
    pub data: Vec<u8>,
    /// When the frame is shown.
    pub pts: Tick,
    /// When the frame is decoded; earlier than `pts` for a frame that is shown after frames
    /// predicted from it.
    pub dts: Tick,
    /// Whether a decoder can start at this packet.
    pub keyframe: bool,
}

/// A running built-in video encoder, owned by one output's encode thread.
pub trait VideoEncoderSession: Send {
    /// Compress `frame` (in the format the session was opened with) and append the packets that
    /// are ready to `out`. An encoder that reorders frames can append none for a while.
    fn encode(&mut self, frame: &VideoFrame, out: &mut Vec<Packet>) -> Result<(), PluginError>;
    /// Append the packets still held back, at the end of the stream.
    fn flush(&mut self, out: &mut Vec<Packet>) -> Result<(), PluginError>;
}

/// What this machine offers encoders.
#[derive(Clone, Copy, Debug, Default)]
pub struct EncoderEnv<'a> {
    /// The encoders the ffmpeg program lists (`ffmpeg -encoders`); empty when that is not known.
    pub ffmpeg_encoders: &'a [String],
}

/// An encoder that runs inside the ffmpeg program.
pub trait FfmpegEncoder {
    /// The ffmpeg codec this encoder uses (checked against `ffmpeg -encoders` before a start).
    fn ffmpeg_codec(&self) -> &'static str;
    /// The codec arguments for `rate` at `fps` frames per second.
    fn ffmpeg_args(&self, settings: &Settings, rate: RateControl, fps: f64) -> Result<Vec<String>, PluginError>;
}

/// An encoder type.
pub trait Encoder: Plugin {
    fn kind(&self) -> EncoderKind;
    fn codec(&self) -> Codec;
    /// The ffmpeg form of this encoder; `None` for a built-in encoder.
    fn ffmpeg(&self) -> Option<&dyn FfmpegEncoder> {
        None
    }
    /// Start compressing frames of `format` at `rate`. An ffmpeg encoder cannot be opened, since
    /// the ffmpeg program runs it.
    fn open(&self, settings: &Settings, format: VideoFormat, rate: RateControl) -> Result<Box<dyn VideoEncoderSession>, PluginError> {
        let _ = (settings, format, rate);
        Err(PluginError::Unavailable(format!("{} runs inside the ffmpeg program", self.name())))
    }
    /// Whether this encoder can run here. A built-in encoder always can. An ffmpeg encoder can when
    /// the ffmpeg program lists its codec, or when that list is not known.
    fn available(&self, env: &EncoderEnv) -> bool {
        match self.ffmpeg() {
            Some(f) => env.ffmpeg_encoders.is_empty() || env.ffmpeg_encoders.iter().any(|c| c == f.ffmpeg_codec()),
            None => true,
        }
    }
    /// Whether an RTMP stream can carry this encoder's video: streams are FLV, which holds H.264.
    fn streams(&self) -> bool {
        self.codec() == Codec::H264
    }
}

#[cfg(test)]
mod tests {
    use studiocraft_media::video::PixelFormat;
    use studiocraft_model::SettingsSchema;

    use super::*;

    struct Builtin;

    impl Plugin for Builtin {
        fn id(&self) -> &'static str {
            "builtin"
        }
        fn name(&self) -> &'static str {
            "Built-in"
        }
        fn schema(&self) -> SettingsSchema {
            SettingsSchema::default()
        }
    }

    impl Encoder for Builtin {
        fn kind(&self) -> EncoderKind {
            EncoderKind::Video
        }
        fn codec(&self) -> Codec {
            Codec::Hevc
        }
    }

    struct External;

    impl Plugin for External {
        fn id(&self) -> &'static str {
            "external"
        }
        fn name(&self) -> &'static str {
            "External"
        }
        fn schema(&self) -> SettingsSchema {
            SettingsSchema::default()
        }
    }

    impl FfmpegEncoder for External {
        fn ffmpeg_codec(&self) -> &'static str {
            "libx264"
        }
        fn ffmpeg_args(&self, _: &Settings, _: RateControl, _: f64) -> Result<Vec<String>, PluginError> {
            Ok(Vec::new())
        }
    }

    impl Encoder for External {
        fn kind(&self) -> EncoderKind {
            EncoderKind::Video
        }
        fn codec(&self) -> Codec {
            Codec::H264
        }
        fn ffmpeg(&self) -> Option<&dyn FfmpegEncoder> {
            Some(self)
        }
    }

    #[test]
    fn an_ffmpeg_encoder_is_available_when_the_program_lists_its_codec() {
        let listed = ["libx264".to_string()];
        let other = ["libx265".to_string()];
        assert!(External.available(&EncoderEnv { ffmpeg_encoders: &listed }));
        assert!(!External.available(&EncoderEnv { ffmpeg_encoders: &other }));
        assert!(External.available(&EncoderEnv::default()), "an unknown list does not rule it out");
        assert!(Builtin.available(&EncoderEnv { ffmpeg_encoders: &other }), "a built-in encoder does not need ffmpeg");
    }

    #[test]
    fn only_h264_streams_and_an_ffmpeg_encoder_does_not_open() {
        assert!(External.streams() && !Builtin.streams());
        let format = VideoFormat { width: 64, height: 48, fps_num: 30, fps_den: 1, pixel: PixelFormat::Nv12 };
        let Err(e) = External.open(&Settings::new(), format, RateControl::Quality(23)) else { panic!("an ffmpeg encoder opened") };
        assert!(matches!(&e, PluginError::Unavailable(m) if m.contains("ffmpeg")), "{e}");
        assert_eq!((Codec::H264.id(), Codec::Hevc.id()), ("h264", "hevc"));
    }
}
