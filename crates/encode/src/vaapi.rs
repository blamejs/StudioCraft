//! The VA-API hardware encoders that run in StudioCraft's own process on 64-bit Linux: H.264,
//! HEVC and AV1 from the compositor's NV12 frames, with the `avcC`, `hvcC` or `av1C` record a
//! container stores.
//!
//! - The rate control, the keyframe interval and the limits on them are the ones every built-in
//!   hardware encoder takes ([`crate::hardware`]).
//! - Every picture is an IDR or P picture (an AV1 key or inter frame), so each packet's decoding
//!   time is its presentation time.
//! - The encoder runs on the first render node whose driver encodes the codec.
//! - On Linux, [`VaApi::session_on`] opens a session on the render node of an export ring's
//!   Vulkan device. It imports the ring's DMA-BUFs as input surfaces and encodes the compositor's
//!   frames from them ([`VideoSession::encode_shared`]) without a copy through memory. When the
//!   encoder cannot open on that device, the session records [`Fallback::NoEncoder`] on the ring;
//!   when the driver cannot import a buffer, it records [`Fallback::ImportFailed`]. The compositor
//!   then sends its frames through memory again.

use craft_gpu_interop::{ExportFrame, ExportRing, Fallback, Feed};
use craft_hwenc_native::Codec as NativeCodec;
use craft_hwenc_native::vaapi::{Config, Encoder as Core, Error, PrimeFrame};
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
    /// VA-API AV1, registered as `vaapi_native_av1`.
    pub const AV1: VaApi = VaApi { codec: NativeCodec::Av1 };

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
        Ok(VaSession { core, name: self.name(), shared: None })
    }

    /// Open a session for `params` that takes its frames from `ring` on the GPU. The ring must
    /// have the size of `params` and DMA-BUFs of a device with a render node. When the encoder
    /// cannot open on that device (no VA-API encoder there, another GPU, a ring without buffers, or
    /// a system other than Linux), [`Fallback::NoEncoder`] is recorded on the ring and the error
    /// returned. The session takes only frames on the GPU: when the ring records a fallback, the
    /// frames the compositor reads back need a session from [`VaApi::session`].
    pub fn session_on(&self, params: &VideoParams, ring: &std::sync::Arc<ExportRing>) -> Result<VaSession, PluginError> {
        let opened = self.open_on(params, ring);
        if opened.is_err() {
            ring.refuse(Fallback::NoEncoder);
        }
        opened
    }

    fn open_on(&self, params: &VideoParams, ring: &std::sync::Arc<ExportRing>) -> Result<VaSession, PluginError> {
        if (ring.width(), ring.height()) != (params.width, params.height) {
            return Err(PluginError::Unavailable(format!(
                "{} takes frames of {}x{}; the graphics ring holds {}x{}",
                self.name(),
                params.width,
                params.height,
                ring.width(),
                ring.height()
            )));
        }
        #[cfg(target_os = "linux")]
        {
            let (Some(dev), Some(_)) = (ring.render_node(), ring.buffer(0)) else {
                return Err(PluginError::Unavailable(format!("{} cannot open the graphics ring's buffers", self.name())));
            };
            let cfg = hardware::config(self.codec, params, "VA-API (built in)")?;
            let core = Core::on_render_node(&cfg, dev).map_err(|e| self.error(e))?;
            Ok(VaSession { core, name: self.name(), shared: Some(Shared { ring: std::sync::Arc::clone(ring), feed: Feed::default() }) })
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(PluginError::Unavailable(format!("{} takes frames on the GPU on Linux only", self.name())))
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

/// How long a flush of a session on the GPU waits for the copies of its last frames.
const FLUSH_WAIT: std::time::Duration = std::time::Duration::from_millis(250);

/// A running VA-API encoder.
pub struct VaSession {
    core: Core,
    name: &'static str,
    /// The ring a session on the GPU takes its frames from, and the frames it holds. Declared
    /// after `core`: it is dropped after the encoder, whose imported surfaces read the buffers.
    shared: Option<Shared>,
}

/// The ring of a session on the GPU and the frames it holds.
struct Shared {
    ring: std::sync::Arc<ExportRing>,
    feed: Feed,
}

/// What [`encode_ready`] needs of an encoder: the input surfaces it imported from DMA-BUFs and
/// encoding one of them.
trait Surfaces {
    fn imported(&self, key: usize) -> bool;
    fn import(&mut self, key: usize, fd: i32, frame: &PrimeFrame) -> Result<(), Error>;
    fn encode_surface(&mut self, key: usize, pts: i64) -> Result<Vec<craft_hwenc_native::Packet>, Error>;
}

impl Surfaces for Core {
    fn imported(&self, key: usize) -> bool {
        Core::imported(self, key)
    }
    fn import(&mut self, key: usize, fd: i32, frame: &PrimeFrame) -> Result<(), Error> {
        Core::import(self, key, fd, frame)
    }
    fn encode_surface(&mut self, key: usize, pts: i64) -> Result<Vec<craft_hwenc_native::Packet>, Error> {
        Core::encode_surface(self, key, pts)
    }
}

/// The descriptor and layout of the buffer of `slot` in `ring`, for an import.
#[cfg(target_os = "linux")]
fn ring_buffer(ring: &ExportRing, slot: usize) -> Option<(i32, PrimeFrame)> {
    use std::os::fd::AsRawFd;
    let b = ring.buffer(slot)?;
    let (width, height) = b.image_size();
    Some((b.fd().as_raw_fd(), PrimeFrame { width, height, size: b.size(), modifier: b.modifier(), offsets: b.offsets(), pitches: b.pitches() }))
}

#[cfg(not(target_os = "linux"))]
fn ring_buffer(_ring: &ExportRing, _slot: usize) -> Option<(i32, PrimeFrame)> {
    None
}

/// Encode the frames of `shared` whose copies have finished, oldest first, on `core`, importing
/// each ring buffer the first time a frame uses it (`buffer` gives its descriptor and layout), and
/// hand back the buffers whose packets came out. It returns at the first frame whose copy is still
/// running, without waiting. A buffer the driver cannot import records [`Fallback::ImportFailed`]
/// on the ring; a frame outside the ring's buffers or an encoder that stops records
/// [`Fallback::NoEncoder`].
fn encode_ready<C: Surfaces>(
    core: &mut C,
    shared: &mut Shared,
    name: &str,
    buffer: impl Fn(&ExportRing, usize) -> Option<(i32, PrimeFrame)>,
    out: &mut Vec<Packet>,
) -> Result<(), PluginError> {
    while let Some((frame, pts)) = shared.feed.take_ready() {
        let slot = frame.slot();
        if !core.imported(slot) {
            let Some((fd, layout)) = buffer(&shared.ring, slot) else {
                shared.ring.refuse(Fallback::NoEncoder);
                return Err(PluginError::Failed(format!("{name} was given a frame outside its ring")));
            };
            if let Err(e) = core.import(slot, fd, &layout) {
                // The compositor reads its frames back through memory again.
                shared.ring.refuse(Fallback::ImportFailed);
                return Err(PluginError::Unavailable(format!("{name} cannot read the graphics ring's buffers: {e}")));
            }
        }
        let ps = match core.encode_surface(slot, pts) {
            Ok(ps) => ps,
            Err(e) => {
                shared.ring.refuse(Fallback::NoEncoder);
                return Err(PluginError::Failed(format!("{name} stopped: {e}")));
            }
        };
        // The encoder gives one packet per picture, in the order it took them.
        shared.feed.out(ps.len());
        packets(out, ps);
    }
    Ok(())
}

impl VaSession {
    /// The settings the session runs with.
    pub fn config(&self) -> &Config {
        self.core.config()
    }

    fn encode_ready(&mut self, out: &mut Vec<Packet>) -> Result<(), PluginError> {
        let Some(shared) = &mut self.shared else { return Ok(()) };
        encode_ready(&mut self.core, shared, self.name, ring_buffer, out)
    }
}

impl VideoSession for VaSession {
    fn codec_private(&self) -> Vec<u8> {
        self.core.codec_private().to_vec()
    }

    fn encode(&mut self, nv12: &[u8], pts: i64, out: &mut Vec<Packet>) -> Result<(), PluginError> {
        if self.shared.is_some() {
            // Once its ring records a fallback, the output opens a session with
            // `VaApi::session` for the frames the compositor reads back.
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
        // A frame beyond the ring's buffers replaces the oldest one waiting, which is not encoded.
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
        self.encode_ready(out)?;
        // The last frames' copies are usually still running on the compositor's queue when the
        // output stops: give them up to `FLUSH_WAIT` to finish so they are encoded.
        let until = std::time::Instant::now() + FLUSH_WAIT;
        while self.shared.as_ref().is_some_and(|s| s.feed.copy_running()) && std::time::Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(1));
            self.encode_ready(out)?;
        }
        let ps = self.core.flush().map_err(|e| PluginError::Failed(format!("{} lost its last frames: {e}", self.name)))?;
        packets(out, ps);
        // The packets of every frame read are out, so their buffers go back.
        let lost = self.shared.as_mut().map_or(0, |s| s.feed.drain());
        if lost > 0 {
            return Err(PluginError::Failed(format!("{} lost its last {lost} frames: their copies did not finish", self.name)));
        }
        Ok(())
    }
}

// `VaSession::shared` is declared after `core`, so the buffers go back only once the encoder has
// been torn down and no longer reads them.
impl Drop for Shared {
    fn drop(&mut self) {
        self.feed.drain();
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
        let expected = if craft_hwenc_native::vaapi::available(NativeCodec::Av1).is_ok() { Availability::Ready } else { Availability::NotRunHere };
        assert_eq!(VaApi::AV1.availability(&recording), expected);
        assert_eq!((VaApi::AV1.id(), VaApi::AV1.name(), VaApi::AV1.codec()), ("vaapi_native_av1", "VA-API AV1 (built in)", Codec::Av1));
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

    #[test]
    fn a_session_that_cannot_take_the_rings_frames_records_why_on_the_ring() {
        let p = params(RateControl::Bitrate { kbps: 2500, keyframe_s: 2 });
        // A ring of another size.
        let ring = ExportRing::detached(128, 72);
        assert!(matches!(VaApi::H264.session_on(&p, &ring), Err(PluginError::Unavailable(_))));
        assert_eq!(ring.fallback(), Some(Fallback::NoEncoder));
        // A ring without buffers or a render node: no encoder opens on its device.
        let ring = ExportRing::detached(256, 144);
        assert!(matches!(VaApi::HEVC.session_on(&p, &ring), Err(PluginError::Unavailable(_))));
        assert_eq!(ring.fallback(), Some(Fallback::NoEncoder));
    }

    /// An encoder that holds two pictures and gives a packet for each older one, as the VA-API
    /// encoder does with buffers on the GPU.
    #[derive(Default)]
    struct FakeCore {
        imported: Vec<usize>,
        in_flight: std::collections::VecDeque<i64>,
        refuse_import: bool,
        fail_encode: bool,
    }

    impl Surfaces for FakeCore {
        fn imported(&self, key: usize) -> bool {
            self.imported.contains(&key)
        }
        fn import(&mut self, key: usize, _fd: i32, _frame: &PrimeFrame) -> Result<(), Error> {
            if self.refuse_import {
                return Err(Error::Failed("vaCreateSurfaces (DMA-BUF): invalid parameter".into()));
            }
            self.imported.push(key);
            Ok(())
        }
        fn encode_surface(&mut self, _key: usize, pts: i64) -> Result<Vec<craft_hwenc_native::Packet>, Error> {
            if self.fail_encode {
                return Err(Error::Failed("vaEndPicture failed".into()));
            }
            let mut out = Vec::new();
            while self.in_flight.len() >= craft_hwenc_native::vaapi::SURFACE_DEPTH {
                let pts = self.in_flight.pop_front().unwrap_or(0);
                out.push(craft_hwenc_native::Packet { data: vec![1], pts, keyframe: false });
            }
            self.in_flight.push_back(pts);
            Ok(out)
        }
    }

    fn layout(_ring: &ExportRing, slot: usize) -> Option<(i32, PrimeFrame)> {
        (slot < 3)
            .then_some((10 + slot as i32, PrimeFrame { width: 256, height: 144, size: 55296, modifier: 0, offsets: [0, 36864], pitches: [256, 256] }))
    }

    fn shared(ring: &std::sync::Arc<ExportRing>) -> Shared {
        Shared { ring: std::sync::Arc::clone(ring), feed: Feed::default() }
    }

    #[test]
    fn a_frame_whose_copy_is_late_waits_without_stopping_the_run_and_buffers_go_back_as_packets_come_out() {
        let ring = ExportRing::detached(256, 144);
        let (mut core, mut s, mut out) = (FakeCore::default(), shared(&ring), Vec::new());
        s.feed.offer(ExportFrame::new(std::sync::Arc::clone(&ring), 0, 1).unwrap(), 0);
        // The copy has not finished: the call returns at once with nothing encoded or imported.
        encode_ready(&mut core, &mut s, "test", layout, &mut out).unwrap();
        assert!(out.is_empty() && core.imported.is_empty() && core.in_flight.is_empty());
        assert_eq!(s.feed.waiting(), 1);
        ring.set_ready(1);
        encode_ready(&mut core, &mut s, "test", layout, &mut out).unwrap();
        assert_eq!((core.imported.clone(), s.feed.reading()), (vec![0], 1));
        for (slot, ready) in [(1, 2), (2, 3), (0, 4)] {
            ring.set_ready(ready);
            s.feed.offer(ExportFrame::new(std::sync::Arc::clone(&ring), slot, ready).unwrap(), i64::try_from(ready).unwrap() - 1);
            encode_ready(&mut core, &mut s, "test", layout, &mut out).unwrap();
        }
        // Each buffer is imported once; the encoder holds two pictures, so the first two packets
        // are out and their buffers went back.
        assert_eq!(core.imported, vec![0, 1, 2]);
        assert_eq!(out.iter().map(|p| p.pts).collect::<Vec<_>>(), vec![0, 1]);
        assert_eq!(ring.released(), 2);
        assert!(!s.feed.stalled(), "the compositor still has a buffer to write");
        assert_eq!(ring.fallback(), None);
    }

    #[test]
    fn a_buffer_the_driver_cannot_import_sends_the_frames_back_through_memory() {
        let ring = ExportRing::detached(256, 144);
        let (mut core, mut s, mut out) = (FakeCore { refuse_import: true, ..FakeCore::default() }, shared(&ring), Vec::new());
        ring.set_ready(1);
        s.feed.offer(ExportFrame::new(std::sync::Arc::clone(&ring), 0, 1).unwrap(), 0);
        let e = encode_ready(&mut core, &mut s, "test", layout, &mut out).unwrap_err();
        assert!(matches!(e, PluginError::Unavailable(ref why) if why.contains("invalid parameter")), "{e}");
        assert_eq!(ring.fallback(), Some(Fallback::ImportFailed));
    }

    #[test]
    fn a_frame_outside_the_rings_buffers_or_an_encoder_that_stops_sends_the_frames_back_through_memory() {
        let ring = ExportRing::detached(256, 144);
        let (mut core, mut s, mut out) = (FakeCore::default(), shared(&ring), Vec::new());
        ring.set_ready(1);
        s.feed.offer(ExportFrame::new(std::sync::Arc::clone(&ring), 0, 1).unwrap(), 0);
        assert!(encode_ready(&mut core, &mut s, "test", |_, _| None, &mut out).is_err());
        assert_eq!(ring.fallback(), Some(Fallback::NoEncoder));
        let ring = ExportRing::detached(256, 144);
        let (mut core, mut s) = (FakeCore { fail_encode: true, ..FakeCore::default() }, shared(&ring));
        ring.set_ready(1);
        s.feed.offer(ExportFrame::new(std::sync::Arc::clone(&ring), 0, 1).unwrap(), 0);
        assert!(matches!(encode_ready(&mut core, &mut s, "test", layout, &mut out), Err(PluginError::Failed(_))));
        assert_eq!(ring.fallback(), Some(Fallback::NoEncoder));
    }
}
