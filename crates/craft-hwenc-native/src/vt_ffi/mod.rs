//! Every call into Apple's VideoToolbox: one compression session with a hardware encoder required
//! ([`session`]). The items re-exported here are safe. On other systems opening an encoder is an
//! error that says VideoToolbox runs on macOS only.

#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use crate::{Codec, Rate};

#[cfg(target_os = "macos")]
mod session;

#[cfg(not(target_os = "macos"))]
pub(crate) use none::Session;
#[cfg(target_os = "macos")]
pub(crate) use session::Session;

/// What a compression session is set up for.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Params {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub fps: (u32, u32),
    pub rate: Rate,
    /// Frames from one keyframe to the next.
    pub gop: u32,
}

/// One compressed sample: 4-byte length-prefixed NAL units, the time it carries in units of 1 /
/// the frame rate's numerator, and the parameter sets of its format description.
pub(crate) struct Output {
    pub data: Vec<u8>,
    pub time: Option<i64>,
    pub sets: Vec<Vec<u8>>,
}

/// Why no session opened.
#[derive(Debug)]
pub(crate) enum OpenError {
    /// No hardware encoder for this codec on this system.
    NoDevice(String),
    /// The hardware encoder does not take this stream.
    Unsupported(String),
    /// The hardware encoder is taken by other sessions now (`kVTVideoEncoderNotAvailableNowErr`).
    Busy(String),
}

/// `kVTVideoEncoderNotAvailableNowErr`: the encoder exists but cannot take another session now.
pub(crate) const ENCODER_NOT_AVAILABLE_NOW: i32 = -12915;

/// The error for a session that `status` stopped from opening: [`OpenError::Busy`] when the
/// encoder is taken now, and otherwise `other`.
pub(crate) fn open_error(status: i32, why: String, other: fn(String) -> OpenError) -> OpenError {
    if status == ENCODER_NOT_AVAILABLE_NOW { OpenError::Busy(why) } else { other(why) }
}

/// The largest compressed frame read from a sample.
pub(crate) const MAX_FRAME_BYTES: usize = 256 << 20;

/// The most parameter sets read from one format description.
pub(crate) const MAX_SETS: usize = 16;

/// The session's quality for level `q` (1 to 51, lower is better): 0.0 to 0.98, higher is better.
pub(crate) fn quality(q: u32) -> f64 {
    f64::from(100u32.saturating_sub(q.saturating_mul(2))) / 100.0
}

/// The bitrate in bit/s of `kbps` kbit/s, at most `i32::MAX`.
pub(crate) fn bitrate_bps(kbps: u32) -> i32 {
    i32::try_from(u64::from(kbps).saturating_mul(1000)).unwrap_or(i32::MAX)
}

/// The average bitrate a quality-mode session gets from an encoder without a quality setting: a
/// tenth of a bit per pixel per frame, at most `i32::MAX`.
pub(crate) fn nominal_bitrate(p: &Params) -> i32 {
    let pixels = u64::from(p.width) * u64::from(p.height);
    let bps = pixels.saturating_mul(u64::from(p.fps.0)) / u64::from(p.fps.1.max(1)) / 10;
    i32::try_from(bps).unwrap_or(i32::MAX).max(1)
}

/// `value` in units of 1 / `from` s, in units of 1 / `to` s rounded to the nearest, or `None` when
/// a timescale is not positive or the result does not fit.
pub(crate) fn rescale(value: i64, from: i32, to: i32) -> Option<i64> {
    if from <= 0 || to <= 0 {
        return None;
    }
    let (num, den) = (i128::from(value) * i128::from(to), i128::from(from));
    i64::try_from((2 * num + den).div_euclid(2 * den)).ok()
}

#[cfg(not(target_os = "macos"))]
mod none {
    use super::{OpenError, Output, Params};

    /// No compression session: VideoToolbox runs on macOS only.
    pub struct Session;

    impl Session {
        pub fn open(_: &Params) -> Result<Session, OpenError> {
            Err(OpenError::NoDevice("VideoToolbox runs on macOS only".into()))
        }

        pub fn encode(&mut self, _: &[u8], _: i64, _: i64, _: &mut Vec<Output>) -> Result<(), String> {
            Err("VideoToolbox runs on macOS only".into())
        }

        pub fn drain(&mut self, _: &mut Vec<Output>) -> Result<(), String> {
            Ok(())
        }
    }
}
