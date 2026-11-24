//! The hardware H.264, HEVC and AV1 encoders that Windows Media Foundation lists (the GPU vendors'
//! encoder MFTs), through `mfplat.dll`, which is loaded at run time. Frames go in as NV12 in system
//! memory. H.264 and HEVC come out as Annex B, which [`Encoder`] turns into 4-byte length-prefixed
//! samples with the parameter sets in the `avcC` or `hvcC` record ([`Encoder::codec_private`]).
//! AV1 comes out as OBUs, which [`crate::av1::Stream`] turns into samples, with the sequence
//! header in the `av1C` record; the output type may carry the sequence header as OBUs or as an
//! `av1C` record.
//!
//! - Quality mode sets the encoder's quality to `100 - 2 × level`, 0 to 98. Bitrate mode is
//!   constant bitrate with a one-second buffer.
//! - The encoder is asked for no B-frames, and each packet's decoding time is its presentation
//!   time. A frame that comes out before one that went in earlier is an error.
//! - The media types say BT.709 at limited range.
//! - When the encoder's output type carries no parameter sets, [`Encoder::new`] encodes one gray
//!   frame in a second instance of the encoder and takes them from its output. A keyframe whose
//!   own parameter sets differ from the record is an error.

use std::collections::VecDeque;

use crate::mf_ffi::{self, OpenError, Output, Params, Transform};
use crate::{Codec, annexb, av1, avc, hevc};
pub use crate::{Color, Config, Error, Input, Packet, Rate};

impl From<OpenError> for Error {
    fn from(e: OpenError) -> Error {
        match e {
            OpenError::NoDevice(why) => Error::NoDevice(why),
            OpenError::Busy(why) => Error::Busy(why),
            OpenError::Unsupported(why) => Error::Unsupported(why),
        }
    }
}

/// Media Foundation's clock: 100 ns units.
const TICKS_PER_SECOND: i128 = 10_000_000;

/// Whether Media Foundation loads and starts on this system. It is loaded once per process, so
/// later calls cost nothing.
pub fn driver() -> Result<(), Error> {
    mf_ffi::loaded().map_err(Error::NoDevice)
}

/// Whether Media Foundation lists a hardware encoder that writes `codec` and takes a 256 × 144
/// stream at 30 frames per second. It opens the encoder and closes it again.
pub fn available(codec: Codec) -> Result<(), Error> {
    driver()?;
    let cfg = Config { codec, input: Input::Nv12, width: 256, height: 144, fps: (30, 1), rate: Rate::Quality(23), keyint: 60, color: Color::Bt709 };
    Transform::open(&params(&cfg)).map(drop).map_err(Error::from)
}

fn params(cfg: &Config) -> Params {
    Params { codec: cfg.codec, width: cfg.width, height: cfg.height, fps: cfg.fps, rate: cfg.rate, gop: cfg.keyint.max(1) }
}

/// The widest and tallest frame this crate sends to a Media Foundation encoder.
const MAX_SIDE: u32 = 8192;

/// Check `cfg` without Media Foundation: sizes, rate and input.
fn check(cfg: &Config) -> Result<(), Error> {
    let bad = |e: String| Err(Error::Unsupported(e));
    let (w, h) = (cfg.width, cfg.height);
    if w == 0 || h == 0 || w % 2 != 0 || h % 2 != 0 {
        return bad(format!("{w}x{h}: the encoder needs even, nonzero dimensions"));
    }
    if w > MAX_SIDE || h > MAX_SIDE {
        return bad(format!("{w}x{h}: the encoders take at most {MAX_SIDE} pixels a side"));
    }
    if cfg.fps.0 == 0 || cfg.fps.1 == 0 {
        return bad(format!("the frame rate {}/{} is not a rate", cfg.fps.0, cfg.fps.1));
    }
    if cfg.input != Input::Nv12 {
        return bad("the Media Foundation encoders take NV12 frames only".into());
    }
    if cfg.color != Color::Bt709 {
        return bad("the Media Foundation encoders write BT.709 only".into());
    }
    match cfg.rate {
        Rate::Quality(q) if !(1..=51).contains(&q) => bad(format!("the quality level {q} is outside 1 to 51")),
        Rate::Cbr(0) => bad("a bitrate of 0 kbit/s".into()),
        Rate::Cbr(kbps) if kbps > u32::MAX / 1000 => bad(format!("a bitrate of {kbps} kbit/s")),
        _ => Ok(()),
    }
}

/// The NAL unit types of `codec`'s parameter sets, in the order the record takes them. AV1 has
/// none: its sequence header is an OBU.
pub(crate) fn set_types(codec: Codec) -> &'static [(u8, &'static str)] {
    match codec {
        Codec::H264 => &[(7, "SPS"), (8, "PPS")],
        Codec::Hevc => &[(32, "VPS"), (33, "SPS"), (34, "PPS")],
        Codec::Av1 => &[],
    }
}

/// The track of the encoder's `header` and the record a container stores: the parameter sets of
/// an Annex B header with the `avcC` or `hvcC` record made of them, or an AV1 sequence header with
/// its `av1C` record. The AMF encoders use it too.
pub(crate) fn track_of(cfg: &Config, header: &[u8]) -> Result<(Track, Vec<u8>), Error> {
    if cfg.codec == Codec::Av1 {
        let stream = av1_stream(header)?;
        let record = stream.record().to_vec();
        return Ok((Track::av1(cfg.fps, stream), record));
    }
    let sets = annexb::parameter_sets(cfg.codec, header, set_types(cfg.codec)).map_err(Error::Failed)?;
    let record = match (cfg.codec, sets.as_slice()) {
        (Codec::H264, [sps, pps]) => avc::avcc(sps, pps),
        (Codec::Hevc, [vps, sps, pps]) => hevc::hvcc(vps, sps, pps, (cfg.width, cfg.height), 8),
        _ => Err("the encoder returned no parameter sets".into()),
    }
    .map_err(Error::Failed)?;
    Ok((Track::new(cfg.codec, cfg.fps, sets), record))
}

/// The 8-bit AV1 stream of the sequence header in `header`: OBUs, or an `av1C` record whose
/// configuration OBUs hold it. A record starts with its marker bit set, which the first byte of an
/// OBU never has.
fn av1_stream(header: &[u8]) -> Result<av1::Stream, Error> {
    let obus = match header {
        [marker, _, _, _, obus @ ..] if marker & 0x80 != 0 => obus,
        _ => header,
    };
    av1::Stream::new(obus, 8).map_err(Error::Failed)
}

/// Whether the Annex B `data` holds a picture a decoder can start at: IDR for H.264, an IRAP
/// picture (types 16 to 21) for HEVC.
fn is_keyframe(codec: Codec, data: &[u8]) -> bool {
    annexb::nals(data).iter().any(|n| match (codec, annexb::nal_type(codec, n)) {
        (Codec::H264, Some(t)) => t == 5,
        (Codec::Hevc, Some(t)) => (16..=21).contains(&t),
        (_, None) | (Codec::Av1, _) => false,
    })
}

/// The frames of one stream between the encoder and its packets: the Media Foundation time each
/// frame goes in at, and the timestamp and parameter sets (AV1: sequence header) each packet must
/// come out with. The AMF encoders, whose clock also counts 100 ns units, use it too.
pub(crate) struct Track {
    codec: Codec,
    fps: (u32, u32),
    sets: Vec<Vec<u8>>,
    av1: Option<av1::Stream>,
    frames: u64,
    /// Frames sent and not returned, oldest first: their Media Foundation time and timestamp.
    pub(crate) pending: VecDeque<(i64, i64)>,
}

impl Track {
    pub(crate) fn new(codec: Codec, fps: (u32, u32), sets: Vec<Vec<u8>>) -> Track {
        Track { codec, fps, sets, av1: None, frames: 0, pending: VecDeque::new() }
    }

    /// The codec of the stream.
    pub(crate) fn codec(&self) -> Codec {
        self.codec
    }

    fn av1(fps: (u32, u32), stream: av1::Stream) -> Track {
        Track { av1: Some(stream), ..Track::new(Codec::Av1, fps, Vec::new()) }
    }

    /// The Media Foundation time of frame `n`, from the frame rate.
    fn time_of(&self, n: u64) -> i64 {
        let t = i128::from(n) * TICKS_PER_SECOND * i128::from(self.fps.1) / i128::from(self.fps.0.max(1));
        i64::try_from(t).unwrap_or(i64::MAX)
    }

    /// The time and duration the next frame goes in with, remembered with its timestamp `pts`.
    pub(crate) fn send(&mut self, pts: i64) -> (i64, i64) {
        let (time, next) = (self.time_of(self.frames), self.time_of(self.frames.saturating_add(1)));
        self.frames = self.frames.saturating_add(1);
        self.pending.push_back((time, pts));
        (time, next.saturating_sub(time).max(1))
    }

    /// The packet of one output sample. A sample at a later time than the oldest frame sent means
    /// the encoder dropped the frames before it; a sample at an earlier time is out of order.
    pub(crate) fn packet(&mut self, o: Output) -> Result<Packet, Error> {
        let pts = loop {
            let Some((time, pts)) = self.pending.pop_front() else {
                return Err(Error::Failed("the encoder returned more frames than it was given".into()));
            };
            match o.time {
                None => break pts,
                Some(t) if t == time => break pts,
                Some(t) if t > time => continue,
                Some(_) => return Err(Error::Failed("the encoder returned a frame out of order".into())),
            }
        };
        if let Some(stream) = &self.av1 {
            let (data, keyframe) = stream.sample(&o.data).map_err(Error::Failed)?;
            return Ok(Packet { data, pts, keyframe });
        }
        let keyframe = is_keyframe(self.codec, &o.data);
        self.sets_in(&o.data)?;
        let data = annexb::to_length_prefixed(self.codec, &o.data).map_err(Error::Failed)?;
        Ok(Packet { data, pts, keyframe })
    }

    /// Check that each parameter set in the Annex B `data` of a sample is the set of that type the
    /// container record was made of; the packet goes out without them.
    fn sets_in(&self, data: &[u8]) -> Result<(), Error> {
        let types = set_types(self.codec);
        for n in annexb::nals(data) {
            let Some(i) = annexb::nal_type(self.codec, n).and_then(|t| types.iter().position(|&(ty, _)| ty == t)) else { continue };
            if self.sets.get(i).map(Vec::as_slice) != Some(n) {
                let name = types.get(i).map_or("parameter sets", |&(_, name)| name);
                return Err(Error::Failed(format!("the encoder changed its {name}")));
            }
        }
        Ok(())
    }

    /// Check that the `header` of a new output type holds the parameter sets (AV1: the sequence
    /// header) the container record was made of.
    pub(crate) fn same_sets(&self, header: &[u8]) -> Result<(), Error> {
        if let Some(stream) = &self.av1 {
            return match av1_stream(header) {
                Ok(s) if s.same_sequence_header(stream) => Ok(()),
                _ => Err(Error::Failed("the encoder changed its sequence header".into())),
            };
        }
        match annexb::parameter_sets(self.codec, header, set_types(self.codec)) {
            Ok(sets) if sets == self.sets => Ok(()),
            _ => Err(Error::Failed("the encoder changed its parameter sets".into())),
        }
    }
}

/// A Media Foundation hardware encoder.
pub struct Encoder {
    transform: Transform,
    cfg: Config,
    record: Vec<u8>,
    track: Track,
    /// The parameter sets the encoder's output type carried when they were last checked.
    header: Option<Vec<u8>>,
}

impl Encoder {
    /// Open an encoder for `cfg`, or say why this system cannot run it.
    pub fn new(cfg: &Config) -> Result<Encoder, Error> {
        check(cfg)?;
        let p = params(cfg);
        let mut transform = Transform::open(&p)?;
        let header = match transform.sequence_header() {
            Some(h) => h.to_vec(),
            None => {
                // Some GPUs allow one encoding session, so this instance closes before the
                // gray frame's instance opens. Off Windows the stub holds nothing to drop.
                #[cfg_attr(not(target_os = "windows"), allow(clippy::drop_non_drop))]
                drop(transform);
                let h = first_frame(cfg, &p)?;
                transform = Transform::open(&p)?;
                h
            }
        };
        let (track, record) = track_of(cfg, &header)?;
        let header = transform.sequence_header().map(<[u8]>::to_vec);
        let mut encoder = Encoder { transform, cfg: *cfg, record, track, header: None };
        if let Some(h) = &header {
            encoder.track.same_sets(h)?;
        }
        encoder.header = header;
        Ok(encoder)
    }

    /// Check the parameter sets of the encoder's output type again after it changed.
    fn check_header(&mut self) -> Result<(), Error> {
        let now = self.transform.sequence_header();
        if now == self.header.as_deref() {
            return Ok(());
        }
        let Some(now) = now.map(<[u8]>::to_vec) else { return Ok(()) };
        self.track.same_sets(&now)?;
        self.header = Some(now);
        Ok(())
    }

    /// The `avcC` (H.264), `hvcC` (HEVC) or `av1C` (AV1) record a container stores with the track.
    pub fn codec_private(&self) -> &[u8] {
        &self.record
    }

    /// The settings the encoder runs with.
    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// Encode `frame` (NV12 of the configured size) with the timestamp `pts`, which must not be
    /// negative. Returns the packets that came out, oldest first.
    pub fn encode(&mut self, frame: &[u8], pts: i64) -> Result<Vec<Packet>, Error> {
        let expected = Input::Nv12.frame_bytes(self.cfg.width, self.cfg.height);
        if frame.len() != expected {
            return Err(Error::Failed(format!("a frame of {} bytes is not an NV12 frame of {}x{}", frame.len(), self.cfg.width, self.cfg.height)));
        }
        if pts < 0 {
            return Err(Error::Failed(format!("the timestamp {pts} is negative")));
        }
        let (time, duration) = self.track.send(pts);
        let mut outputs = Vec::new();
        let sent = self.transform.encode(frame, time, duration, &mut outputs);
        if sent.is_err() {
            self.track.pending.pop_back();
        }
        self.check_header()?;
        let packets = outputs.into_iter().map(|o| self.track.packet(o)).collect::<Result<Vec<_>, _>>()?;
        sent.map_err(Error::Failed)?;
        Ok(packets)
    }

    /// End the stream: every frame still inside comes out.
    pub fn flush(&mut self) -> Result<Vec<Packet>, Error> {
        let mut outputs = Vec::new();
        self.transform.drain(&mut outputs).map_err(Error::Failed)?;
        self.check_header()?;
        outputs.into_iter().map(|o| self.track.packet(o)).collect()
    }
}

/// One gray NV12 frame of `cfg`'s size, allocated fallibly: an 8192 × 8192 frame needs 96 MiB.
/// The AMF encoders use it too.
pub(crate) fn gray_frame(cfg: &Config) -> Result<Vec<u8>, Error> {
    let luma = (cfg.width as usize).saturating_mul(cfg.height as usize);
    let size = luma.saturating_add(luma / 2);
    let mut gray = Vec::new();
    gray.try_reserve_exact(size).map_err(|e| Error::Failed(format!("no memory for a {}x{} frame: {e}", cfg.width, cfg.height)))?;
    gray.resize(luma, 16u8);
    gray.resize(size, 128u8);
    Ok(gray)
}

/// The output of one gray frame from a separate instance of the encoder for `cfg`: the parameter
/// sets of an encoder whose output type carries none.
fn first_frame(cfg: &Config, p: &Params) -> Result<Vec<u8>, Error> {
    let mut t = Transform::open(p)?;
    let gray = gray_frame(cfg)?;
    let (time, duration) = Track::new(cfg.codec, cfg.fps, Vec::new()).send(0);
    let mut out = Vec::new();
    t.encode(&gray, time, duration, &mut out).map_err(Error::Failed)?;
    t.drain(&mut out).map_err(Error::Failed)?;
    let data: Vec<u8> = out.into_iter().flat_map(|o| o.data).collect();
    if data.is_empty() {
        return Err(Error::Failed("the encoder returned no parameter sets and no frame".into()));
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(codec: Codec) -> Config {
        Config { codec, input: Input::Nv12, width: 256, height: 144, fps: (30, 1), rate: Rate::Quality(23), keyint: 60, color: Color::Bt709 }
    }

    /// An Annex B access unit of the NAL units `nals`, each a header byte (two for HEVC) and one
    /// byte of payload.
    fn annex_b(nals: &[&[u8]]) -> Vec<u8> {
        nals.iter().flat_map(|n| [&[0u8, 0, 0, 1][..], *n].concat()).collect()
    }

    const SPS: &[u8] = &[0x67, 1];
    const PPS: &[u8] = &[0x68, 2];
    const IDR: &[u8] = &[0x65, 3];
    const P: &[u8] = &[0x41, 4];

    fn track() -> Track {
        Track::new(Codec::H264, (30000, 1001), vec![SPS.to_vec(), PPS.to_vec()])
    }

    #[test]
    fn configurations_the_encoder_cannot_take_are_refused_before_media_foundation() {
        let ok = cfg(Codec::Hevc);
        assert_eq!(check(&ok), Ok(()));
        for bad in [
            Config { width: 255, ..ok },
            Config { height: 0, ..ok },
            Config { width: 8194, ..ok },
            Config { height: u32::MAX - 1, ..ok },
            Config { fps: (30, 0), ..ok },
            Config { fps: (0, 1), ..ok },
            Config { input: Input::P010, ..ok },
            Config { color: Color::Hlg, ..ok },
            Config { color: Color::Pq { mastering: None, light: None }, ..ok },
            Config { rate: Rate::Quality(0), ..ok },
            Config { rate: Rate::Quality(52), ..ok },
            Config { rate: Rate::Cbr(0), ..ok },
            Config { rate: Rate::Cbr(u32::MAX / 1000 + 1), ..ok },
        ] {
            assert!(matches!(check(&bad), Err(Error::Unsupported(_))), "{bad:?}");
            assert!(matches!(Encoder::new(&bad), Err(Error::Unsupported(_))), "{bad:?} reached Media Foundation");
        }
    }

    #[test]
    fn levels_map_to_the_encoders_quality_scale_and_quality_mode_names_a_bitrate() {
        assert_eq!([1, 23, 50, 51].map(mf_ffi::quality), [98, 54, 0, 0]);
        let p = params(&Config { width: 1920, height: 1080, fps: (60, 1), ..cfg(Codec::H264) });
        assert_eq!(mf_ffi::nominal_bitrate(&p), 12_441_600);
        let huge = Params { width: u32::MAX, height: u32::MAX, fps: (u32::MAX, 1), ..p };
        assert_eq!(mf_ffi::nominal_bitrate(&huge), u32::MAX);
        assert_eq!(mf_ffi::nominal_bitrate(&Params { width: 2, height: 2, ..p }), 24);
        // The buffer size is in bytes: one second at 6000 kbit/s is 750,000 bytes.
        assert_eq!([1, 6000, u32::MAX].map(mf_ffi::cbr_buffer_bytes), [125, 750_000, u32::MAX]);
    }

    #[test]
    fn output_buffers_hold_a_raw_frame_and_an_encoder_cannot_ask_for_any_size() {
        assert_eq!(mf_ffi::output_size(0, 1920, 1080), Ok(3_110_400));
        assert_eq!(mf_ffi::output_size(0, 16, 16), Ok(1 << 20));
        assert_eq!(mf_ffi::output_size(64 << 20, 16, 16), Ok(64 << 20));
        assert!(mf_ffi::output_size((64 << 20) + 1, 16, 16).is_err());
        assert_eq!(mf_ffi::output_size(99_532_800, 7680, 4320), Ok(99_532_800), "two raw 8K frames");
        assert!(mf_ffi::output_size(99_532_801, 7680, 4320).is_err());
        assert!(mf_ffi::output_size(u32::MAX, u32::MAX, u32::MAX).is_err());
    }

    #[test]
    fn output_buffers_this_side_allocates_take_the_alignment_the_encoder_asks_for() {
        assert_eq!(mf_ffi::alignment_flag(0), Ok(None));
        assert_eq!(mf_ffi::alignment_flag(1), Ok(None));
        assert_eq!([2, 16, 64, 512].map(mf_ffi::alignment_flag), [Ok(Some(1)), Ok(Some(0x0f)), Ok(Some(0x3f)), Ok(Some(0x1ff))]);
        for bad in [3, 24, 1024, u32::MAX] {
            assert!(mf_ffi::alignment_flag(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn one_output_event_reads_one_sample_and_only_a_type_change_reads_again() {
        use mf_ffi::{Read, read_again};
        // A sample ends the read, also one the encoder marks incomplete: the rest comes with its
        // next METransformHaveOutput event.
        assert_eq!(read_again(Read::Done, 0), Ok(false));
        assert_eq!(read_again(Read::StreamChanged, 1), Ok(true));
        assert_eq!(read_again(Read::StreamChanged, mf_ffi::MAX_STREAM_CHANGES), Ok(true));
        assert!(read_again(Read::StreamChanged, mf_ffi::MAX_STREAM_CHANGES + 1).is_err());
    }

    #[test]
    fn frames_go_in_at_the_frame_rates_times_and_their_durations_add_up() {
        let mut t = track();
        let sent: Vec<_> = (0..4).map(|pts| t.send(pts * 1001)).collect();
        assert_eq!(sent, [(0, 333_666), (333_666, 333_667), (667_333, 333_667), (1_001_000, 333_666)]);
        assert_eq!(t.time_of(30_000), 10_010_000_000, "1001 seconds after 30000 frames at 29.97");
        assert_eq!(Track::new(Codec::Hevc, (u32::MAX, 1), Vec::new()).send(0), (0, 1), "a frame is never 0 ticks long");
    }

    #[test]
    fn packets_take_the_timestamps_their_frames_went_in_with() {
        let mut t = track();
        let times: Vec<_> = (0..3).map(|pts| t.send(100 + pts).0).collect();
        let key = t.packet(Output { data: annex_b(&[SPS, PPS, IDR]), time: Some(times[0]) }).unwrap();
        assert_eq!((key.pts, key.keyframe), (100, true));
        assert_eq!(key.data, [0, 0, 0, 2, 0x65, 3], "the parameter sets are in the record, not the packet");
        // A sample without a time is the oldest frame; a later sample skips a dropped frame.
        assert_eq!(t.packet(Output { data: annex_b(&[P]), time: None }).unwrap().pts, 101);
        t.send(103);
        let p = t.packet(Output { data: annex_b(&[P]), time: Some(t.time_of(3)) }).unwrap();
        assert_eq!((p.pts, p.keyframe), (103, false));
        assert!(t.pending.is_empty());
        assert!(t.packet(Output { data: annex_b(&[P]), time: None }).is_err(), "more frames than went in");
    }

    #[test]
    fn a_new_output_type_must_keep_the_parameter_sets_of_the_record() {
        let t = track();
        assert!(t.same_sets(&annex_b(&[SPS, PPS])).is_ok());
        assert!(t.same_sets(&annex_b(&[&[0x67, 9], PPS])).is_err(), "another SPS");
        assert!(t.same_sets(&annex_b(&[SPS])).is_err(), "no PPS");
        assert!(t.same_sets(&[]).is_err());
    }

    #[test]
    fn a_frame_out_of_order_or_with_other_parameter_sets_is_an_error() {
        let mut t = track();
        let (first, _) = t.send(0);
        let (second, _) = t.send(1);
        let early = t.packet(Output { data: annex_b(&[P]), time: Some(first - 1) });
        assert!(matches!(early, Err(Error::Failed(why)) if why.contains("out of order")));
        let mut t = track();
        t.send(0);
        let other = t.packet(Output { data: annex_b(&[&[0x67, 9], PPS, IDR]), time: Some(0) });
        assert!(matches!(other, Err(Error::Failed(why)) if why.contains("changed its SPS")));
        // A keyframe without parameter sets of its own is fine.
        t.send(1);
        assert!(t.packet(Output { data: annex_b(&[IDR]), time: Some(second) }).unwrap().keyframe);
        // Each set a sample carries is checked, alone too and in any picture.
        t.send(2);
        assert!(t.packet(Output { data: annex_b(&[SPS, IDR]), time: None }).is_ok(), "the same SPS alone");
        t.send(3);
        let pps = t.packet(Output { data: annex_b(&[&[0x68, 7], P]), time: None });
        assert!(matches!(pps, Err(Error::Failed(why)) if why.contains("changed its PPS")));
    }

    #[test]
    fn keyframes_are_found_from_the_nal_unit_types() {
        assert!(is_keyframe(Codec::H264, &annex_b(&[SPS, PPS, IDR])));
        assert!(!is_keyframe(Codec::H264, &annex_b(&[P])));
        for t in [16u8, 19, 20, 21] {
            assert!(is_keyframe(Codec::Hevc, &annex_b(&[&[t << 1, 1, 0]])), "{t}");
        }
        for t in [0u8, 1, 15, 22, 32] {
            assert!(!is_keyframe(Codec::Hevc, &annex_b(&[&[t << 1, 1, 0]])), "{t}");
        }
        assert!(!is_keyframe(Codec::Hevc, &[]));
    }

    /// The sequence header OBU of `av1.rs`'s tests (8-bit 4:2:0 Main at level 4, 256 × 144), and
    /// OBUs with a one-byte size: a temporal delimiter and a frame whose header starts with a shown
    /// key frame or an inter frame.
    const SEQ_OBU: [u8; 12] = [0x0a, 0x0a, 0x00, 0x00, 0x00, 0x23, 0xbf, 0xfc, 0x78, 0x00, 0x00, 0x10];
    const TD: [u8; 2] = [0x12, 0x00];
    const KEY_FRAME: [u8; 4] = [0x32, 0x02, 0x10, 0x01];
    const INTER_FRAME: [u8; 4] = [0x32, 0x02, 0x30, 0x01];

    #[test]
    fn av1_output_types_carry_the_sequence_header_as_obus_or_as_a_record() {
        let from_obus = av1_stream(&[&TD[..], &SEQ_OBU].concat()).unwrap();
        let record = from_obus.record().to_vec();
        assert_eq!(&record[..4], &[0x81, 0x04, 0x0c, 0x00]);
        let from_record = av1_stream(&record).unwrap();
        assert!(from_record.same_sequence_header(&from_obus));
        assert!(av1_stream(&[0x81, 0x04, 0x0c]).is_err(), "a cut record");
        assert!(av1_stream(&[]).is_err());
        let cfg = Config { codec: Codec::Av1, ..cfg(Codec::H264) };
        let (track, r) = track_of(&cfg, &SEQ_OBU).unwrap();
        assert_eq!(r, record);
        assert!(track.same_sets(&record).is_ok());
        assert!(track.same_sets(&SEQ_OBU).is_ok());
        let mut other = SEQ_OBU;
        other[5] = 0x2b;
        assert!(matches!(track.same_sets(&other), Err(Error::Failed(why)) if why.contains("sequence header")));
    }

    #[test]
    fn av1_packets_are_samples_without_temporal_delimiters_with_keyframes_from_the_frame_header() {
        let mut t = Track::av1((30, 1), av1_stream(&SEQ_OBU).unwrap());
        let times: Vec<_> = (0..3).map(|pts| t.send(pts).0).collect();
        let key = t.packet(Output { data: [&TD[..], &SEQ_OBU, &KEY_FRAME].concat(), time: Some(times[0]) }).unwrap();
        assert_eq!((key.pts, key.keyframe), (0, true));
        assert_eq!(key.data, [&SEQ_OBU[..], &KEY_FRAME].concat());
        let inter = t.packet(Output { data: [&TD[..], &INTER_FRAME].concat(), time: None }).unwrap();
        assert_eq!((inter.pts, inter.keyframe, inter.data), (1, false, INTER_FRAME.to_vec()));
        let mut other = SEQ_OBU;
        other[5] = 0x2b;
        let changed = t.packet(Output { data: [&other[..], &KEY_FRAME].concat(), time: Some(times[2]) });
        assert!(matches!(changed, Err(Error::Failed(why)) if why.contains("changed its sequence header")));
        t.send(3);
        assert!(t.packet(Output { data: TD.to_vec(), time: None }).is_err(), "no frame");
        assert!(set_types(Codec::Av1).is_empty());
        assert!(!is_keyframe(Codec::Av1, &KEY_FRAME));
    }

    #[test]
    fn a_system_without_a_media_foundation_encoder_says_so() {
        // Off Windows (and on every CI runner without a GPU) opening is a `NoDevice` error, never
        // a panic; with a hardware encoder, it opens and writes its record, or says that the
        // encoder does not take the stream.
        for codec in [Codec::H264, Codec::Hevc, Codec::Av1] {
            match Encoder::new(&cfg(codec)) {
                Ok(e) => assert!(!e.codec_private().is_empty()),
                Err(Error::NoDevice(why) | Error::Unsupported(why)) => assert!(!why.is_empty()),
                Err(other) => panic!("{codec:?}: {other}"),
            }
            match available(codec) {
                Ok(()) => {}
                Err(Error::NoDevice(why) | Error::Unsupported(why)) => assert!(!why.is_empty()),
                Err(other) => panic!("{codec:?}: {other}"),
            }
        }
        #[cfg(not(target_os = "windows"))]
        assert_eq!(driver(), Err(Error::NoDevice("Media Foundation runs on Windows only".into())));
    }

    #[test]
    fn an_encoder_round_trips_frames_when_a_hardware_encoder_is_present() {
        let Ok(mut e) = Encoder::new(&cfg(Codec::H264)) else { return };
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
