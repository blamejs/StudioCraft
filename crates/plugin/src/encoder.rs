//! Encoders: how frames and audio become a compressed stream.
//!
//! An encoder names the [`Codec`] it writes. An ffmpeg encoder runs inside the separate `ffmpeg`
//! program the user installs, and describes itself as the codec arguments that program takes
//! ([`Encoder::ffmpeg`], [`FfmpegEncoder`]). Hardware encoders (NVENC, AMF, Quick Sync, Media
//! Foundation, VideoToolbox, VA-API) are ffmpeg encoders whose arguments name ffmpeg's hardware
//! codec. An encoder without
//! an ffmpeg form is built in: it runs in the app's own process as a [`VideoSession`]
//! ([`Encoder::open`]), and only where the output says it can ([`EncoderEnv::builtin`]).

use studiocraft_model::Settings;

use crate::{Plugin, PluginError};

/// What an encoder compresses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncoderKind {
    Video,
    Audio,
}

/// The compressed format an encoder writes. Formats are added over time, so a `match` on it
/// outside this crate needs a wildcard arm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Codec {
    /// H.264 (ITU-T H.264, Advanced Video Coding).
    H264,
    /// HEVC (ITU-T H.265, High Efficiency Video Coding).
    Hevc,
    /// AV1 (AOMedia Video 1).
    Av1,
    /// AAC (ISO/IEC 14496-3, Advanced Audio Coding).
    Aac,
}

impl Codec {
    /// Stable lowercase id, as `plugin.list` reports it.
    pub fn id(self) -> &'static str {
        match self {
            Codec::H264 => "h264",
            Codec::Hevc => "hevc",
            Codec::Av1 => "av1",
            Codec::Aac => "aac",
        }
    }

    /// Whether the format holds video or audio.
    pub fn kind(self) -> EncoderKind {
        match self {
            Codec::H264 | Codec::Hevc | Codec::Av1 => EncoderKind::Video,
            Codec::Aac => EncoderKind::Audio,
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

/// What an output offers encoders on this machine.
#[derive(Clone, Copy, Debug, Default)]
pub struct EncoderEnv<'a> {
    /// The encoders the ffmpeg program lists (`ffmpeg -encoders`); `None` when that list could not
    /// be read.
    pub ffmpeg_encoders: Option<&'a [String]>,
    /// Whether the output runs built-in encoders.
    pub builtin: bool,
}

impl<'a> EncoderEnv<'a> {
    /// An output that runs its encoder inside the ffmpeg program, which listed `listed`. An empty
    /// list means the list could not be read. Recordings and streams are such outputs.
    pub fn ffmpeg(listed: &'a [String]) -> Self {
        EncoderEnv { ffmpeg_encoders: (!listed.is_empty()).then_some(listed), builtin: false }
    }
}

/// Whether an encoder can run in an output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Availability {
    /// It can run.
    Ready,
    /// It runs inside the ffmpeg program, whose encoder list could not be read.
    Unknown,
    /// The ffmpeg program does not list its codec.
    NotInFfmpeg,
    /// It is built in, and the output does not run built-in encoders.
    NotRunHere,
}

impl Availability {
    /// Whether an output may start with the encoder: it is ready, or nothing shows it is not.
    pub fn can_start(self) -> bool {
        matches!(self, Availability::Ready | Availability::Unknown)
    }
}

/// What a built-in video encoder is opened for: NV12 frames of this size at this rate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VideoParams {
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    pub rate: RateControl,
}

/// One compressed picture. `pts` and `dts` count frames from the first frame given to the session.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Packet {
    pub data: Vec<u8>,
    pub pts: i64,
    pub dts: i64,
    pub keyframe: bool,
}

/// A running built-in video encoder. Frames go in display order and packets come out in decoding
/// order.
pub trait VideoSession: Send {
    /// The setup bytes a container stores with the track (for H.264 the `avcC` record).
    fn codec_private(&self) -> Vec<u8>;
    /// Encode `nv12`, a tightly packed NV12 frame of the opened size, shown at frame `pts`. The
    /// packets that are ready are appended to `out`.
    fn encode(&mut self, nv12: &[u8], pts: i64, out: &mut Vec<Packet>) -> Result<(), PluginError>;
    /// Append the packets the encoder still holds to `out`. No frame can follow.
    fn flush(&mut self, out: &mut Vec<Packet>);
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
    fn codec(&self) -> Codec;
    /// Video or audio, as its codec is.
    fn kind(&self) -> EncoderKind {
        self.codec().kind()
    }
    /// The ffmpeg form of this encoder; `None` for a built-in encoder.
    fn ffmpeg(&self) -> Option<&dyn FfmpegEncoder> {
        None
    }
    /// Start a built-in video encoder with `settings` (its schema's values) for `params`. An
    /// ffmpeg encoder has no session of its own.
    fn open(&self, settings: &Settings, params: &VideoParams) -> Result<Box<dyn VideoSession>, PluginError> {
        let _ = (settings, params);
        Err(PluginError::Unavailable(format!("{} runs only inside the ffmpeg program", self.name())))
    }
    /// Whether this encoder can run in an output that offers `env`.
    fn availability(&self, env: &EncoderEnv) -> Availability {
        match (self.ffmpeg(), env.ffmpeg_encoders) {
            (Some(_), None) => Availability::Unknown,
            (Some(f), Some(listed)) if listed.iter().any(|c| c == f.ffmpeg_codec()) => Availability::Ready,
            (Some(_), Some(_)) => Availability::NotInFfmpeg,
            (None, _) if env.builtin => Availability::Ready,
            (None, _) => Availability::NotRunHere,
        }
    }
    /// Whether an RTMP stream can carry this encoder's output: streams are FLV, which carries
    /// H.264 video and AAC audio.
    fn streams(&self) -> bool {
        matches!(self.codec(), Codec::H264 | Codec::Aac)
    }
}

#[cfg(test)]
mod tests {
    use studiocraft_model::SettingsSchema;

    use super::*;

    struct Builtin(Codec);

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
        fn codec(&self) -> Codec {
            self.0
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
        fn codec(&self) -> Codec {
            Codec::H264
        }
        fn ffmpeg(&self) -> Option<&dyn FfmpegEncoder> {
            Some(self)
        }
    }

    #[test]
    fn an_ffmpeg_encoder_is_ready_when_the_program_lists_its_codec() {
        let listed = ["libx264".to_string()];
        let other = ["libx265".to_string()];
        assert_eq!(External.availability(&EncoderEnv::ffmpeg(&listed)), Availability::Ready);
        assert_eq!(External.availability(&EncoderEnv::ffmpeg(&other)), Availability::NotInFfmpeg);
        let unknown = External.availability(&EncoderEnv::ffmpeg(&[]));
        assert_eq!(unknown, Availability::Unknown, "an empty list is one that could not be read");
        assert!(unknown.can_start() && !Availability::NotInFfmpeg.can_start());
    }

    #[test]
    fn a_built_in_encoder_runs_only_where_the_output_runs_built_in_encoders() {
        let e = Builtin(Codec::Hevc);
        let listed = ["libx264".to_string()];
        assert_eq!(e.availability(&EncoderEnv::ffmpeg(&listed)), Availability::NotRunHere);
        assert_eq!(e.availability(&EncoderEnv::ffmpeg(&[])), Availability::NotRunHere, "an unknown ffmpeg list does not let it run");
        assert!(!Availability::NotRunHere.can_start());
        assert_eq!(e.availability(&EncoderEnv { ffmpeg_encoders: None, builtin: true }), Availability::Ready);
        assert_eq!(External.availability(&EncoderEnv { ffmpeg_encoders: None, builtin: true }), Availability::Unknown);
    }

    #[test]
    fn the_codec_sets_the_kind_and_whether_a_stream_carries_it() {
        assert_eq!((Codec::H264.id(), Codec::Hevc.id(), Codec::Aac.id()), ("h264", "hevc", "aac"));
        assert_eq!(Builtin(Codec::Aac).kind(), EncoderKind::Audio);
        assert_eq!(Builtin(Codec::Hevc).kind(), EncoderKind::Video);
        assert!(External.streams() && Builtin(Codec::Aac).streams() && !Builtin(Codec::Hevc).streams());
    }
}
