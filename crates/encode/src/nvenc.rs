//! The NVENC encoders that run through the NVIDIA driver in StudioCraft's own process: H.264 and
//! HEVC from the compositor's NV12 frames, with the `avcC` or `hvcC` record a container stores.
//!
//! - The rate control, the keyframe interval and the limits on them are the ones every built-in
//!   hardware encoder takes ([`crate::hardware`]).
//! - The encoder writes no B-frames, so each packet's decoding time is its presentation time.
//! - The stream's VUI says BT.709 at limited range, as StudioCraft's NV12 conversion writes it.
//! - On Windows, [`Nvenc::session_on`] opens a session on an export ring's Direct3D 11 device.
//!   It takes the compositor's frames as the ring's textures ([`VideoSession::encode_shared`]),
//!   encodes each one once its copy has finished, and hands the texture back once its packet is
//!   out. A session that cannot open there, or that fails on a frame, records
//!   [`Fallback::NoEncoder`] on the ring, and the compositor sends its frames through memory again.

use craft_gpu_interop::{ExportFrame, ExportRing, Fallback, Feed};
use craft_hwenc_native::Codec as NativeCodec;
use craft_hwenc_native::nvenc::{Config, Encoder as Core, Error};
use studiocraft_model::{Settings, SettingsSchema};
use studiocraft_plugin::{Availability, Codec, Encoder, EncoderEnv, Packet, Plugin, PluginError, VideoParams, VideoSession};

use crate::hardware::{self, Hardware, Tested, Tests, packets};
pub use crate::hardware::{QUALITY, SessionError};

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
    /// NVENC AV1, registered as `nvenc_native_av1`.
    pub const AV1: Nvenc = Nvenc { codec: NativeCodec::Av1 };

    /// The encoder that runs `candidate` (a `craft_hwenc` candidate id) in this process, if any.
    pub fn for_candidate(candidate: &str) -> Option<Nvenc> {
        match candidate {
            "nvenc_h264" => Some(Nvenc::H264),
            "nvenc_hevc" => Some(Nvenc::HEVC),
            "nvenc_av1" => Some(Nvenc::AV1),
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
        LAST_TEST.opened(self.codec);
        Ok(NvencSession { core, name: self.name(), shared: None })
    }

    /// Open a session for `params` that takes its frames from `ring` on the GPU. The ring must
    /// have the size of `params`. When the session cannot open on the ring's device (no NVIDIA
    /// encoder on that adapter, every session taken, another size, or a system other than
    /// Windows), [`Fallback::NoEncoder`] is recorded on the ring and the error returned. The
    /// session takes only frames on the GPU: when the ring records a fallback, the frames the
    /// compositor reads back go to a new session from [`Nvenc::session`].
    pub fn session_on(&self, params: &VideoParams, ring: &std::sync::Arc<ExportRing>) -> Result<NvencSession, PluginError> {
        let opened = self.open_on(params, ring);
        if opened.is_err() {
            ring.refuse(Fallback::NoEncoder);
        }
        opened
    }

    fn open_on(&self, params: &VideoParams, ring: &std::sync::Arc<ExportRing>) -> Result<NvencSession, PluginError> {
        if (ring.width(), ring.height()) != (params.width, params.height) {
            return Err(PluginError::Unavailable(format!(
                "{} was opened for {}x{} frames, and the compositor's are {}x{}",
                self.name(),
                params.width,
                params.height,
                ring.width(),
                ring.height()
            )));
        }
        let cfg = config(self.codec, params)?;
        #[cfg(target_os = "windows")]
        {
            let Some(device) = ring.device() else {
                return Err(PluginError::Unavailable(format!("{} has no graphics device to take frames from", self.name())));
            };
            let core = Core::on_d3d11(&cfg, &device).map_err(|e| self.error(e))?;
            LAST_TEST.opened(self.codec);
            Ok(NvencSession { core, name: self.name(), shared: Some(Shared { ring: std::sync::Arc::clone(ring), feed: Feed::default() }) })
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = cfg;
            Err(PluginError::Unavailable(format!("{} takes frames on the GPU only on Windows", self.name())))
        }
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
            NativeCodec::Av1 => "nvenc_native_av1",
        }
    }
    fn name(&self) -> &'static str {
        match self.codec {
            NativeCodec::H264 => "NVIDIA NVENC H.264 (built in)",
            NativeCodec::Hevc => "NVIDIA NVENC HEVC (built in)",
            NativeCodec::Av1 => "NVIDIA NVENC AV1 (built in)",
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
            NativeCodec::Av1 => Codec::Av1,
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

impl Hardware for Nvenc {
    fn try_open(&self, params: &VideoParams) -> Result<Box<dyn VideoSession>, SessionError> {
        Ok(Box::new(self.try_session(params)?))
    }
}

/// The session tests of NVENC and the sessions that opened.
static LAST_TEST: Tests = Tests::new();

/// Whether an NVENC session that writes `codec` opens on this system. A call opens and closes one;
/// a session that opened is remembered, and a failure is remembered for
/// [`hardware::RETEST_AFTER`]. A failure because every session was taken is also forgotten when a
/// session of the other codec opens. Without the NVIDIA driver nothing is tested again.
fn usable(codec: NativeCodec) -> bool {
    if craft_hwenc_native::nvenc::driver().is_err() {
        return false;
    }
    LAST_TEST.usable(codec, || match craft_hwenc_native::nvenc::available(codec) {
        Ok(()) => Tested::Works,
        Err(Error::Busy(_)) => Tested::Busy,
        Err(_) => Tested::Failed,
    })
}

/// The encoder settings of `codec` for `params`.
fn config(codec: NativeCodec, params: &VideoParams) -> Result<Config, PluginError> {
    hardware::config(codec, params, "NVENC (built in)")
}

/// How long a flush of a session on the GPU waits for the copies of its last frames.
const FLUSH_WAIT: std::time::Duration = std::time::Duration::from_millis(250);

/// A running NVENC encoder.
pub struct NvencSession {
    core: Core,
    name: &'static str,
    /// The ring a session on the GPU takes its frames from, and the frames it holds. Declared
    /// after `core`: it is dropped after the encoder.
    shared: Option<Shared>,
}

// Only Windows opens sessions on the GPU.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
struct Shared {
    ring: std::sync::Arc<ExportRing>,
    feed: Feed,
}

impl NvencSession {
    /// The settings the session runs with.
    pub fn config(&self) -> &Config {
        self.core.config()
    }

    /// Encode the frames whose copies have finished, oldest first, and hand back the textures
    /// whose packets came out. It returns at the first frame whose copy is still running.
    #[cfg(target_os = "windows")]
    fn encode_ready(&mut self, out: &mut Vec<Packet>) -> Result<(), PluginError> {
        let Some(shared) = &mut self.shared else { return Ok(()) };
        if shared.ring.fallback() == Some(Fallback::DeviceRemoved) {
            return Err(PluginError::Failed(format!("{} stopped: {}", self.name, Fallback::DeviceRemoved)));
        }
        while let Some((frame, pts)) = shared.feed.take_ready() {
            let Some(texture) = shared.ring.texture(frame.slot()) else {
                shared.ring.refuse(Fallback::NoEncoder);
                return Err(PluginError::Failed(format!("{} was given a frame outside its ring", self.name)));
            };
            let ps = match self.core.encode_texture(&texture, pts) {
                Ok(ps) => ps,
                Err(e) => {
                    // The compositor reads its frames back through memory again.
                    shared.ring.refuse(Fallback::NoEncoder);
                    return Err(PluginError::Failed(format!("{} stopped: {e}", self.name)));
                }
            };
            // The encoder gives one packet per picture, in the order it took them.
            shared.feed.out(ps.len());
            packets(out, ps);
        }
        Ok(())
    }

    #[cfg(not(target_os = "windows"))]
    fn encode_ready(&mut self, _out: &mut Vec<Packet>) -> Result<(), PluginError> {
        Ok(())
    }
}

impl VideoSession for NvencSession {
    fn codec_private(&self) -> Vec<u8> {
        self.core.codec_private().to_vec()
    }

    fn encode(&mut self, nv12: &[u8], pts: i64, out: &mut Vec<Packet>) -> Result<(), PluginError> {
        if self.shared.is_some() {
            // Once its ring records a fallback, the output opens a session with
            // `Nvenc::session` for the frames the compositor reads back.
            return Err(PluginError::Unavailable(format!("{} takes its frames on the GPU; frames in memory need a new session", self.name)));
        }
        let ps = self.core.encode(nv12, pts).map_err(|e| PluginError::Failed(format!("{} stopped: {e}", self.name)))?;
        packets(out, ps);
        Ok(())
    }

    fn encode_shared(&mut self, frame: ExportFrame, pts: i64, out: &mut Vec<Packet>) -> Result<(), PluginError> {
        let Some(shared) = &mut self.shared else {
            return Err(PluginError::Unavailable(format!("{} takes its frames from memory", self.name)));
        };
        if frame.ring().id() != shared.ring.id() {
            return Err(PluginError::Failed(format!("{} was given a frame of another ring", self.name)));
        }
        if !shared.feed.accepts(&frame) {
            return Err(PluginError::Failed(format!("{} was given a frame it was given before", self.name)));
        }
        // A frame beyond the ring's textures replaces the oldest one waiting, which is not encoded.
        let _ = shared.feed.offer(frame, pts);
        self.poll_shared(out)
    }

    fn poll_shared(&mut self, out: &mut Vec<Packet>) -> Result<(), PluginError> {
        self.encode_ready(out)?;
        if let Some(shared) = &self.shared
            && shared.feed.stalled()
        {
            // The compositor reads its frames back through memory again.
            shared.ring.refuse(Fallback::NoEncoder);
            return Err(PluginError::Unavailable(format!("{} holds more pictures than the graphics ring has", self.name)));
        }
        Ok(())
    }

    fn flush(&mut self, out: &mut Vec<Packet>) -> Result<(), PluginError> {
        let mut failed = self.encode_ready(out).err();
        // The last frames' copies are usually still running on the compositor's queue when the
        // output stops: give them up to `FLUSH_WAIT` to finish so they are encoded. After a
        // failure no more frames are submitted.
        let until = std::time::Instant::now() + FLUSH_WAIT;
        while failed.is_none() && self.shared.as_ref().is_some_and(|s| s.feed.copy_running()) && std::time::Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(1));
            failed = self.encode_ready(out).err();
        }
        // The encoder is drained after a failure too: a packet it already read comes out here.
        let flushed = self.core.flush().map_err(|e| PluginError::Failed(format!("{} lost its last frames: {e}", self.name)));
        if let Some(e) = failed {
            if let Ok(ps) = flushed {
                packets(out, ps);
            }
            return Err(e);
        }
        packets(out, flushed?);
        // Frames whose copies did not finish in time are not encoded; the packets in `out` are
        // still written.
        let lost = self.shared.as_mut().map_or(0, |s| s.feed.drain());
        if lost > 0 {
            return Err(PluginError::Failed(format!("{} lost its last {lost} frames: their copies did not finish", self.name)));
        }
        Ok(())
    }
}

// `NvencSession::shared` is declared after `core`, so the textures go back only once the
// encoder has been torn down and no longer reads them.
impl Drop for Shared {
    fn drop(&mut self) {
        self.feed.drain();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use craft_hwenc_native::nvenc::{Input, Rate};
    use studiocraft_plugin::RateControl;

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
        assert_eq!(Nvenc::for_candidate("nvenc_av1").map(|n| (n.id(), n.codec())), Some(("nvenc_native_av1", Codec::Av1)));
        assert_eq!(Nvenc::AV1.name(), "NVIDIA NVENC AV1 (built in)");
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
    fn a_session_that_cannot_take_the_rings_frames_records_why_on_the_ring() {
        let p = params(RateControl::Quality(23));
        // A ring of another size.
        let ring = ExportRing::detached(128, 72);
        assert!(matches!(Nvenc::H264.session_on(&p, &ring), Err(PluginError::Unavailable(_))));
        assert_eq!(ring.fallback(), Some(Fallback::NoEncoder));
        // Settings the encoder cannot take.
        let ring = ExportRing::detached(256, 144);
        assert!(Nvenc::H264.session_on(&params(RateControl::Lossless), &ring).is_err());
        assert_eq!(ring.fallback(), Some(Fallback::NoEncoder));
        // A ring without a device: no session opens on it anywhere.
        let ring = ExportRing::detached(256, 144);
        assert!(matches!(Nvenc::HEVC.session_on(&p, &ring), Err(PluginError::Unavailable(_))));
        assert_eq!(ring.fallback(), Some(Fallback::NoEncoder));
    }

    #[test]
    fn a_session_on_frames_in_memory_refuses_frames_on_the_gpu() {
        let Ok(mut s) = Nvenc::H264.session(&params(RateControl::Quality(23))) else { return };
        let ring = ExportRing::detached(256, 144);
        let frame = ExportFrame::new(ring, 0, 1).unwrap();
        let mut out = Vec::new();
        assert!(matches!(s.encode_shared(frame, 0, &mut out), Err(PluginError::Unavailable(_))));
        assert!(s.poll_shared(&mut out).is_ok(), "nothing is kept to encode");
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
