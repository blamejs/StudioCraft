//! The VA-API hardware encoders that run in StudioCraft's own process on 64-bit Linux: H.264 and
//! HEVC from the compositor's NV12 frames, with the `avcC` or `hvcC` record a container stores.
//!
//! - The rate control, the keyframe interval and the limits on them are the ones every built-in
//!   hardware encoder takes ([`crate::hardware`]).
//! - Every picture is an IDR or P picture, so each packet's decoding time is its presentation time.
//! - The encoder runs on the first render node whose driver encodes the codec.

use craft_hwenc_native::Codec as NativeCodec;
use craft_hwenc_native::vaapi::{Config, Encoder as Core, Error};
use studiocraft_model::{Settings, SettingsSchema};
use studiocraft_plugin::{Availability, Codec, Encoder, EncoderEnv, Packet, Plugin, PluginError, VideoParams, VideoSession};

use crate::hardware::{self, Hardware, SessionError, Tested, Tests, packets};

/// A VA-API hardware encoder of one codec.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VaApi {
    codec: NativeCodec,
}

impl VaApi {
    /// VA-API H.264, registered as `vaapi_native_h264`.
    pub const H264: VaApi = VaApi { codec: NativeCodec::H264 };
    /// VA-API HEVC, registered as `vaapi_native_hevc`.
    pub const HEVC: VaApi = VaApi { codec: NativeCodec::Hevc };

    /// Open a session for `params` without a registry.
    pub fn session(&self, params: &VideoParams) -> Result<VaSession, PluginError> {
        self.try_session(params).map_err(|e| match e {
            SessionError::Busy(why) => PluginError::Unavailable(why),
            SessionError::Other(e) => e,
        })
    }

    /// [`VaApi::session`], telling an encoder that other programs hold now apart from other
    /// failures; the hardware encoder tests use it.
    pub fn try_session(&self, params: &VideoParams) -> Result<VaSession, SessionError> {
        let cfg = hardware::config(self.codec, params, "VA-API (built in)").map_err(SessionError::Other)?;
        let core = Core::new(&cfg).map_err(|e| match e {
            Error::Busy(why) => SessionError::Busy(format!("{} has no free encoding session: {why}", self.name())),
            e => SessionError::Other(self.error(e)),
        })?;
        LAST_TEST.opened(self.codec);
        Ok(VaSession { core, name: self.name() })
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

impl Plugin for VaApi {
    fn id(&self) -> &'static str {
        match self.codec {
            NativeCodec::H264 => "vaapi_native_h264",
            NativeCodec::Hevc => "vaapi_native_hevc",
            NativeCodec::Av1 => "vaapi_native_av1",
        }
    }
    fn name(&self) -> &'static str {
        match self.codec {
            NativeCodec::H264 => "VA-API H.264 (built in)",
            NativeCodec::Hevc => "VA-API HEVC (built in)",
            NativeCodec::Av1 => "VA-API AV1 (built in)",
        }
    }
    fn schema(&self) -> SettingsSchema {
        SettingsSchema::default()
    }
}

impl Encoder for VaApi {
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
    /// Ready in an output that runs built-in encoders on a system where a VA-API driver opens an
    /// encoder for this codec (see [`usable`]).
    fn availability(&self, env: &EncoderEnv) -> Availability {
        if env.builtin && usable(self.codec) { Availability::Ready } else { Availability::NotRunHere }
    }
}

impl Hardware for VaApi {
    fn try_open(&self, params: &VideoParams) -> Result<Box<dyn VideoSession>, SessionError> {
        Ok(Box::new(self.try_session(params)?))
    }
}

/// The session tests of the VA-API encoders and the sessions that opened.
static LAST_TEST: Tests = Tests::new();

/// Whether a VA-API driver opens an encoder that writes `codec` on this system. A call opens and
/// closes one; an encoder that opened is remembered, and a failure is remembered for
/// [`hardware::RETEST_AFTER`]. Without libva nothing is tested, and once every render node's
/// driver has listed no encoder for the codec, a test returns at once without initializing the
/// drivers again.
fn usable(codec: NativeCodec) -> bool {
    if craft_hwenc_native::vaapi::driver().is_err() {
        return false;
    }
    LAST_TEST.usable(codec, || match craft_hwenc_native::vaapi::available(codec) {
        Ok(()) => Tested::Works,
        Err(Error::Busy(_)) => Tested::Busy,
        Err(_) => Tested::Failed,
    })
}

/// A running VA-API encoder.
pub struct VaSession {
    core: Core,
    name: &'static str,
}

impl VaSession {
    /// The settings the session runs with.
    pub fn config(&self) -> &Config {
        self.core.config()
    }
}

impl VideoSession for VaSession {
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
        match VaApi::H264.session(&params(RateControl::Lossless)) {
            Err(PluginError::Unavailable(why)) => assert!(why.starts_with("VA-API (built in) cannot record losslessly"), "{why}"),
            Err(e) => panic!("{e}"),
            Ok(_) => panic!("a lossless session opened"),
        }
    }

    #[test]
    fn it_runs_only_where_built_in_encoders_run_and_an_encoder_opens() {
        let stream = EncoderEnv { ffmpeg_encoders: None, builtin: false };
        assert_eq!(VaApi::H264.availability(&stream), Availability::NotRunHere);
        let recording = EncoderEnv { ffmpeg_encoders: None, builtin: true };
        let expected = if craft_hwenc_native::vaapi::available(NativeCodec::Hevc).is_ok() { Availability::Ready } else { Availability::NotRunHere };
        assert_eq!(VaApi::HEVC.availability(&recording), expected);
    }

    #[test]
    fn without_an_encoder_opening_is_an_error_and_with_one_frames_come_back_in_order() {
        let mut s = match VaApi::H264.open(&Settings::new(), &params(RateControl::Bitrate { kbps: 2500, keyframe_s: 2 })) {
            Ok(s) => s,
            Err(PluginError::Unavailable(why)) => return assert!(why.starts_with("VA-API H.264 (built in)"), "{why}"),
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
