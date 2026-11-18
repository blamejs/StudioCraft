//! Every call into Windows Media Foundation: `mfplat.dll`, loaded at run time and started once
//! ([`api`]), and one hardware encoder MFT driven through its asynchronous events
//! ([`transform`]). The items re-exported here are safe. On other systems opening an encoder is an
//! error that says Media Foundation runs on Windows only.

#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

use crate::{Codec, Rate};

#[cfg(target_os = "windows")]
mod api;
#[cfg(target_os = "windows")]
mod transform;

#[cfg(target_os = "windows")]
pub(crate) use api::loaded;
#[cfg(not(target_os = "windows"))]
pub(crate) use none::{Transform, loaded};
#[cfg(target_os = "windows")]
pub(crate) use transform::Transform;

/// What an encoder MFT is set up for.
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

/// One sample an encoder MFT returned: Annex B, and the sample time it carries when it has one.
pub(crate) struct Output {
    pub data: Vec<u8>,
    pub time: Option<i64>,
}

/// The encoder's quality setting for level `q` (1 to 51, lower is better): 0 to 100, higher is
/// better.
pub(crate) fn quality(q: u32) -> u32 {
    100u32.saturating_sub(q.saturating_mul(2))
}

/// The bitrate a quality-mode output type names: a tenth of a bit per pixel per frame.
pub(crate) fn nominal_bitrate(p: &Params) -> u32 {
    let pixels = u64::from(p.width) * u64::from(p.height);
    let bps = pixels.saturating_mul(u64::from(p.fps.0)) / u64::from(p.fps.1.max(1)) / 10;
    u32::try_from(bps).unwrap_or(u32::MAX).max(1)
}

/// The largest parameter-set blob read from an output type; a larger one is not read.
pub(crate) const MAX_HEADER: u32 = 1 << 16;

/// How many output type changes in a row one output read follows before it fails.
pub(crate) const MAX_STREAM_CHANGES: u32 = 4;

/// How many samples one output read takes while the encoder marks them incomplete.
pub(crate) const MAX_SAMPLES_PER_OUTPUT: u32 = 64;

/// The buffer size of the output samples this side allocates for a `width` by `height` stream,
/// from the size the encoder asks for (`asked`): at least one raw NV12 frame and 1 MiB. Asking for
/// more than two raw frames or 64 MiB, whichever is larger, is an error.
pub(crate) fn output_size(asked: u32, width: u32, height: u32) -> Result<u32, String> {
    let frame = width.saturating_mul(height).saturating_mul(3) / 2;
    let limit = frame.saturating_mul(2).max(64 << 20);
    if asked > limit {
        return Err(format!("the encoder asks for {asked}-byte output buffers, more than {limit} bytes"));
    }
    Ok(asked.max(frame).max(1 << 20))
}

/// Why no encoder MFT opened.
pub(crate) enum OpenError {
    /// Media Foundation, or a hardware encoder for the codec, is missing.
    NoDevice(String),
    /// None opened, and at least one lacked the hardware resources to start streaming: other
    /// programs may hold its sessions, and a later try can work.
    Busy(String),
    /// Every hardware encoder for the codec refused the configuration.
    Unsupported(String),
}

#[cfg(not(target_os = "windows"))]
mod none {
    use super::{OpenError, Output, Params};

    /// Whether `mfplat.dll` loaded and started: never, off Windows.
    pub fn loaded() -> Result<(), String> {
        Err("Media Foundation runs on Windows only".into())
    }

    /// No encoder: Media Foundation runs on Windows only.
    pub struct Transform;

    impl Transform {
        pub fn open(_: &Params) -> Result<Transform, OpenError> {
            Err(OpenError::NoDevice("Media Foundation runs on Windows only".into()))
        }

        pub fn sequence_header(&self) -> Option<&[u8]> {
            None
        }

        pub fn encode(&mut self, _: &[u8], _: i64, _: i64, _: &mut Vec<Output>) -> Result<(), String> {
            Err("Media Foundation runs on Windows only".into())
        }

        pub fn drain(&mut self, _: &mut Vec<Output>) -> Result<(), String> {
            Ok(())
        }
    }
}
