//! NVIDIA's NVENC encoder through the driver's encode API (API 12.1): H.264 High and HEVC Main from
//! NV12 frames, HEVC Main 10 from P010 frames. It runs on Windows (a Direct3D 11 device of the
//! NVIDIA adapter, `nvEncodeAPI64.dll`) and 64-bit Linux (the CUDA driver's primary context,
//! `libnvidia-encode.so.1`). Building needs no NVIDIA SDK: the driver is loaded at run time.
//!
//! - Frames go in through input buffers in system memory and come out as Annex B, which
//!   [`Encoder`] turns into 4-byte length-prefixed samples with the parameter sets in the `avcC`
//!   or `hvcC` record ([`Encoder::codec_private`]).
//! - The encoder uses preset P5 with high-quality tuning and no B-frames, so packets come out in
//!   the order the frames went in and each packet's decoding time is its presentation time.
//! - The stream's VUI says BT.709 at limited range.

use std::collections::VecDeque;

use crate::nvenc_ffi::{self, Locked, Session, Submitted};
use crate::{Codec, annexb, avc, hevc};

/// The layout of the frames an encoder takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Input {
    /// 8-bit 4:2:0: the luma rows, then the interleaved Cb and Cr rows at half height.
    Nv12,
    /// 10-bit 4:2:0 in 16-bit little-endian samples with the value in the high 10 bits, the
    /// planes laid out as NV12. HEVC only (Main 10).
    P010,
}

impl Input {
    fn bytes_per_sample(self) -> usize {
        match self {
            Input::Nv12 => 1,
            Input::P010 => 2,
        }
    }

    /// The bytes of one `width` × `height` frame.
    pub fn frame_bytes(self, width: u32, height: u32) -> usize {
        (width as usize).saturating_mul(height as usize).saturating_mul(3) / 2 * self.bytes_per_sample()
    }
}

/// How the encoder spends bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rate {
    /// Constant quality at this level, 1 to 51, lower is better.
    Quality(u32),
    /// Constant bitrate in kbit/s, with a one-second buffer.
    Cbr(u32),
}

/// What to encode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    pub codec: Codec,
    pub input: Input,
    pub width: u32,
    pub height: u32,
    /// Frames per second as numerator and denominator.
    pub fps: (u32, u32),
    pub rate: Rate,
    /// Frames from one keyframe to the next.
    pub keyint: u32,
}

/// One encoded frame: 4-byte length-prefixed NAL units without parameter sets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packet {
    pub data: Vec<u8>,
    /// The timestamp the frame went in with; with no B-frames it is also the decoding time.
    pub pts: i64,
    pub keyframe: bool,
}

/// Why an encoder could not open or run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// No NVIDIA GPU, driver or encode library on this system.
    NoDevice(String),
    /// Every encoding session of the GPU is taken by other programs; a later open can work.
    Busy(String),
    /// The GPU's encoder does not take this configuration.
    Unsupported(String),
    /// The driver failed.
    Failed(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NoDevice(e) | Error::Busy(e) | Error::Unsupported(e) | Error::Failed(e) => f.write_str(e),
        }
    }
}

impl std::error::Error for Error {}

impl From<nvenc_ffi::OpenError> for Error {
    fn from(e: nvenc_ffi::OpenError) -> Error {
        match e {
            nvenc_ffi::OpenError::NoDevice(why) => Error::NoDevice(why),
            nvenc_ffi::OpenError::Busy(why) => Error::Busy(why),
        }
    }
}

/// Pictures in flight: input and output buffer pairs.
const RING: usize = 4;

/// Whether this system has the NVIDIA driver's encode library in a version this crate speaks. The
/// library is loaded once per process, so later calls cost nothing.
pub fn driver() -> Result<(), Error> {
    nvenc_ffi::driver().map_err(Error::NoDevice)
}

/// Whether this system has an NVIDIA GPU whose encoder writes `codec`, with a driver whose encode
/// API this crate speaks. It opens a session, reads the codec's capabilities and closes it again.
pub fn available(codec: Codec) -> Result<(), Error> {
    driver()?;
    let session = Session::open()?;
    session.caps(ffi_codec(codec)).map(drop).map_err(Error::Unsupported)
}

fn ffi_codec(codec: Codec) -> nvenc_ffi::Codec {
    match codec {
        Codec::H264 => nvenc_ffi::Codec::H264,
        Codec::Hevc => nvenc_ffi::Codec::Hevc,
    }
}

/// Check `cfg` without the driver: sizes, rate, codec and input.
fn check(cfg: &Config) -> Result<(), Error> {
    let bad = |e: String| Err(Error::Unsupported(e));
    let (w, h) = (cfg.width, cfg.height);
    if w == 0 || h == 0 || w % 2 != 0 || h % 2 != 0 {
        return bad(format!("{w}x{h}: NVENC needs even, nonzero dimensions"));
    }
    if cfg.fps.0 == 0 || cfg.fps.1 == 0 {
        return bad(format!("the frame rate {}/{} is not a rate", cfg.fps.0, cfg.fps.1));
    }
    if cfg.input == Input::P010 && cfg.codec != Codec::Hevc {
        return bad("P010 frames are encoded as HEVC Main 10 only".into());
    }
    match cfg.rate {
        Rate::Quality(q) if !(1..=51).contains(&q) => bad(format!("the quality level {q} is outside 1 to 51")),
        Rate::Cbr(0) => bad("a bitrate of 0 kbit/s".into()),
        _ => Ok(()),
    }
}

/// An NVENC encoder.
pub struct Encoder {
    session: Session,
    cfg: Config,
    record: Vec<u8>,
    free: Vec<usize>,
    /// Slots submitted and not read yet, in submission order, and how many of them are ready.
    pending: VecDeque<usize>,
    ready: usize,
}

impl Encoder {
    /// Open an encoder for `cfg`, or say why this system cannot run it.
    pub fn new(cfg: &Config) -> Result<Encoder, Error> {
        check(cfg)?;
        let mut session = Session::open()?;
        let codec = ffi_codec(cfg.codec);
        let caps = session.caps(codec).map_err(Error::Unsupported)?;
        let ten_bit = cfg.input == Input::P010;
        if ten_bit && !caps.ten_bit {
            return Err(Error::Unsupported("this GPU's encoder has no 10-bit support".into()));
        }
        let (w, h) = (cfg.width, cfg.height);
        if w < caps.min_size.0 || h < caps.min_size.1 || w > caps.max_size.0 || h > caps.max_size.1 {
            let (min, max) = (caps.min_size, caps.max_size);
            return Err(Error::Unsupported(format!("{w}x{h} is outside the encoder's {}x{} to {}x{}", min.0, min.1, max.0, max.1)));
        }
        let rate = match cfg.rate {
            Rate::Quality(q) => nvenc_ffi::Rate::Quality(q),
            Rate::Cbr(kbps) => nvenc_ffi::Rate::Cbr(kbps),
        };
        let params = nvenc_ffi::Params { codec, width: w, height: h, fps: cfg.fps, rate, gop: cfg.keyint.max(1), ten_bit };
        session.initialize(&params, RING).map_err(Error::Unsupported)?;
        let sets = session.sequence_params().map_err(Error::Failed)?;
        let record = match cfg.codec {
            Codec::H264 => match annexb::parameter_sets(Codec::H264, &sets, &[(7, "SPS"), (8, "PPS")]).map_err(Error::Failed)?.as_slice() {
                [sps, pps] => avc::avcc(sps, pps),
                _ => Err("the encoder returned no SPS or PPS".into()),
            },
            Codec::Hevc => {
                match annexb::parameter_sets(Codec::Hevc, &sets, &[(32, "VPS"), (33, "SPS"), (34, "PPS")]).map_err(Error::Failed)?.as_slice() {
                    [vps, sps, pps] => hevc::hvcc(vps, sps, pps, (w, h), if ten_bit { 10 } else { 8 }),
                    _ => Err("the encoder returned no VPS, SPS or PPS".into()),
                }
            }
        }
        .map_err(Error::Failed)?;
        let slots = session.slots();
        Ok(Encoder { session, cfg: *cfg, record, free: (0..slots).rev().collect(), pending: VecDeque::new(), ready: 0 })
    }

    /// The `avcC` (H.264) or `hvcC` (HEVC) record a container stores with the track.
    pub fn codec_private(&self) -> &[u8] {
        &self.record
    }

    /// The settings the encoder runs with.
    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// Encode `frame` (the encoder's [`Input`] layout and size) with the timestamp `pts`, which
    /// must not be negative. Returns the packets that came out, oldest first.
    pub fn encode(&mut self, frame: &[u8], pts: i64) -> Result<Vec<Packet>, Error> {
        let expected = self.cfg.input.frame_bytes(self.cfg.width, self.cfg.height);
        if frame.len() != expected {
            return Err(Error::Failed(format!(
                "a frame of {} bytes is not a {:?} frame of {}x{}",
                frame.len(),
                self.cfg.input,
                self.cfg.width,
                self.cfg.height
            )));
        }
        let pts = u64::try_from(pts).map_err(|_| Error::Failed(format!("the timestamp {pts} is negative")))?;
        let mut out = Vec::new();
        let slot = match self.free.pop() {
            Some(s) => s,
            None => {
                if self.ready == 0 {
                    return Err(Error::Failed("the encoder holds more frames than it has buffers".into()));
                }
                out.push(self.read_oldest()?);
                self.free.pop().ok_or_else(|| Error::Failed("no free encoder buffer".into()))?
            }
        };
        let (row, h) = ((self.cfg.width as usize).saturating_mul(self.cfg.input.bytes_per_sample()), self.cfg.height as usize);
        let submitted = match self.session.submit(slot, pts, |l| fill(l, frame, row, h)) {
            Ok(s) => s,
            Err(e) => {
                self.free.push(slot);
                return Err(Error::Failed(e));
            }
        };
        self.pending.push_back(slot);
        // In synchronous mode a picture that returns success makes the output of every picture
        // queued before it ready as well, including those that returned NEED_MORE_INPUT; they are
        // locked in the order they were queued.
        if submitted == Submitted::Ready {
            self.ready = self.pending.len();
        }
        while self.ready > 0 {
            out.push(self.read_oldest()?);
        }
        Ok(out)
    }

    /// End the stream: every frame still inside comes out.
    pub fn flush(&mut self) -> Result<Vec<Packet>, Error> {
        if !self.pending.is_empty() {
            self.session.end_of_stream().map_err(Error::Failed)?;
            self.ready = self.pending.len();
        }
        let mut out = Vec::new();
        while self.ready > 0 {
            out.push(self.read_oldest()?);
        }
        Ok(out)
    }

    fn read_oldest(&mut self) -> Result<Packet, Error> {
        let slot = self.pending.pop_front().ok_or_else(|| Error::Failed("no frame in flight".into()))?;
        self.ready = self.ready.saturating_sub(1);
        let read = self.session.read(slot);
        self.free.push(slot);
        let o = read.map_err(Error::Failed)?;
        let data = annexb::to_length_prefixed(self.cfg.codec, &o.data).map_err(Error::Failed)?;
        Ok(Packet { data, pts: i64::try_from(o.pts).unwrap_or(i64::MAX), keyframe: o.key })
    }
}

/// Copy a frame of `h` luma rows and `h / 2` chroma rows of `row` bytes each into a locked input
/// buffer whose rows are `pitch` bytes apart. A row the buffer cannot hold is skipped.
fn fill(l: Locked<'_>, frame: &[u8], row: usize, h: usize) {
    if row == 0 || l.pitch < row {
        return;
    }
    let (luma, chroma) = frame.split_at(row.saturating_mul(h).min(frame.len()));
    let (dst_luma, dst_chroma) = l.data.split_at_mut(l.pitch.saturating_mul(h).min(l.data.len()));
    for (src, dst) in luma.chunks_exact(row).zip(dst_luma.chunks_mut(l.pitch)).take(h) {
        if let Some(d) = dst.get_mut(..row) {
            d.copy_from_slice(src);
        }
    }
    for (src, dst) in chroma.chunks_exact(row).zip(dst_chroma.chunks_mut(l.pitch)).take(h / 2) {
        if let Some(d) = dst.get_mut(..row) {
            d.copy_from_slice(src);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(codec: Codec, input: Input) -> Config {
        Config { codec, input, width: 256, height: 144, fps: (30, 1), rate: Rate::Quality(23), keyint: 60 }
    }

    #[test]
    fn rows_land_at_the_buffer_pitch_with_chroma_after_the_luma_rows() {
        let (w, h, pitch) = (4usize, 2usize, 8usize);
        let frame: Vec<u8> = (0..12).collect();
        let mut buf = vec![0xaau8; pitch * h * 3 / 2];
        fill(Locked { data: &mut buf, pitch }, &frame, w, h);
        assert_eq!(&buf[..8], &[0, 1, 2, 3, 0xaa, 0xaa, 0xaa, 0xaa]);
        assert_eq!(&buf[8..12], &[4, 5, 6, 7]);
        assert_eq!(&buf[16..20], &[8, 9, 10, 11]);
        assert_eq!(&buf[20..], &[0xaa; 4]);
    }

    #[test]
    fn p010_rows_are_two_bytes_per_sample() {
        let input = Input::P010;
        assert_eq!(input.frame_bytes(4, 2), 24);
        assert_eq!(Input::Nv12.frame_bytes(4, 2), 12);
        let frame: Vec<u8> = (0..24).collect();
        let mut buf = vec![0u8; 16 * 3];
        fill(Locked { data: &mut buf, pitch: 16 }, &frame, 8, 2);
        assert_eq!(&buf[16..24], &frame[8..16]);
        assert_eq!(&buf[32..40], &frame[16..24]);
    }

    #[test]
    fn short_buffers_and_narrow_pitches_are_never_overrun() {
        let frame = vec![7u8; 8 * 4 * 3 / 2];
        for (len, pitch) in [(0, 0), (0, 8), (5, 64), (8 * 6, 4), (8 * 6, 8), (8 * 5, 8)] {
            let mut buf = vec![0u8; len];
            fill(Locked { data: &mut buf, pitch }, &frame, 8, 4);
        }
        fill(Locked { data: &mut [0; 8], pitch: 8 }, &[], 8, 4);
        fill(Locked { data: &mut [0; 8], pitch: 8 }, &[1; 3], 0, 4);
    }

    #[test]
    fn configurations_the_encoder_cannot_take_are_refused_before_the_driver() {
        let ok = cfg(Codec::Hevc, Input::P010);
        assert_eq!(check(&ok), Ok(()));
        for bad in [
            Config { width: 255, ..ok },
            Config { height: 0, ..ok },
            Config { fps: (30, 0), ..ok },
            Config { fps: (0, 1), ..ok },
            Config { codec: Codec::H264, ..ok },
            Config { rate: Rate::Quality(0), ..ok },
            Config { rate: Rate::Quality(52), ..ok },
            Config { rate: Rate::Cbr(0), ..ok },
        ] {
            assert!(matches!(check(&bad), Err(Error::Unsupported(_))), "{bad:?}");
            assert!(matches!(Encoder::new(&bad), Err(Error::Unsupported(_))), "{bad:?} reached the driver");
        }
    }

    #[test]
    fn a_system_without_the_driver_says_so() {
        // On a machine without an NVIDIA GPU (every CI runner) opening is a `NoDevice` error,
        // never a panic; with one, the encoder opens and writes its record.
        for (codec, input) in [(Codec::H264, Input::Nv12), (Codec::Hevc, Input::Nv12)] {
            match Encoder::new(&cfg(codec, input)) {
                Ok(e) => assert!(!e.codec_private().is_empty()),
                Err(Error::NoDevice(why)) => assert!(!why.is_empty()),
                Err(other) => panic!("{codec:?}: {other}"),
            }
        }
        for codec in [Codec::H264, Codec::Hevc] {
            match available(codec) {
                Ok(()) => {}
                Err(Error::NoDevice(why)) => assert!(!why.is_empty()),
                Err(other) => panic!("{codec:?}: {other}"),
            }
        }
    }

    #[test]
    fn an_encoder_round_trips_frames_when_a_gpu_is_present() {
        let Ok(mut e) = Encoder::new(&cfg(Codec::H264, Input::Nv12)) else { return };
        let frame = vec![128u8; Input::Nv12.frame_bytes(256, 144)];
        let mut packets = Vec::new();
        for pts in 0..10 {
            packets.extend(e.encode(&frame, pts).unwrap());
        }
        packets.extend(e.flush().unwrap());
        assert_eq!(packets.iter().map(|p| p.pts).collect::<Vec<_>>(), (0..10).collect::<Vec<_>>());
        assert!(packets[0].keyframe);
        assert!(e.encode(&frame[1..], 10).is_err(), "a short frame");
        assert!(e.encode(&frame, -1).is_err(), "a negative timestamp");
    }
}
