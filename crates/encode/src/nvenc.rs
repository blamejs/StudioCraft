//! The NVENC encoders that run through the NVIDIA driver in StudioCraft's own process: H.264 and
//! HEVC from the compositor's NV12 frames, with the `avcC` or `hvcC` record a container stores.
//!
//! - Quality mode is constant quality ([`QUALITY`]) with a keyframe every
//!   [`RECORDING_KEYFRAME_S`] seconds. Bitrate mode is constant bitrate ([`KBPS`]) with the
//!   requested keyframe interval ([`KEYFRAME_S`]). A level, bitrate or interval outside those
//!   ranges is an error, and lossless recording is not offered.
//! - The encoder writes no B-frames, so each packet's decoding time is its presentation time.
//! - The stream's VUI says BT.709 at limited range, as StudioCraft's NV12 conversion writes it.

use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use craft_hwenc_native::Codec as NativeCodec;
use craft_hwenc_native::nvenc::{Config, Encoder as Core, Error, Input, Rate};
use studiocraft_model::{Settings, SettingsSchema};
use studiocraft_plugin::{Availability, Codec, Encoder, EncoderEnv, Packet, Plugin, PluginError, RateControl, VideoParams, VideoSession};

use crate::h264::{KBPS, KEYFRAME_S, RECORDING_KEYFRAME_S};

/// The quality levels NVENC takes; lower is better.
pub const QUALITY: std::ops::RangeInclusive<u32> = 1..=51;

/// Why [`Nvenc::try_session`] did not open a session.
#[derive(Debug)]
pub enum SessionError {
    /// Every encoding session of the GPU is taken by other programs; a later try can work.
    Busy(String),
    /// Any other reason.
    Other(PluginError),
}

/// An NVENC encoder of one codec.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Nvenc {
    codec: NativeCodec,
}

impl Nvenc {
    /// NVENC H.264, registered as `nvenc_native_h264`.
    pub const H264: Nvenc = Nvenc { codec: NativeCodec::H264 };
    /// NVENC HEVC, registered as `nvenc_native_hevc`.
    pub const HEVC: Nvenc = Nvenc { codec: NativeCodec::Hevc };

    /// The encoder that runs `candidate` (a `craft_hwenc` candidate id) in this process, if any.
    pub fn for_candidate(candidate: &str) -> Option<Nvenc> {
        match candidate {
            "nvenc_h264" => Some(Nvenc::H264),
            "nvenc_hevc" => Some(Nvenc::HEVC),
            _ => None,
        }
    }

    /// Open a session for `params` without a registry.
    pub fn session(&self, params: &VideoParams) -> Result<NvencSession, PluginError> {
        self.try_session(params).map_err(|e| match e {
            SessionError::Busy(why) => PluginError::Unavailable(why),
            SessionError::Other(e) => e,
        })
    }

    /// [`Nvenc::session`], telling a GPU whose sessions are all taken apart from other failures;
    /// the hardware encoder tests use it.
    pub fn try_session(&self, params: &VideoParams) -> Result<NvencSession, SessionError> {
        let cfg = config(self.codec, params).map_err(SessionError::Other)?;
        let core = Core::new(&cfg).map_err(|e| match e {
            Error::Busy(why) => SessionError::Busy(format!("{} has no free encoding session: {why}", self.name())),
            e => SessionError::Other(self.error(e)),
        })?;
        // A session opened: this codec works, and the other one is tested again if it failed only
        // because every session was taken.
        *last_test(self.codec) = Some((Tested::Works, Instant::now()));
        let other = if self.codec == NativeCodec::H264 { NativeCodec::Hevc } else { NativeCodec::H264 };
        let mut o = last_test(other);
        if matches!(*o, Some((Tested::Busy, _))) {
            *o = None;
        }
        Ok(NvencSession { core, name: self.name() })
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

impl Plugin for Nvenc {
    fn id(&self) -> &'static str {
        match self.codec {
            NativeCodec::H264 => "nvenc_native_h264",
            NativeCodec::Hevc => "nvenc_native_hevc",
        }
    }
    fn name(&self) -> &'static str {
        match self.codec {
            NativeCodec::H264 => "NVIDIA NVENC H.264 (built in)",
            NativeCodec::Hevc => "NVIDIA NVENC HEVC (built in)",
        }
    }
    fn schema(&self) -> SettingsSchema {
        SettingsSchema::default()
    }
}

impl Encoder for Nvenc {
    fn codec(&self) -> Codec {
        match self.codec {
            NativeCodec::H264 => Codec::H264,
            NativeCodec::Hevc => Codec::Hevc,
        }
    }
    fn open(&self, _settings: &Settings, params: &VideoParams) -> Result<Box<dyn VideoSession>, PluginError> {
        Ok(Box::new(self.session(params)?))
    }
    /// Ready in an output that runs built-in encoders on a system where an NVENC session that
    /// writes this codec opens (see [`usable`]).
    fn availability(&self, env: &EncoderEnv) -> Availability {
        if env.builtin && usable(self.codec) { Availability::Ready } else { Availability::NotRunHere }
    }
}

/// How long a failed session test stands before the next call tests again.
const RETEST_AFTER: Duration = Duration::from_secs(30);

/// What a session test found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tested {
    Works,
    /// Every session was taken by other programs.
    Busy,
    /// No device, or a GPU whose encoder does not write the codec.
    Failed,
}

/// The last answer of [`usable`] for each codec, or of a session that opened, and when it was found.
static LAST_TEST: [Mutex<Option<(Tested, Instant)>>; 2] = [Mutex::new(None), Mutex::new(None)];

/// The [`LAST_TEST`] entry of `codec`, locked.
fn last_test(codec: NativeCodec) -> std::sync::MutexGuard<'static, Option<(Tested, Instant)>> {
    let i = match codec {
        NativeCodec::H264 => 0,
        NativeCodec::Hevc => 1,
    };
    LAST_TEST[i].lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whether an NVENC session that writes `codec` opens on this system. A call opens and closes one;
/// a session that opened is remembered, and a failure is remembered for [`RETEST_AFTER`]. A
/// failure because every session was taken is also forgotten when a session of the other codec
/// opens. Without the NVIDIA driver nothing is tested again.
fn usable(codec: NativeCodec) -> bool {
    if craft_hwenc_native::nvenc::driver().is_err() {
        return false;
    }
    let mut last = last_test(codec);
    match *last {
        Some((Tested::Works, _)) => true,
        Some((_, at)) if at.elapsed() < RETEST_AFTER => false,
        _ => {
            let found = match craft_hwenc_native::nvenc::available(codec) {
                Ok(()) => Tested::Works,
                Err(Error::Busy(_)) => Tested::Busy,
                Err(_) => Tested::Failed,
            };
            *last = Some((found, Instant::now()));
            found == Tested::Works
        }
    }
}

/// The encoder settings of `codec` for `params`.
fn config(codec: NativeCodec, params: &VideoParams) -> Result<Config, PluginError> {
    if params.fps_num == 0 || params.fps_den == 0 {
        return Err(PluginError::Failed(format!("the frame rate {}/{} is not valid", params.fps_num, params.fps_den)));
    }
    let fps = f64::from(params.fps_num) / f64::from(params.fps_den);
    // A float-to-integer `as` saturates.
    let gop = |seconds: u32| (fps * f64::from(seconds)).round().max(1.0) as u32;
    let (rate, keyint) = match params.rate {
        RateControl::Quality(q) => {
            if !QUALITY.contains(&q) {
                return Err(PluginError::Failed(format!("the quality level {q} is outside {} to {}", QUALITY.start(), QUALITY.end())));
            }
            (Rate::Quality(q), gop(RECORDING_KEYFRAME_S))
        }
        RateControl::Bitrate { kbps, keyframe_s } => {
            if !KBPS.contains(&kbps) {
                return Err(PluginError::Failed(format!("the video bitrate {kbps} kbps is outside {} to {} kbps", KBPS.start(), KBPS.end())));
            }
            if !KEYFRAME_S.contains(&keyframe_s) {
                return Err(PluginError::Failed(format!(
                    "the keyframe interval of {keyframe_s} seconds is outside {} to {} seconds",
                    KEYFRAME_S.start(),
                    KEYFRAME_S.end()
                )));
            }
            (Rate::Cbr(kbps), gop(keyframe_s))
        }
        RateControl::Lossless => {
            return Err(PluginError::Unavailable("NVENC (built in) cannot record losslessly; choose another encoder in Settings › Output".into()));
        }
    };
    Ok(Config { codec, input: Input::Nv12, width: params.width, height: params.height, fps: (params.fps_num, params.fps_den), rate, keyint })
}

/// A running NVENC encoder.
pub struct NvencSession {
    core: Core,
    name: &'static str,
}

impl NvencSession {
    /// The settings the session runs with.
    pub fn config(&self) -> &Config {
        self.core.config()
    }
}

fn packets(out: &mut Vec<Packet>, ps: Vec<craft_hwenc_native::nvenc::Packet>) {
    out.extend(ps.into_iter().map(|p| Packet { data: p.data, pts: p.pts, dts: p.pts, keyframe: p.keyframe }));
}

impl VideoSession for NvencSession {
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

    fn params(rate: RateControl) -> VideoParams {
        VideoParams { width: 256, height: 144, fps_num: 60, fps_den: 1, rate }
    }

    #[test]
    fn the_rate_and_keyframes_follow_the_output_settings() {
        let c = config(NativeCodec::Hevc, &params(RateControl::Quality(20))).unwrap();
        assert_eq!((c.rate, c.keyint, c.input), (Rate::Quality(20), 120, Input::Nv12));
        assert_eq!(config(NativeCodec::H264, &params(RateControl::Quality(51))).unwrap().rate, Rate::Quality(51));
        assert_eq!(config(NativeCodec::H264, &params(RateControl::Quality(1))).unwrap().rate, Rate::Quality(1));
        let c = config(NativeCodec::H264, &params(RateControl::Bitrate { kbps: 6000, keyframe_s: 4 })).unwrap();
        assert_eq!((c.rate, c.keyint), (Rate::Cbr(6000), 240));
        let p = VideoParams { fps_num: 30000, fps_den: 1001, ..params(RateControl::Quality(23)) };
        assert_eq!(config(NativeCodec::H264, &p).unwrap().keyint, 60);
    }

    #[test]
    fn settings_the_encoder_cannot_take_are_errors() {
        for rate in [
            RateControl::Quality(0),
            RateControl::Quality(52),
            RateControl::Bitrate { kbps: 50, keyframe_s: 2 },
            RateControl::Bitrate { kbps: 6000, keyframe_s: 21 },
            RateControl::Lossless,
        ] {
            assert!(config(NativeCodec::H264, &params(rate)).is_err(), "{rate:?}");
        }
        let p = VideoParams { fps_den: 0, ..params(RateControl::Quality(23)) };
        assert!(config(NativeCodec::Hevc, &p).is_err());
        assert!(matches!(Nvenc::H264.session(&params(RateControl::Lossless)), Err(PluginError::Unavailable(_))));
    }

    #[test]
    fn the_hardware_candidates_map_to_their_built_in_twins() {
        assert_eq!(Nvenc::for_candidate("nvenc_h264").map(|n| n.id()), Some("nvenc_native_h264"));
        assert_eq!(Nvenc::for_candidate("nvenc_hevc").map(|n| (n.id(), n.codec())), Some(("nvenc_native_hevc", Codec::Hevc)));
        assert_eq!(Nvenc::for_candidate("nvenc_av1"), None);
        assert_eq!(Nvenc::for_candidate("amf_h264"), None);
    }

    #[test]
    fn it_runs_only_where_built_in_encoders_run_and_a_session_opens() {
        let stream = EncoderEnv { ffmpeg_encoders: None, builtin: false };
        assert_eq!(Nvenc::H264.availability(&stream), Availability::NotRunHere);
        let recording = EncoderEnv { ffmpeg_encoders: None, builtin: true };
        let expected = if craft_hwenc_native::nvenc::available(NativeCodec::Hevc).is_ok() { Availability::Ready } else { Availability::NotRunHere };
        assert_eq!(Nvenc::HEVC.availability(&recording), expected);
    }

    #[test]
    fn without_a_gpu_opening_is_an_error_and_with_one_frames_come_back_in_order() {
        let mut s = match Nvenc::HEVC.open(&Settings::new(), &params(RateControl::Quality(23))) {
            Ok(s) => s,
            Err(PluginError::Unavailable(why)) => return assert!(why.starts_with("NVIDIA NVENC HEVC (built in)"), "{why}"),
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
