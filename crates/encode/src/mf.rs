//! The Windows Media Foundation hardware encoders that run in StudioCraft's own process: H.264 and
//! HEVC from the compositor's NV12 frames, with the `avcC` or `hvcC` record a container stores.
//! Media Foundation is loaded at run time, and the encoder is the first hardware encoder MFT it
//! lists that takes the stream.
//!
//! - The rate control, the keyframe interval and the limits on them are the ones every built-in
//!   hardware encoder takes ([`crate::hardware`]).
//! - The encoder is asked for no B-frames, so each packet's decoding time is its presentation
//!   time.
//! - The media types say BT.709 at limited range, as StudioCraft's NV12 conversion writes it.

use craft_hwenc_native::Codec as NativeCodec;
use craft_hwenc_native::mf::{Config, Encoder as Core, Error};
use studiocraft_model::{Settings, SettingsSchema};
use studiocraft_plugin::{Availability, Codec, Encoder, EncoderEnv, Packet, Plugin, PluginError, VideoParams, VideoSession};

use crate::hardware::{self, Hardware, SessionError, Tested, Tests, packets};

/// A Media Foundation hardware encoder of one codec.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MediaFoundation {
    codec: NativeCodec,
}

impl MediaFoundation {
    /// Media Foundation H.264, registered as `mf_native_h264`.
    pub const H264: MediaFoundation = MediaFoundation { codec: NativeCodec::H264 };
    /// Media Foundation HEVC, registered as `mf_native_hevc`.
    pub const HEVC: MediaFoundation = MediaFoundation { codec: NativeCodec::Hevc };
    /// Media Foundation AV1, registered as `mf_native_av1`.
    pub const AV1: MediaFoundation = MediaFoundation { codec: NativeCodec::Av1 };

    /// Open a session for `params` without a registry.
    pub fn session(&self, params: &VideoParams) -> Result<MfSession, PluginError> {
        self.try_session(params).map_err(|e| match e {
            SessionError::Busy(why) => PluginError::Unavailable(why),
            SessionError::Other(e) => e,
        })
    }

    /// [`MediaFoundation::session`], telling an encoder that lacked the resources to start apart
    /// from other failures; the hardware encoder tests use it.
    pub fn try_session(&self, params: &VideoParams) -> Result<MfSession, SessionError> {
        let cfg = hardware::config(self.codec, params, "Media Foundation (built in)").map_err(SessionError::Other)?;
        let core = Core::new(&cfg).map_err(|e| match e {
            Error::Busy(why) => SessionError::Busy(format!("{} has no free encoding session: {why}", self.name())),
            e => SessionError::Other(self.error(e)),
        })?;
        LAST_TEST.opened(self.codec);
        Ok(MfSession { core, name: self.name() })
    }

    fn error(&self, e: Error) -> PluginError {
        match e {
            Error::NoDevice(why) => PluginError::Unavailable(format!("{} cannot run on this system: {why}", self.name())),
            Error::Busy(why) => PluginError::Unavailable(format!("{} has no free encoding session: {why}", self.name())),
            Error::Unsupported(why) => PluginError::Unavailable(format!("{} cannot encode this video: {why}", self.name())),
            Error::Failed(why) => PluginError::Failed(format!("{} could not start: {why}", self.name())),
        }
    }
}

impl Plugin for MediaFoundation {
    fn id(&self) -> &'static str {
        match self.codec {
            NativeCodec::H264 => "mf_native_h264",
            NativeCodec::Hevc => "mf_native_hevc",
            NativeCodec::Av1 => "mf_native_av1",
        }
    }
    fn name(&self) -> &'static str {
        match self.codec {
            NativeCodec::H264 => "Windows Media Foundation H.264 (built in)",
            NativeCodec::Hevc => "Windows Media Foundation HEVC (built in)",
            NativeCodec::Av1 => "Windows Media Foundation AV1 (built in)",
        }
    }
    fn schema(&self) -> SettingsSchema {
        SettingsSchema::default()
    }
}

impl Encoder for MediaFoundation {
    fn codec(&self) -> Codec {
        match self.codec {
            NativeCodec::H264 => Codec::H264,
            NativeCodec::Hevc => Codec::Hevc,
            NativeCodec::Av1 => Codec::Av1,
        }
    }
    fn open(&self, _settings: &Settings, params: &VideoParams) -> Result<Box<dyn VideoSession>, PluginError> {
        Ok(Box::new(self.session(params)?))
    }
    /// Ready in an output that runs built-in encoders on a system where Media Foundation lists a
    /// hardware encoder for this codec that opens (see [`usable`]).
    fn availability(&self, env: &EncoderEnv) -> Availability {
        if env.builtin && usable(self.codec) { Availability::Ready } else { Availability::NotRunHere }
    }
}

impl Hardware for MediaFoundation {
    fn try_open(&self, params: &VideoParams) -> Result<Box<dyn VideoSession>, SessionError> {
        Ok(Box::new(self.try_session(params)?))
    }
}

/// The session tests of the Media Foundation encoders and the sessions that opened.
static LAST_TEST: Tests = Tests::new();

/// Whether a Media Foundation hardware encoder that writes `codec` opens on this system. A call
/// opens and closes one; an encoder that opened is remembered, and a failure is remembered for
/// [`hardware::RETEST_AFTER`]. A failure because the encoder lacked the resources to start is
/// also forgotten when a session of the other codec opens. Without Media Foundation nothing is
/// tested again.
fn usable(codec: NativeCodec) -> bool {
    if craft_hwenc_native::mf::driver().is_err() {
        return false;
    }
    LAST_TEST.usable(codec, || match craft_hwenc_native::mf::available(codec) {
        Ok(()) => Tested::Works,
        Err(Error::Busy(_)) => Tested::Busy,
        Err(_) => Tested::Failed,
    })
}

/// A running Media Foundation encoder.
pub struct MfSession {
    core: Core,
    name: &'static str,
}

impl MfSession {
    /// The settings the session runs with.
    pub fn config(&self) -> &Config {
        self.core.config()
    }
}

impl VideoSession for MfSession {
    fn codec_private(&self) -> Vec<u8> {
        self.core.codec_private().to_vec()
    }

    fn encode(&mut self, nv12: &[u8], pts: i64, out: &mut Vec<Packet>) -> Result<(), PluginError> {
        let ps = self.core.encode(nv12, pts).map_err(|e| PluginError::Failed(format!("{} stopped: {e}", self.name)))?;
        packets(out, ps);
        Ok(())
    }

    fn flush(&mut self, out: &mut Vec<Packet>) -> Result<(), PluginError> {
        let ps = self.core.flush().map_err(|e| PluginError::Failed(format!("{} lost its last frames: {e}", self.name)))?;
        packets(out, ps);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use studiocraft_plugin::RateControl;

    fn params(rate: RateControl) -> VideoParams {
        VideoParams { width: 256, height: 144, fps_num: 60, fps_den: 1, rate }
    }

    #[test]
    fn a_lossless_recording_names_the_encoder() {
        match MediaFoundation::H264.session(&params(RateControl::Lossless)) {
            Err(PluginError::Unavailable(why)) => assert!(why.starts_with("Media Foundation (built in) cannot record losslessly"), "{why}"),
            Err(e) => panic!("{e}"),
            Ok(_) => panic!("a lossless session opened"),
        }
    }

    #[test]
    fn it_runs_only_where_built_in_encoders_run_and_an_encoder_opens() {
        let stream = EncoderEnv { ffmpeg_encoders: None, builtin: false };
        assert_eq!(MediaFoundation::H264.availability(&stream), Availability::NotRunHere);
        let recording = EncoderEnv { ffmpeg_encoders: None, builtin: true };
        let expected = if craft_hwenc_native::mf::available(NativeCodec::Hevc).is_ok() { Availability::Ready } else { Availability::NotRunHere };
        assert_eq!(MediaFoundation::HEVC.availability(&recording), expected);
    }

    #[test]
    fn without_an_encoder_opening_is_an_error_and_with_one_frames_come_back_in_order() {
        let mut s = match MediaFoundation::H264.open(&Settings::new(), &params(RateControl::Bitrate { kbps: 2500, keyframe_s: 2 })) {
            Ok(s) => s,
            Err(PluginError::Unavailable(why)) => return assert!(why.starts_with("Windows Media Foundation H.264 (built in)"), "{why}"),
            Err(e) => panic!("{e}"),
        };
        assert!(!s.codec_private().is_empty());
        let frame = vec![128u8; 256 * 144 * 3 / 2];
        let mut out = Vec::new();
        for pts in 0..5 {
            s.encode(&frame, pts, &mut out).unwrap();
        }
        s.flush(&mut out).unwrap();
        assert_eq!(out.iter().map(|p| (p.pts, p.dts)).collect::<Vec<_>>(), (0..5).map(|i| (i, i)).collect::<Vec<_>>());
        assert!(out[0].keyframe);
    }
}
