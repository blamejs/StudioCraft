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
            Ok(VaSession { core, name: self.name(), shared: Some(Shared::new(ring)) })
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
    /// The encoder stopped before its pictures were coded and may still read their buffers: none
    /// of them is handed back.
    held: bool,
}

impl Shared {
    fn new(ring: &std::sync::Arc<ExportRing>) -> Shared {
        Shared { ring: std::sync::Arc::clone(ring), feed: Feed::default(), held: false }
    }
}

/// What [`encode_ready`] needs of an encoder: the input surfaces it imported from DMA-BUFs and
/// encoding one of them.
trait Surfaces {
    fn imported(&self, key: usize) -> bool;
    fn import(&mut self, key: usize, fd: i32, frame: &PrimeFrame) -> Result<(), Error>;
    /// Whether [`Surfaces::encode_surface`] takes a picture without waiting for the GPU.
    fn surface_ready(&mut self) -> bool;
    fn encode_surface(&mut self, key: usize, pts: i64) -> Result<Vec<craft_hwenc_native::Packet>, Error>;
    fn flush(&mut self) -> Result<Vec<craft_hwenc_native::Packet>, Error>;
    /// The packets that came out during a call that then failed.
    fn take_kept(&mut self) -> Vec<craft_hwenc_native::Packet>;
    /// Wait for the pictures in flight; `false` when one did not finish.
    fn wait_idle(&mut self) -> bool;
}

impl Surfaces for Core {
    fn imported(&self, key: usize) -> bool {
        Core::imported(self, key)
    }
    fn import(&mut self, key: usize, fd: i32, frame: &PrimeFrame) -> Result<(), Error> {
        Core::import(self, key, fd, frame)
    }
    fn surface_ready(&mut self) -> bool {
        Core::surface_ready(self)
    }
    fn encode_surface(&mut self, key: usize, pts: i64) -> Result<Vec<craft_hwenc_native::Packet>, Error> {
        Core::encode_surface(self, key, pts)
    }
    fn flush(&mut self) -> Result<Vec<craft_hwenc_native::Packet>, Error> {
        Core::flush(self)
    }
    fn take_kept(&mut self) -> Vec<craft_hwenc_native::Packet> {
        Core::take_kept(self)
    }
    fn wait_idle(&mut self) -> bool {
        Core::wait_idle(self)
    }
}

/// Add the packets `core` kept from a call that failed to `out`, and hand back the buffers of
/// their pictures, for a session that stops after the error.
fn rescue<C: Surfaces>(core: &mut C, feed: Option<&mut Feed>, out: &mut Vec<Packet>) {
    let kept = core.take_kept();
    if let Some(feed) = feed {
        feed.out(kept.len());
    }
    packets(out, kept);
}

/// For a session on the GPU that stops: wait for the encoder's pictures. When one does not
/// finish, the encoder may still read the ring's buffers, so none is handed back and the ring
/// records [`Fallback::NoEncoder`].
fn stop<C: Surfaces>(core: &mut C, shared: &mut Shared) {
    if !core.wait_idle() {
        shared.ring.refuse(Fallback::NoEncoder);
        shared.held = true;
    }
}

/// The descriptor and layout of the buffer of `slot` in `ring`, for an import.
#[cfg(target_os = "linux")]
fn ring_buffer(ring: &ExportRing, slot: usize) -> Option<(i32, PrimeFrame)> {
    use std::os::fd::AsRawFd;
    let b = ring.buffer(slot)?;
    let (width, height) = b.image_size();
    Some((
        b.fd().as_raw_fd(),
        PrimeFrame {
            width,
            height,
            size: b.size(),
            modifier: b.modifier(),
            offsets: b.offsets(),
            pitches: b.pitches(),
            plane_sizes: b.plane_sizes(),
        },
    ))
}

#[cfg(not(target_os = "linux"))]
fn ring_buffer(_ring: &ExportRing, _slot: usize) -> Option<(i32, PrimeFrame)> {
    None
}

/// Encode the frames of `shared` whose copies have finished, oldest first, on `core`, importing
/// each ring buffer the first time a frame uses it (`buffer` gives its descriptor and layout), and
/// hand back the buffers whose packets came out. It returns at the first frame whose copy is still
/// running, without waiting, and unless `wait` is set, also while the encoder could take the next
/// picture only after waiting for an earlier one to be coded: that frame stays queued. A buffer the
/// driver cannot import records [`Fallback::ImportFailed`] on the ring, and the packets of the
/// pictures the encoder already took are added to `out` before the error returns, also when
/// reading them fails part way; a frame outside the ring's buffers or an encoder that stops
/// records [`Fallback::NoEncoder`], and the packets that came out before the error are added to
/// `out`.
fn encode_ready<C: Surfaces>(
    core: &mut C,
    shared: &mut Shared,
    name: &str,
    buffer: impl Fn(&ExportRing, usize) -> Option<(i32, PrimeFrame)>,
    out: &mut Vec<Packet>,
    wait: bool,
) -> Result<(), PluginError> {
    while wait || core.surface_ready() {
        let Some((frame, pts)) = shared.feed.take_ready() else { break };
        let slot = frame.slot();
        if !core.imported(slot) {
            let Some((fd, layout)) = buffer(&shared.ring, slot) else {
                shared.ring.refuse(Fallback::NoEncoder);
                return Err(PluginError::Failed(format!("{name} was given a frame outside its ring")));
            };
            if let Err(e) = core.import(slot, fd, &layout) {
                // The compositor reads its frames back through memory again.
                shared.ring.refuse(Fallback::ImportFailed);
                // The pictures the encoder already took still come out, ahead of the frames read
                // back through memory.
                let taken = match core.flush() {
                    Ok(taken) => taken,
                    Err(lost) => {
                        rescue(core, Some(&mut shared.feed), out);
                        return Err(PluginError::Failed(format!("{name} lost its last frames: {lost}")));
                    }
                };
                shared.feed.out(taken.len());
                packets(out, taken);
                return Err(PluginError::Unavailable(format!("{name} cannot read the graphics ring's buffers: {e}")));
            }
        }
        let ps = match core.encode_surface(slot, pts) {
            Ok(ps) => ps,
            Err(e) => {
                shared.ring.refuse(Fallback::NoEncoder);
                rescue(core, Some(&mut shared.feed), out);
                return Err(PluginError::Failed(format!("{name} stopped: {e}")));
            }
        };
        // The encoder gives one packet per picture, in the order it took them.
        shared.feed.out(ps.len());
        packets(out, ps);
    }
    Ok(())
}

/// For a stream that ends: [`encode_ready`] with the encoder allowed to wait, until no frame of
/// `shared` waits or `wait` has passed on `now` since the first try returned. A copy that finished
/// after the last try is encoded on the next try at once; `pause` runs only while the oldest
/// frame's copy is still running.
#[allow(clippy::too_many_arguments)]
fn encode_waiting<C: Surfaces>(
    core: &mut C,
    shared: &mut Shared,
    name: &str,
    buffer: impl Fn(&ExportRing, usize) -> Option<(i32, PrimeFrame)>,
    out: &mut Vec<Packet>,
    wait: std::time::Duration,
    mut now: impl FnMut() -> std::time::Instant,
    mut pause: impl FnMut(),
) -> Result<(), PluginError> {
    let mut until = None;
    loop {
        encode_ready(core, shared, name, &buffer, out, true)?;
        if shared.feed.waiting() == 0 {
            return Ok(());
        }
        // The first try may wait for earlier pictures; the copies get `wait` after it.
        let t = now();
        if t >= *until.get_or_insert(t + wait) {
            return Ok(());
        }
        if shared.feed.copy_running() {
            pause();
        }
    }
}

impl VaSession {
    /// The settings the session runs with.
    pub fn config(&self) -> &Config {
        self.core.config()
    }

    fn encode_ready(&mut self, out: &mut Vec<Packet>, wait: bool) -> Result<(), PluginError> {
        let Some(shared) = &mut self.shared else { return Ok(()) };
        encode_ready(&mut self.core, shared, self.name, ring_buffer, out, wait)
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
        let ps = match self.core.encode(nv12, pts) {
            Ok(ps) => ps,
            Err(e) => {
                rescue(&mut self.core, None, out);
                return Err(PluginError::Failed(format!("{} stopped: {e}", self.name)));
            }
        };
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
        self.encode_ready(out, false)?;
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
        // The last frames' copies are usually still running on the compositor's queue when the
        // output stops: give them up to `FLUSH_WAIT` to finish so they are encoded.
        if let Some(shared) = &mut self.shared {
            let pause = || std::thread::sleep(std::time::Duration::from_millis(1));
            encode_waiting(&mut self.core, shared, self.name, ring_buffer, out, FLUSH_WAIT, std::time::Instant::now, pause)?;
        }
        let ps = match self.core.flush() {
            Ok(ps) => ps,
            Err(e) => {
                rescue(&mut self.core, None, out);
                return Err(PluginError::Failed(format!("{} lost its last frames: {e}", self.name)));
            }
        };
        packets(out, ps);
        // The packets of every frame read are out, so their buffers go back.
        let lost = self.shared.as_mut().map_or(0, |s| s.feed.drain());
        if lost > 0 {
            return Err(PluginError::Failed(format!("{} lost its last {lost} frames: their copies did not finish", self.name)));
        }
        Ok(())
    }
}

impl Drop for VaSession {
    fn drop(&mut self) {
        if let Some(shared) = &mut self.shared {
            stop(&mut self.core, shared);
        }
    }
}

// `VaSession::shared` is declared after `core`, so the buffers go back only once the encoder has
// been torn down. An encoder that stopped before its pictures were coded is left open and may
// still read them, so then none goes back.
impl Drop for Shared {
    fn drop(&mut self) {
        if !self.held {
            self.feed.drain();
        }
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
        /// A slot whose buffer the driver refuses; the others import.
        refuse_slot: Option<usize>,
        fail_encode: bool,
        /// The oldest picture is still being coded: the next one waits for it.
        coding: bool,
        /// A flush reads this many pictures and then fails on the next one.
        flush_fails_after: Option<usize>,
        /// A picture in flight never finishes.
        hung: bool,
        kept: Vec<craft_hwenc_native::Packet>,
    }

    impl Surfaces for FakeCore {
        fn imported(&self, key: usize) -> bool {
            self.imported.contains(&key)
        }
        fn import(&mut self, key: usize, _fd: i32, _frame: &PrimeFrame) -> Result<(), Error> {
            if self.refuse_import || self.refuse_slot == Some(key) {
                return Err(Error::Failed("vaCreateSurfaces (DMA-BUF): invalid parameter".into()));
            }
            self.imported.push(key);
            Ok(())
        }
        fn surface_ready(&mut self) -> bool {
            !self.coding || self.in_flight.len() < craft_hwenc_native::vaapi::SURFACE_DEPTH
        }
        fn take_kept(&mut self) -> Vec<craft_hwenc_native::Packet> {
            std::mem::take(&mut self.kept)
        }
        fn wait_idle(&mut self) -> bool {
            self.in_flight.clear();
            !self.hung
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
        fn flush(&mut self) -> Result<Vec<craft_hwenc_native::Packet>, Error> {
            let packet = |pts| craft_hwenc_native::Packet { data: vec![1], pts, keyframe: false };
            if let Some(read) = self.flush_fails_after {
                // As the VA-API encoder does: the packets read before the failure are kept for
                // the next call, and the picture that failed is gone.
                let mut kept: Vec<_> = self.in_flight.drain(..read.min(self.in_flight.len())).map(packet).collect();
                self.in_flight.pop_front();
                self.kept.append(&mut kept);
                return Err(Error::Failed("the coded buffer reported an error".into()));
            }
            let mut out = std::mem::take(&mut self.kept);
            out.extend(self.in_flight.drain(..).map(packet));
            Ok(out)
        }
    }

    fn layout(_ring: &ExportRing, slot: usize) -> Option<(i32, PrimeFrame)> {
        (slot < 3).then_some((
            10 + slot as i32,
            PrimeFrame { width: 256, height: 144, size: 55296, modifier: 0, offsets: [0, 36864], pitches: [256, 256], plane_sizes: [36864, 18432] },
        ))
    }

    fn shared(ring: &std::sync::Arc<ExportRing>) -> Shared {
        Shared::new(ring)
    }

    #[test]
    fn a_frame_whose_copy_is_late_waits_without_stopping_the_run_and_buffers_go_back_as_packets_come_out() {
        let ring = ExportRing::detached(256, 144);
        let (mut core, mut s, mut out) = (FakeCore::default(), shared(&ring), Vec::new());
        s.feed.offer(ExportFrame::new(std::sync::Arc::clone(&ring), 0, 1).unwrap(), 0);
        // The copy has not finished: the call returns at once with nothing encoded or imported.
        encode_ready(&mut core, &mut s, "test", layout, &mut out, false).unwrap();
        assert!(out.is_empty() && core.imported.is_empty() && core.in_flight.is_empty());
        assert_eq!(s.feed.waiting(), 1);
        ring.set_ready(1);
        encode_ready(&mut core, &mut s, "test", layout, &mut out, false).unwrap();
        assert_eq!((core.imported.clone(), s.feed.reading()), (vec![0], 1));
        for (slot, ready) in [(1, 2), (2, 3), (0, 4)] {
            ring.set_ready(ready);
            s.feed.offer(ExportFrame::new(std::sync::Arc::clone(&ring), slot, ready).unwrap(), i64::try_from(ready).unwrap() - 1);
            encode_ready(&mut core, &mut s, "test", layout, &mut out, false).unwrap();
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
        let e = encode_ready(&mut core, &mut s, "test", layout, &mut out, false).unwrap_err();
        assert!(matches!(e, PluginError::Unavailable(ref why) if why.contains("invalid parameter")), "{e}");
        assert_eq!(ring.fallback(), Some(Fallback::ImportFailed));
    }

    /// When the buffer of a later frame cannot be imported, the packets of the pictures the encoder
    /// already took come out before the error, and their buffers go back.
    #[test]
    fn a_refused_import_still_gives_the_packets_of_the_pictures_already_taken() {
        let ring = ExportRing::detached(256, 144);
        let (mut core, mut s, mut out) = (FakeCore { refuse_slot: Some(1), ..FakeCore::default() }, shared(&ring), Vec::new());
        ring.set_ready(1);
        s.feed.offer(ExportFrame::new(std::sync::Arc::clone(&ring), 0, 1).unwrap(), 0);
        encode_ready(&mut core, &mut s, "test", layout, &mut out, false).unwrap();
        assert!(out.is_empty(), "the encoder holds the first picture");
        ring.set_ready(2);
        s.feed.offer(ExportFrame::new(std::sync::Arc::clone(&ring), 1, 2).unwrap(), 1);
        let e = encode_ready(&mut core, &mut s, "test", layout, &mut out, false).unwrap_err();
        assert!(matches!(e, PluginError::Unavailable(_)), "{e}");
        assert_eq!(out.iter().map(|p| p.pts).collect::<Vec<_>>(), vec![0], "the first picture's packet");
        assert_eq!(ring.released(), 1, "its buffer went back");
        assert_eq!(ring.fallback(), Some(Fallback::ImportFailed));
    }

    #[test]
    fn a_frame_outside_the_rings_buffers_or_an_encoder_that_stops_sends_the_frames_back_through_memory() {
        let ring = ExportRing::detached(256, 144);
        let (mut core, mut s, mut out) = (FakeCore::default(), shared(&ring), Vec::new());
        ring.set_ready(1);
        s.feed.offer(ExportFrame::new(std::sync::Arc::clone(&ring), 0, 1).unwrap(), 0);
        assert!(encode_ready(&mut core, &mut s, "test", |_, _| None, &mut out, false).is_err());
        assert_eq!(ring.fallback(), Some(Fallback::NoEncoder));
        let ring = ExportRing::detached(256, 144);
        let (mut core, mut s) = (FakeCore { fail_encode: true, ..FakeCore::default() }, shared(&ring));
        ring.set_ready(1);
        s.feed.offer(ExportFrame::new(std::sync::Arc::clone(&ring), 0, 1).unwrap(), 0);
        assert!(matches!(encode_ready(&mut core, &mut s, "test", layout, &mut out, false), Err(PluginError::Failed(_))));
        assert_eq!(ring.fallback(), Some(Fallback::NoEncoder));
    }

    /// When a refused import makes the encoder give out its pictures and reading the second one
    /// fails, the first one's packet still comes out with the error, and its buffer goes back.
    #[test]
    fn a_refused_import_whose_flush_fails_part_way_still_gives_the_packets_read() {
        let ring = ExportRing::detached(256, 144);
        let core = FakeCore { refuse_slot: Some(2), flush_fails_after: Some(1), ..FakeCore::default() };
        let (mut core, mut s, mut out) = (core, shared(&ring), Vec::new());
        for (slot, ready) in [(0, 1), (1, 2)] {
            ring.set_ready(ready);
            s.feed.offer(ExportFrame::new(std::sync::Arc::clone(&ring), slot, ready).unwrap(), i64::try_from(ready).unwrap() - 1);
            encode_ready(&mut core, &mut s, "test", layout, &mut out, false).unwrap();
        }
        assert!(out.is_empty(), "the encoder holds both pictures");
        ring.set_ready(3);
        s.feed.offer(ExportFrame::new(std::sync::Arc::clone(&ring), 2, 3).unwrap(), 2);
        let e = encode_ready(&mut core, &mut s, "test", layout, &mut out, false).unwrap_err();
        assert!(matches!(e, PluginError::Failed(ref why) if why.contains("lost its last frames")), "{e}");
        assert_eq!(out.iter().map(|p| p.pts).collect::<Vec<_>>(), vec![0], "the first picture's packet");
        assert_eq!(ring.released(), 1, "its buffer went back");
        assert_eq!(ring.fallback(), Some(Fallback::ImportFailed));
    }

    /// While the encoder still codes the oldest of the two pictures it holds, a ready frame stays
    /// queued instead of waiting for it; it goes in once that picture is coded, and a flush may wait.
    #[test]
    fn a_ready_frame_waits_in_the_queue_while_the_encoder_is_busy() {
        let ring = ExportRing::detached(256, 144);
        let (mut core, mut s, mut out) = (FakeCore { coding: true, ..FakeCore::default() }, shared(&ring), Vec::new());
        for (slot, ready) in [(0, 1), (1, 2), (2, 3)] {
            ring.set_ready(ready);
            s.feed.offer(ExportFrame::new(std::sync::Arc::clone(&ring), slot, ready).unwrap(), i64::try_from(ready).unwrap() - 1);
            encode_ready(&mut core, &mut s, "test", layout, &mut out, false).unwrap();
        }
        assert_eq!((core.in_flight.len(), s.feed.waiting(), out.len()), (2, 1, 0), "the third frame waits");
        core.coding = false;
        encode_ready(&mut core, &mut s, "test", layout, &mut out, false).unwrap();
        assert_eq!((s.feed.waiting(), out.iter().map(|p| p.pts).collect::<Vec<_>>()), (0, vec![0]));
        core.coding = true;
        ring.set_ready(4);
        s.feed.offer(ExportFrame::new(std::sync::Arc::clone(&ring), 0, 4).unwrap(), 3);
        encode_ready(&mut core, &mut s, "test", layout, &mut out, true).unwrap();
        assert_eq!(s.feed.waiting(), 0, "a flush takes the frame though the encoder is busy");
    }

    /// At the end of a stream, a frame whose copy finishes just after a try is encoded on the next
    /// try without a pause, also when the first try took longer than the wait; one whose copy is
    /// still running at the deadline stays waiting.
    #[test]
    fn a_flush_encodes_a_frame_whose_copy_finishes_after_a_try() {
        let start = std::time::Instant::now();
        let wait = std::time::Duration::from_millis(100);
        let ring = ExportRing::detached(256, 144);
        let (mut core, mut s, mut out) = (FakeCore::default(), shared(&ring), Vec::new());
        ring.set_ready(1);
        s.feed.offer(ExportFrame::new(std::sync::Arc::clone(&ring), 0, 1).unwrap(), 0);
        s.feed.offer(ExportFrame::new(std::sync::Arc::clone(&ring), 1, 2).unwrap(), 1);
        // The first try returns a second later, and the second frame's copy has just finished.
        let now = || {
            ring.set_ready(2);
            start + std::time::Duration::from_secs(1)
        };
        let mut pauses = 0;
        encode_waiting(&mut core, &mut s, "test", layout, &mut out, wait, now, || pauses += 1).unwrap();
        assert_eq!((s.feed.waiting(), s.feed.reading(), pauses), (0, 2, 0), "both frames are taken without a pause");

        let ring = ExportRing::detached(256, 144);
        let (mut core, mut s) = (FakeCore::default(), shared(&ring));
        s.feed.offer(ExportFrame::new(std::sync::Arc::clone(&ring), 0, 1).unwrap(), 0);
        let mut clock = start;
        let now = || {
            clock += std::time::Duration::from_millis(60);
            clock
        };
        let mut pauses = 0;
        encode_waiting(&mut core, &mut s, "test", layout, &mut out, wait, now, || pauses += 1).unwrap();
        // The wait starts at 60 ms: tries at 60 and 120 ms pause, and the one at 180 ms stops.
        assert_eq!((s.feed.waiting(), pauses), (1, 2), "a copy still running at the deadline stays waiting");
    }

    /// An encoder that stops while a picture never finishes may still read the ring's buffers: none
    /// goes back, and the ring records why its frames return to memory. One whose pictures finish
    /// hands the buffers back.
    #[test]
    fn a_stopped_encoder_whose_picture_never_finishes_hands_back_no_buffer() {
        for (hung, released, fallback) in [(true, 1, Some(Fallback::NoEncoder)), (false, 3, None)] {
            let ring = ExportRing::detached(256, 144);
            let (mut core, mut s, mut out) = (FakeCore { hung, ..FakeCore::default() }, shared(&ring), Vec::new());
            for (slot, ready) in [(0, 1), (1, 2), (2, 3)] {
                ring.set_ready(ready);
                s.feed.offer(ExportFrame::new(std::sync::Arc::clone(&ring), slot, ready).unwrap(), i64::try_from(ready).unwrap() - 1);
                encode_ready(&mut core, &mut s, "test", layout, &mut out, false).unwrap();
            }
            assert_eq!(ring.released(), 1, "the first packet is out");
            stop(&mut core, &mut s);
            drop(s);
            assert_eq!((ring.released(), ring.fallback()), (released, fallback), "hung: {hung}");
        }
    }
}
