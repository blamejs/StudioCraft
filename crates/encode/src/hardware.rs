//! What the built-in hardware encoders share: the encoder that runs a `craft_hwenc` candidate in
//! this process ([`for_candidate`]), the settings an output asks of them, and the cache of their
//! session tests.
//!
//! - Quality mode is constant quality ([`QUALITY`]) with a keyframe every
//!   [`RECORDING_KEYFRAME_S`] seconds. Bitrate mode is constant bitrate ([`KBPS`]) with the
//!   requested keyframe interval ([`KEYFRAME_S`]). A level, bitrate or interval outside those
//!   ranges is an error, and lossless recording is not offered.
//! - The encoders write no B-frames, so each packet's decoding time is its presentation time.

use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use craft_hwenc_native::{Codec as NativeCodec, Color, Config, Input, Rate};
use studiocraft_plugin::{Encoder, Packet, PluginError, RateControl, VideoParams, VideoSession};

use crate::h264::{KBPS, KEYFRAME_S, RECORDING_KEYFRAME_S};
use crate::mf::MediaFoundation;
use crate::nvenc::Nvenc;
use crate::vaapi::VaApi;
use crate::vt::VideoToolbox;

/// The quality levels the hardware encoders take; lower is better.
pub const QUALITY: std::ops::RangeInclusive<u32> = 1..=51;

/// Why [`Hardware::try_open`] did not open a session.
#[derive(Debug)]
pub enum SessionError {
    /// Every encoding session of the GPU is taken by other programs; a later try can work.
    Busy(String),
    /// Any other reason.
    Other(PluginError),
}

/// A built-in hardware encoder.
pub trait Hardware: Encoder {
    /// Open a session for `params`, telling a GPU whose sessions are all taken apart from other
    /// failures; the hardware encoder tests use it.
    fn try_open(&self, params: &VideoParams) -> Result<Box<dyn VideoSession>, SessionError>;
}

static NVENC_H264: Nvenc = Nvenc::H264;
static NVENC_HEVC: Nvenc = Nvenc::HEVC;
static NVENC_AV1: Nvenc = Nvenc::AV1;
static MF_H264: MediaFoundation = MediaFoundation::H264;
static MF_HEVC: MediaFoundation = MediaFoundation::HEVC;
static VT_H264: VideoToolbox = VideoToolbox::H264;
static VT_HEVC: VideoToolbox = VideoToolbox::HEVC;
static VAAPI_H264: VaApi = VaApi::H264;
static VAAPI_HEVC: VaApi = VaApi::HEVC;
static VAAPI_AV1: VaApi = VaApi::AV1;

/// The built-in encoder that runs `candidate` (a `craft_hwenc` candidate id) in this process, if
/// any.
pub fn for_candidate(candidate: &str) -> Option<&'static dyn Hardware> {
    match candidate {
        "nvenc_h264" => Some(&NVENC_H264),
        "nvenc_hevc" => Some(&NVENC_HEVC),
        "nvenc_av1" => Some(&NVENC_AV1),
        "mf_h264" => Some(&MF_H264),
        "mf_hevc" => Some(&MF_HEVC),
        "vt_h264" => Some(&VT_H264),
        "vt_hevc" => Some(&VT_HEVC),
        "vaapi_h264" => Some(&VAAPI_H264),
        "vaapi_hevc" => Some(&VAAPI_HEVC),
        "vaapi_av1" => Some(&VAAPI_AV1),
        _ => None,
    }
}

/// How long a failed session test stands before the next call tests again.
pub(crate) const RETEST_AFTER: Duration = Duration::from_secs(30);

/// What a session test found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tested {
    Works,
    /// Every session was taken by other programs.
    Busy,
    /// No device, or a GPU whose encoder does not write the codec.
    Failed,
}

/// The last answer of a session test for each codec, or of a session that opened, and when it was
/// found.
pub(crate) struct Tests([Mutex<Option<(Tested, Instant)>>; 3]);

/// Every codec [`Tests`] keeps an answer for, in slot order.
const CODECS: [NativeCodec; 3] = [NativeCodec::H264, NativeCodec::Hevc, NativeCodec::Av1];

impl Tests {
    pub(crate) const fn new() -> Tests {
        Tests([Mutex::new(None), Mutex::new(None), Mutex::new(None)])
    }

    fn slot(&self, codec: NativeCodec) -> MutexGuard<'_, Option<(Tested, Instant)>> {
        let i = match codec {
            NativeCodec::H264 => 0,
            NativeCodec::Hevc => 1,
            NativeCodec::Av1 => 2,
        };
        self.0[i].lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether a session that writes `codec` opens, running `test` when no answer stands: a
    /// session that opened is remembered, and a failure is remembered for [`RETEST_AFTER`]. The
    /// lock is not held while `test` runs, so a session that opens meanwhile does not wait for
    /// it. When the answer changed while `test` ran (a session opened, or a busy answer was
    /// cleared), the test's answer is not stored over it.
    pub(crate) fn usable(&self, codec: NativeCodec, test: impl FnOnce() -> Tested) -> bool {
        let before = *self.slot(codec);
        match before {
            Some((Tested::Works, _)) => return true,
            Some((_, at)) if at.elapsed() < RETEST_AFTER => return false,
            _ => {}
        }
        let found = test();
        let mut last = self.slot(codec);
        if *last != before {
            return found == Tested::Works || matches!(*last, Some((Tested::Works, _)));
        }
        *last = Some((found, Instant::now()));
        found == Tested::Works
    }

    /// A session that writes `codec` opened: the codec works, and each other one is tested again
    /// if it failed only because every session was taken.
    pub(crate) fn opened(&self, codec: NativeCodec) {
        *self.slot(codec) = Some((Tested::Works, Instant::now()));
        for other in CODECS.into_iter().filter(|&c| c != codec) {
            let mut o = self.slot(other);
            if matches!(*o, Some((Tested::Busy, _))) {
                *o = None;
            }
        }
    }
}

/// The encoder settings of `codec` for `params`. `encoder` names the encoder in the error for a
/// lossless recording.
pub(crate) fn config(codec: NativeCodec, params: &VideoParams, encoder: &str) -> Result<Config, PluginError> {
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
            return Err(PluginError::Unavailable(format!("{encoder} cannot record losslessly; choose another encoder in Settings › Output")));
        }
    };
    Ok(Config {
        codec,
        input: Input::Nv12,
        width: params.width,
        height: params.height,
        fps: (params.fps_num, params.fps_den),
        rate,
        keyint,
        color: Color::Bt709,
    })
}

/// The packets of a built-in hardware encoder as the plugin's packets.
pub(crate) fn packets(out: &mut Vec<Packet>, ps: Vec<craft_hwenc_native::Packet>) {
    out.extend(ps.into_iter().map(|p| Packet { data: p.data, pts: p.pts, dts: p.pts, keyframe: p.keyframe }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_built_in_twin_runs_its_candidate() {
        let ids = |c: &str| for_candidate(c).map(|e| (e.id(), e.codec()));
        assert_eq!(ids("nvenc_h264"), Some(("nvenc_native_h264", studiocraft_plugin::Codec::H264)));
        assert_eq!(ids("nvenc_hevc"), Some(("nvenc_native_hevc", studiocraft_plugin::Codec::Hevc)));
        assert_eq!(ids("mf_h264"), Some(("mf_native_h264", studiocraft_plugin::Codec::H264)));
        assert_eq!(ids("mf_hevc"), Some(("mf_native_hevc", studiocraft_plugin::Codec::Hevc)));
        assert_eq!(ids("vt_h264"), Some(("vt_native_h264", studiocraft_plugin::Codec::H264)));
        assert_eq!(ids("vt_hevc"), Some(("vt_native_hevc", studiocraft_plugin::Codec::Hevc)));
        assert_eq!(ids("vaapi_h264"), Some(("vaapi_native_h264", studiocraft_plugin::Codec::H264)));
        assert_eq!(ids("vaapi_hevc"), Some(("vaapi_native_hevc", studiocraft_plugin::Codec::Hevc)));
        assert_eq!(ids("vaapi_av1"), Some(("vaapi_native_av1", studiocraft_plugin::Codec::Av1)));
        assert_eq!(ids("nvenc_av1"), Some(("nvenc_native_av1", studiocraft_plugin::Codec::Av1)));
        for none in ["amf_h264", "qsv_hevc", "qsv_av1", "x264"] {
            assert!(for_candidate(none).is_none(), "{none}");
        }
    }

    #[test]
    fn a_failed_test_stands_for_a_while_and_a_session_that_opens_clears_a_busy_one() {
        let tests = Tests::new();
        let mut runs = 0;
        assert!(!tests.usable(NativeCodec::H264, || {
            runs += 1;
            Tested::Busy
        }));
        assert!(!tests.usable(NativeCodec::H264, || {
            runs += 1;
            Tested::Works
        }));
        assert_eq!(runs, 1, "the busy answer stands");
        tests.opened(NativeCodec::Hevc);
        assert!(tests.usable(NativeCodec::Hevc, || Tested::Failed), "a session that opened is remembered");
        assert!(tests.usable(NativeCodec::H264, || Tested::Works), "the busy answer was cleared");
        let failed = Tests::new();
        assert!(!failed.usable(NativeCodec::Hevc, || Tested::Failed));
        failed.opened(NativeCodec::H264);
        assert!(!failed.usable(NativeCodec::Hevc, || Tested::Works), "a failure that was not a full GPU stands");
    }

    #[test]
    fn av1_keeps_its_own_answer_and_any_session_that_opens_clears_its_busy_one() {
        let tests = Tests::new();
        assert!(!tests.usable(NativeCodec::Av1, || Tested::Busy));
        assert!(!tests.usable(NativeCodec::Hevc, || Tested::Busy));
        assert!(tests.usable(NativeCodec::H264, || Tested::Works), "H.264 has its own answer");
        tests.opened(NativeCodec::H264);
        assert!(tests.usable(NativeCodec::Av1, || Tested::Works), "the AV1 busy answer was cleared");
        assert!(tests.usable(NativeCodec::Hevc, || Tested::Works), "the HEVC busy answer was cleared");
        let failed = Tests::new();
        assert!(!failed.usable(NativeCodec::Av1, || Tested::Failed));
        failed.opened(NativeCodec::Av1);
        assert!(failed.usable(NativeCodec::Av1, || Tested::Failed), "a session that opened replaces the failure");
        assert!(failed.0[0].try_lock().is_ok_and(|g| g.is_none()), "H.264's slot is untouched");
    }

    #[test]
    fn a_test_runs_without_the_lock_and_a_session_that_opens_meanwhile_is_kept() {
        let tests = Tests::new();
        assert!(tests.usable(NativeCodec::H264, || {
            assert!(tests.0[0].try_lock().is_ok(), "the lock is free while the test runs");
            tests.opened(NativeCodec::H264);
            Tested::Failed
        }));
        assert!(tests.usable(NativeCodec::H264, || Tested::Failed), "the session that opened during the test is kept");
        // A busy answer cleared while a retest runs is not put back by that retest.
        let busy = Tests::new();
        let long_ago = Instant::now().checked_sub(RETEST_AFTER * 2).unwrap_or_else(Instant::now);
        *busy.slot(NativeCodec::Hevc) = Some((Tested::Busy, long_ago));
        assert!(!busy.usable(NativeCodec::Hevc, || {
            busy.opened(NativeCodec::H264);
            Tested::Busy
        }));
        assert!(busy.slot(NativeCodec::Hevc).is_none(), "the next call tests again");
    }
}
