//! The built-in H.264 encoder (ITU-T H.264): NV12 frames in, length-prefixed access units out,
//! with the `avcC` record a container stores with the track.
//!
//! - The frames are BT.709 at limited range, as StudioCraft's NV12 conversion writes them
//!   (`crates/render/src/shaders.rs`), and the stream's VUI says so.
//! - A size that is not a multiple of 16 is coded at the next multiple and cropped back.
//! - A picture may have at most [`MAX_MACROBLOCKS`] macroblocks, the limit of the highest level.
//! - Quality mode is constant quality with a keyframe every [`RECORDING_KEYFRAME_S`] seconds and up
//!   to two B-frames. Bitrate mode is constant bitrate ([`KBPS`]) with the requested keyframe
//!   interval ([`KEYFRAME_S`]), no B-frames and no extra keyframes at scene cuts, so a stream has a
//!   fixed group of pictures. A bitrate or interval outside those ranges is an error.
//! - The encoder runs on `available_parallelism() - 2` threads (at least one), leaving cores to
//!   the compositor, capture and mixer threads.

use craft_video::h264::{ColorConfig, Encoder as Core, EncoderConfig, PacketFormat, Preset, Profile, RateControl as Rate, YuvFrame};
use studiocraft_model::{ChoiceOption, Field, FieldKind, Settings, SettingsSchema};
use studiocraft_plugin::{Codec, Encoder, Packet, Plugin, PluginError, RateControl, VideoParams, VideoSession};

/// Most macroblocks of 16×16 pixels in one picture (ITU-T H.264 Table A-1, level 6.2): 8192×4320
/// fits, 8192×8192 does not.
pub const MAX_MACROBLOCKS: u64 = 139_264;
/// Largest side the encoder takes, in pixels.
pub const MAX_SIDE: u32 = 16_384;
/// Seconds between keyframes in quality mode. A Matroska file starts its clusters at keyframes, so
/// a recording cut short loses at most about this much.
pub const RECORDING_KEYFRAME_S: u32 = 2;
/// The bitrates bitrate mode takes, in kbit/s: the range Settings › Stream offers.
pub const KBPS: std::ops::RangeInclusive<u32> = 100..=200_000;
/// The keyframe intervals bitrate mode takes, in seconds.
pub const KEYFRAME_S: std::ops::RangeInclusive<u32> = 1..=20;
/// The preset values, fastest first.
pub const PRESETS: [&str; 3] = ["speed", "balanced", "quality"];

/// The built-in H.264 encoder.
pub struct H264;

impl Plugin for H264 {
    fn id(&self) -> &'static str {
        "h264"
    }
    fn name(&self) -> &'static str {
        "H.264 (built in)"
    }
    fn schema(&self) -> SettingsSchema {
        let labels = ["Speed", "Balanced", "Quality"];
        let options = PRESETS.iter().zip(labels).map(|(v, l)| ChoiceOption::new(*v, l)).collect();
        SettingsSchema::new(vec![Field::new("preset", "CPU usage preset (faster uses less CPU)", FieldKind::Choice { options }, "speed")])
    }
}

impl Encoder for H264 {
    fn codec(&self) -> Codec {
        Codec::H264
    }
    fn open(&self, settings: &Settings, params: &VideoParams) -> Result<Box<dyn VideoSession>, PluginError> {
        Ok(Box::new(H264Session::new(config(settings, params)?)?))
    }
}

/// Whether H.264 can code `width` × `height`.
pub fn check_size(width: u32, height: u32) -> Result<(), PluginError> {
    if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return Err(PluginError::Failed(format!("the output size {width}×{height} must be even on both sides")));
    }
    let macroblocks = u64::from(width.div_ceil(16)) * u64::from(height.div_ceil(16));
    if width > MAX_SIDE || height > MAX_SIDE || macroblocks > MAX_MACROBLOCKS {
        return Err(PluginError::Failed(format!(
            "H.264 cannot code {width}×{height}: a picture has at most {MAX_MACROBLOCKS} macroblocks of 16×16 pixels (8192×4320 fits); \
             choose a smaller output size in Settings › Video"
        )));
    }
    Ok(())
}

/// The encoder settings for `params` with the preset in `settings`. Without a preset the fastest
/// one applies; a preset that is not one of [`PRESETS`] is an error.
pub fn config(settings: &Settings, params: &VideoParams) -> Result<EncoderConfig, PluginError> {
    check_size(params.width, params.height)?;
    if params.fps_num == 0 || params.fps_den == 0 {
        return Err(PluginError::Failed(format!("the frame rate {}/{} is not valid", params.fps_num, params.fps_den)));
    }
    let fps = f64::from(params.fps_num) / f64::from(params.fps_den);
    // A float-to-integer `as` saturates.
    let gop = |seconds: u32| (fps * f64::from(seconds)).round().max(1.0) as u32;
    let mut c = EncoderConfig::new(params.width, params.height, params.fps_num, params.fps_den);
    c.profile = Profile::High;
    c.preset = match settings.get("preset").map(|v| v.as_str()) {
        None | Some(Some("speed")) => Preset::Speed,
        Some(Some("balanced")) => Preset::Balanced,
        Some(Some("quality")) => Preset::Quality,
        Some(other) => {
            let shown = other.map_or_else(|| "a value that is not text".to_string(), |p| format!("`{p}`"));
            return Err(PluginError::Failed(format!(
                "the H.264 (built in) preset {shown} is not one of {}; choose a preset in Settings › Output",
                PRESETS.join(", ")
            )));
        }
    };
    match params.rate {
        RateControl::Quality(q) => {
            c.rate = Rate::Crf(q.min(51) as f32);
            c.keyint = gop(RECORDING_KEYFRAME_S);
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
            c.rate = Rate::Cbr { kbps };
            c.keyint = gop(keyframe_s);
            c.bframes = 0;
            c.scenecut = false;
        }
        RateControl::Lossless => {
            return Err(PluginError::Unavailable("H.264 (built in) cannot record losslessly; choose another encoder in Settings › Output".into()));
        }
    }
    c.color = ColorConfig { primaries: 1, transfer: 1, matrix: 1, full_range: false };
    c.format = PacketFormat::LengthPrefixed;
    c.aud = false;
    c.threads = std::thread::available_parallelism().map_or(1, |n| n.get()).saturating_sub(2).max(1);
    c.writer = format!("StudioCraft {}", env!("CARGO_PKG_VERSION"));
    Ok(c)
}

/// A running H.264 encoder.
pub struct H264Session {
    core: Core,
    width: usize,
    height: usize,
    /// The chroma planes of the frame being encoded, split out of NV12's interleaved plane.
    u: Vec<u8>,
    v: Vec<u8>,
}

impl H264Session {
    pub fn new(config: EncoderConfig) -> Result<H264Session, PluginError> {
        check_size(config.width, config.height)?;
        let (width, height) = (config.width as usize, config.height as usize);
        let core = Core::new(config).map_err(|e| PluginError::Failed(format!("the H.264 encoder could not start: {e}")))?;
        let chroma = (width / 2) * (height / 2);
        Ok(H264Session { core, width, height, u: vec![0; chroma], v: vec![0; chroma] })
    }

    /// The settings the session runs with.
    pub fn config(&self) -> &EncoderConfig {
        self.core.config()
    }
}

/// Split the interleaved chroma plane `uv` (U, V, U, V, ...) into `u` and `v`.
fn split_chroma(uv: &[u8], u: &mut [u8], v: &mut [u8]) {
    for ((&[a, b], u), v) in uv.as_chunks::<2>().0.iter().zip(u.iter_mut()).zip(v.iter_mut()) {
        *u = a;
        *v = b;
    }
}

fn packet(p: craft_video::h264::Packet) -> Packet {
    Packet { data: p.data, pts: p.pts, dts: p.dts, keyframe: p.keyframe }
}

impl VideoSession for H264Session {
    fn codec_private(&self) -> Vec<u8> {
        self.core.avcc()
    }

    fn encode(&mut self, nv12: &[u8], pts: i64, out: &mut Vec<Packet>) -> Result<(), PluginError> {
        let luma = self.width * self.height;
        if nv12.len() != luma + 2 * self.u.len() {
            return Err(PluginError::Failed(format!("a frame of {} bytes is not an NV12 frame of {}×{}", nv12.len(), self.width, self.height)));
        }
        let (y, uv) = nv12.split_at(luma);
        split_chroma(uv, &mut self.u, &mut self.v);
        let frame = YuvFrame { y, u: &self.u, v: &self.v, y_stride: self.width, uv_stride: self.width / 2 };
        let packets = self.core.encode(&frame, pts).map_err(|e| PluginError::Failed(format!("the H.264 encoder stopped: {e}")))?;
        out.extend(packets.into_iter().map(packet));
        Ok(())
    }

    fn flush(&mut self, out: &mut Vec<Packet>) -> Result<(), PluginError> {
        out.extend(self.core.flush().into_iter().map(packet));
        Ok(())
    }
}

#[cfg(test)]
mod tests;
