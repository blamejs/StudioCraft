//! AMD's hardware H.264, HEVC and AV1 encoders through AMF (Advanced Media Framework), with
//! `amfrt64.dll`, which AMD's graphics driver installs, loaded at run time. Frames go in as NV12
//! in system memory. H.264 and HEVC come out as Annex B, which [`Encoder`] turns into 4-byte
//! length-prefixed samples with the parameter sets in the `avcC` or `hvcC` record
//! ([`Encoder::codec_private`]). AV1 comes out as OBUs, which [`crate::av1::Stream`] turns into
//! samples, with the sequence header in the `av1C` record.
//!
//! - Quality mode is constant QP at the level (AV1: base quantizer index `5 × level`, at most
//!   255). Bitrate mode is constant bitrate with a one-second buffer.
//! - The encoder runs with the transcoding usage and no B-frames, and each packet's decoding time
//!   is its presentation time. A frame that comes out before one that went in earlier is an error.
//! - The stream is BT.709 at limited range.
//! - The parameter sets come from the encoder's extra data after it starts. When it reports none,
//!   [`Encoder::new`] encodes one gray frame in a second instance of the encoder and takes them
//!   from its output; the first frame of the encoder that records must then carry parameter sets
//!   too. A keyframe whose own parameter sets differ from the record is an error.
//! - [`Encoder::encode`] hands the frame to the encoder and returns the packets that are ready
//!   without waiting for the frame's own; it waits only while the encoder's input queue is full,
//!   for up to 10 seconds. [`Encoder::flush`] waits for the rest, also for up to 10 seconds.

use crate::Codec;
use crate::amf_ffi::{self, Engine, OpenError, Output, Params, Session};
use crate::mf::{Track, gray_frame, set_types, track_of};
pub use crate::{Color, Config, Error, Input, Packet, Rate};

impl From<OpenError> for Error {
    fn from(e: OpenError) -> Error {
        match e {
            OpenError::NoDevice(why) => Error::NoDevice(why),
            OpenError::Unsupported(why) => Error::Unsupported(why),
            OpenError::Failed(why) => Error::Failed(why),
        }
    }
}

/// Whether the AMF runtime loads on this system. It is loaded once per process, so later calls
/// cost nothing.
pub fn driver() -> Result<(), Error> {
    amf_ffi::loaded().map_err(Error::NoDevice)
}

/// Whether an AMD encoder that writes `codec` opens for a 256 × 144 stream at 30 frames per
/// second. It opens the encoder and closes it again.
pub fn available(codec: Codec) -> Result<(), Error> {
    driver()?;
    let cfg = Config { codec, input: Input::Nv12, width: 256, height: 144, fps: (30, 1), rate: Rate::Quality(23), keyint: 60, color: Color::Bt709 };
    Session::open(&params(&cfg)).map(drop).map_err(Error::from)
}

fn params(cfg: &Config) -> Params {
    Params { codec: cfg.codec, width: cfg.width, height: cfg.height, fps: cfg.fps, rate: cfg.rate, gop: cfg.keyint.max(1) }
}

/// The widest and tallest frame this crate sends to an AMF encoder.
const MAX_SIDE: u32 = 8192;

/// Check `cfg` without AMF: sizes, rate and input.
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
        return bad("the AMF encoders take NV12 frames only".into());
    }
    if cfg.color != Color::Bt709 {
        return bad("the AMF encoders write BT.709 only".into());
    }
    match cfg.rate {
        Rate::Quality(q) if !(1..=51).contains(&q) => bad(format!("the quality level {q} is outside 1 to 51")),
        Rate::Cbr(0) => bad("a bitrate of 0 kbit/s".into()),
        Rate::Cbr(kbps) if kbps > u32::MAX / 1000 => bad(format!("a bitrate of {kbps} kbit/s")),
        _ => Ok(()),
    }
}

/// An AMD hardware encoder.
pub struct Encoder {
    session: Session,
    cfg: Config,
    record: Vec<u8>,
    track: Track,
    /// Whether the record came from a second instance of the encoder, so the first frame of this
    /// one must carry parameter sets (AV1: a sequence header) to check it against.
    unchecked: bool,
}

impl Encoder {
    /// Open an encoder for `cfg`, or say why this system cannot run it.
    pub fn new(cfg: &Config) -> Result<Encoder, Error> {
        check(cfg)?;
        let p = params(cfg);
        let mut session = Session::open(&p)?;
        let (header, unchecked) = match session.extra_data() {
            Some(h) => (h, false),
            None => {
                // An encoder may allow one session at a time, so this one closes before the gray
                // frame's opens. Off Windows the stub holds nothing to drop.
                #[cfg_attr(not(target_os = "windows"), allow(clippy::drop_non_drop))]
                drop(session);
                let h = first_frame(cfg, &p)?;
                session = Session::open(&p)?;
                (h, true)
            }
        };
        let (track, record) = track_of(cfg, &header)?;
        Ok(Encoder { session, cfg: *cfg, record, track, unchecked })
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
        encode_on(&mut self.session, &mut self.track, &mut self.unchecked, frame, pts, &mut amf_ffi::waiter())
    }

    /// End the stream: every frame still inside comes out.
    pub fn flush(&mut self) -> Result<Vec<Packet>, Error> {
        flush_on(&mut self.session, &mut self.track, &mut self.unchecked, &mut amf_ffi::waiter())
    }
}

/// Whether the Annex B (AV1: OBU) `data` of a frame carries every parameter set (AV1: a sequence
/// header).
fn carries_header(codec: Codec, data: &[u8]) -> bool {
    match codec {
        Codec::Av1 => crate::av1::Stream::new(data, 8).is_ok(),
        _ => crate::annexb::parameter_sets(codec, data, set_types(codec)).is_ok(),
    }
}

/// The packets of `outputs`. While `unchecked`, the first output must carry the parameter sets
/// (AV1: a sequence header), which `track` then checks against the record.
fn packets(track: &mut Track, unchecked: &mut bool, outputs: Vec<Output>) -> Result<Vec<Packet>, Error> {
    if *unchecked && let Some(first) = outputs.first() {
        if !carries_header(track.codec(), &first.data) {
            return Err(Error::Failed("the encoder's first frame carries no parameter sets to check the record against".into()));
        }
        *unchecked = false;
    }
    outputs.into_iter().map(|o| track.packet(o)).collect()
}

/// Drain `engine` and return the packets of every frame still inside.
fn flush_on(engine: &mut impl Engine, track: &mut Track, unchecked: &mut bool, wait: &mut impl FnMut(bool) -> bool) -> Result<Vec<Packet>, Error> {
    let mut outputs = Vec::new();
    amf_ffi::drain(engine, &mut outputs, wait).map_err(Error::Failed)?;
    packets(track, unchecked, outputs)
}

/// Send one frame with the timestamp `pts` to `engine` and return the packets that came out. A
/// frame the encoder did not take is not waited for.
fn encode_on(
    engine: &mut impl Engine,
    track: &mut Track,
    unchecked: &mut bool,
    frame: &[u8],
    pts: i64,
    wait: &mut impl FnMut(bool) -> bool,
) -> Result<Vec<Packet>, Error> {
    let (time, duration) = track.send(pts);
    let mut outputs = Vec::new();
    let sent = amf_ffi::encode(engine, frame, time, duration, &mut outputs, wait);
    if sent.is_err() {
        track.pending.pop_back();
    }
    let packets = packets(track, unchecked, outputs)?;
    sent.map_err(Error::Failed)?;
    Ok(packets)
}

/// The output of one gray frame from a separate instance of the encoder for `cfg`: the parameter
/// sets of an encoder that reports none.
fn first_frame(cfg: &Config, p: &Params) -> Result<Vec<u8>, Error> {
    let mut s = Session::open(p)?;
    let gray = gray_frame(cfg)?;
    let (time, duration) = Track::new(cfg.codec, cfg.fps, Vec::new()).send(0);
    let mut out = Vec::new();
    amf_ffi::encode(&mut s, &gray, time, duration, &mut out, &mut amf_ffi::waiter()).map_err(Error::Failed)?;
    amf_ffi::drain(&mut s, &mut out, &mut amf_ffi::waiter()).map_err(Error::Failed)?;
    let data: Vec<u8> = out.into_iter().flat_map(|o| o.data).collect();
    if data.is_empty() {
        return Err(Error::Failed("the encoder returned no parameter sets and no frame".into()));
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;
    use crate::amf_ffi::{Need, Output, Polled, Submitted, Value};

    fn cfg(codec: Codec) -> Config {
        Config { codec, input: Input::Nv12, width: 256, height: 144, fps: (30, 1), rate: Rate::Quality(23), keyint: 60, color: Color::Bt709 }
    }

    #[test]
    fn configurations_the_encoder_cannot_take_are_refused_before_amf() {
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
            assert!(matches!(Encoder::new(&bad), Err(Error::Unsupported(_))), "{bad:?} reached AMF");
        }
    }

    fn property(props: &[(&'static str, Value, Need)], name: &str) -> Option<(Value, Need)> {
        props.iter().find(|(n, _, _)| *n == name).map(|&(_, v, need)| (v, need))
    }

    #[test]
    fn the_usage_comes_first_and_quality_mode_is_constant_qp_at_the_level() {
        let p = params(&Config { width: 1920, height: 1080, fps: (60000, 1001), rate: Rate::Quality(23), keyint: 120, ..cfg(Codec::H264) });
        let props = amf_ffi::properties(&p);
        assert_eq!(props[0], ("Usage", Value::Int(0), Need::Required));
        assert_eq!(property(&props, "FrameSize"), Some((Value::Size(1920, 1080), Need::Required)));
        assert_eq!(property(&props, "FrameRate"), Some((Value::Rate(60000, 1001), Need::Required)));
        assert_eq!(property(&props, "Profile"), Some((Value::Int(100), Need::Required)), "High");
        assert_eq!(property(&props, "BPicturesPattern"), Some((Value::Int(0), Need::Required)));
        assert_eq!(property(&props, "IDRPeriod"), Some((Value::Int(120), Need::Required)));
        assert_eq!(property(&props, "RateControlMethod"), Some((Value::Int(0), Need::Required)));
        assert_eq!(property(&props, "QPI"), Some((Value::Int(23), Need::Required)));
        assert_eq!(property(&props, "QPP"), Some((Value::Int(23), Need::Required)));
        assert_eq!(property(&props, "TargetBitrate"), None);
        assert_eq!(property(&props, "OutColorPrimaries"), Some((Value::Int(1), Need::Optional)), "BT.709, left out by older drivers");
        let hevc = amf_ffi::properties(&Params { codec: Codec::Hevc, ..p });
        assert_eq!(hevc[0].0, "HevcUsage");
        assert_eq!(property(&hevc, "HevcQP_P"), Some((Value::Int(23), Need::Required)));
        assert_eq!(property(&hevc, "HevcGOPSize"), Some((Value::Int(120), Need::Required)));
        let av1 = amf_ffi::properties(&Params { codec: Codec::Av1, ..p });
        assert_eq!(av1[0].0, "Av1Usage");
        assert_eq!(property(&av1, "Av1QIndex_Intra"), Some((Value::Int(115), Need::Required)));
        assert_eq!(property(&av1, "Av1QIndex_Inter"), Some((Value::Int(115), Need::Required)));
        assert_eq!([1, 23, 51].map(amf_ffi::av1_q_index), [5, 115, 255]);
        // A GOP of 0 frames would mean one keyframe at the start only.
        assert_eq!(property(&amf_ffi::properties(&Params { gop: 0, ..p }), "IDRPeriod"), Some((Value::Int(1), Need::Required)));
    }

    #[test]
    fn bitrate_mode_is_cbr_in_each_codecs_own_enum_with_a_one_second_buffer() {
        let p = params(&Config { rate: Rate::Cbr(6000), ..cfg(Codec::H264) });
        let h264 = amf_ffi::properties(&p);
        assert_eq!(property(&h264, "RateControlMethod"), Some((Value::Int(1), Need::Required)));
        for name in ["TargetBitrate", "PeakBitrate", "VBVBufferSize"] {
            assert_eq!(property(&h264, name), Some((Value::Int(6_000_000), Need::Required)), "{name}");
        }
        assert_eq!(property(&h264, "QPI"), None);
        for (codec, method, target) in
            [(Codec::Hevc, "HevcRateControlMethod", "HevcTargetBitrate"), (Codec::Av1, "Av1RateControlMethod", "Av1TargetBitrate")]
        {
            let props = amf_ffi::properties(&Params { codec, ..p });
            assert_eq!(property(&props, method), Some((Value::Int(3), Need::Required)), "{codec:?}");
            assert_eq!(property(&props, target), Some((Value::Int(6_000_000), Need::Required)), "{codec:?}");
        }
        let top = amf_ffi::properties(&Params { rate: Rate::Cbr(u32::MAX / 1000), ..p });
        assert_eq!(property(&top, "TargetBitrate"), Some((Value::Int(4_294_967_000), Need::Required)), "no overflow");
    }

    #[test]
    fn amf_results_map_to_the_crates_errors() {
        use amf_ffi::abi::*;
        assert!(
            matches!(amf_ffi::refusal("Init", AMF_ENCODER_NOT_PRESENT), OpenError::NoDevice(why) if why == "Init failed (AMF_ENCODER_NOT_PRESENT)")
        );
        assert!(matches!(amf_ffi::refusal("CreateComponent", AMF_NOT_FOUND), OpenError::NoDevice(_)));
        for r in [AMF_CODEC_NOT_SUPPORTED, AMF_NOT_SUPPORTED, AMF_INVALID_RESOLUTION, AMF_INVALID_ARG] {
            assert!(matches!(amf_ffi::refusal("Init", r), OpenError::Unsupported(_)), "{r}");
        }
        assert!(matches!(amf_ffi::refusal("Init", AMF_FAIL), OpenError::Failed(_)));
        assert_eq!(amf_ffi::result_name(9999), "AMF result 9999");
        assert!(amf_ffi::av1_default_alignment_fits(1920, 1088));
        assert!(!amf_ffi::av1_default_alignment_fits(1920, 1080));
    }

    #[test]
    fn the_runtime_is_asked_for_its_own_version_or_the_headers_whichever_is_older() {
        use amf_ffi::abi::{AMF_FULL_VERSION, make_version};
        assert_eq!(amf_ffi::init_version(make_version(1, 4, 36, 0)), Ok(make_version(1, 4, 36, 0)));
        assert_eq!(amf_ffi::init_version(make_version(1, 6, 0, 0)), Ok(AMF_FULL_VERSION));
        assert_eq!(amf_ffi::init_version(make_version(1, 3, 9, 0)), Err("the AMF runtime 1.3.9 is older than 1.4".into()));
    }

    #[test]
    fn frames_are_copied_into_pitched_planes_and_short_planes_are_refused() {
        let src: Vec<u8> = (0..12).collect();
        let mut dst = vec![0xffu8; 3 * 6];
        amf_ffi::copy_plane(&src, 4, 3, &mut dst, 6).unwrap();
        assert_eq!(dst, [0, 1, 2, 3, 0xff, 0xff, 4, 5, 6, 7, 0xff, 0xff, 8, 9, 10, 11, 0xff, 0xff]);
        let mut last_row_short = vec![0u8; 6 * 2 + 4];
        assert!(amf_ffi::copy_plane(&src, 4, 3, &mut last_row_short, 6).is_ok(), "the last row needs only the frame's width");
        assert!(amf_ffi::copy_plane(&src, 4, 3, &mut [0u8; 6 * 2 + 3], 6).is_err());
        assert!(amf_ffi::copy_plane(&src, 4, 3, &mut dst, 3).is_err(), "a pitch below the width");
        assert!(amf_ffi::copy_plane(&src[..11], 4, 3, &mut dst, 6).is_err(), "a short frame");
        assert!(amf_ffi::copy_plane(&[], 4, 0, &mut [], 6).is_ok());
        // The plane sizes a surface reports are checked before its memory is addressed.
        assert_eq!(amf_ffi::plane_extent(4, 3, 6, 3), Ok((6, 16)));
        assert_eq!(amf_ffi::plane_extent(4, 3, 6, i32::MAX), Ok((6, 16)), "a taller plane is read only to the frame's rows");
        assert_eq!(amf_ffi::plane_extent(4, 0, 6, 0), Ok((6, 0)));
        assert!(amf_ffi::plane_extent(4, 3, 3, 3).is_err(), "a pitch below the width");
        assert!(amf_ffi::plane_extent(4, 3, 4 + 4097, 3).is_err(), "a pitch far past the width");
        assert!(amf_ffi::plane_extent(4, 3, i32::MAX, 3).is_err());
        assert!(amf_ffi::plane_extent(4, 3, 6, 2).is_err(), "a plane shorter than the frame");
        assert!(amf_ffi::plane_extent(4, 3, -1, 3).is_err() && amf_ffi::plane_extent(4, 3, 6, -1).is_err());
        let gray = gray_frame(&cfg(Codec::H264)).unwrap();
        assert_eq!(gray.len(), Input::Nv12.frame_bytes(256, 144));
        assert!(gray[..256 * 144].iter().all(|&y| y == 16) && gray[256 * 144..].iter().all(|&c| c == 128), "black-level luma, neutral chroma");
        assert_eq!(amf_ffi::max_packet(16, 16), 64 << 20);
        assert_eq!(amf_ffi::max_packet(7680, 4320), 99_532_800);
    }

    /// An encoder whose answers the test scripts: each submit takes the next answer of `submits`
    /// (then takes the frame), and each poll the next answer of `polls` (then has nothing).
    #[derive(Default)]
    struct Script {
        submits: VecDeque<Result<Submitted, String>>,
        drains: VecDeque<Submitted>,
        polls: VecDeque<Polled>,
        taken: Vec<i64>,
        drained: bool,
    }

    impl Engine for Script {
        fn submit(&mut self, _: &[u8], time: i64, _: i64) -> Result<Submitted, String> {
            let r = self.submits.pop_front().unwrap_or(Ok(Submitted::Taken));
            if r == Ok(Submitted::Taken) {
                self.taken.push(time);
            }
            r
        }
        fn poll(&mut self) -> Result<Polled, String> {
            Ok(self.polls.pop_front().unwrap_or(if self.drained { Polled::Ended } else { Polled::Empty }))
        }
        fn drain(&mut self) -> Result<Submitted, String> {
            let r = self.drains.pop_front().unwrap_or(Submitted::Taken);
            self.drained |= r == Submitted::Taken;
            Ok(r)
        }
    }

    /// An IDR access unit with its SPS and PPS, and a P picture.
    const KEY: [u8; 18] = [0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 2, 0, 0, 0, 1, 0x65, 3];
    const P: [u8; 6] = [0, 0, 0, 1, 0x41, 4];

    fn out(data: &[u8], time: i64) -> Polled {
        Polled::Output(Output { data: data.to_vec(), time: Some(time) })
    }

    fn h264_track() -> Track {
        Track::new(Codec::H264, (30, 1), vec![vec![0x67, 1], vec![0x68, 2]])
    }

    #[test]
    fn a_late_packet_does_not_hold_up_encode_and_comes_out_with_a_later_call() {
        let mut e = Script::default();
        let mut track = h264_track();
        let waits = std::cell::Cell::new(0);
        let mut wait = |_| {
            waits.set(waits.get() + 1);
            true
        };
        // The first frame's packet is not ready when its encode call ends; the record waits to be
        // checked against it.
        let mut unchecked = true;
        let first = encode_on(&mut e, &mut track, &mut unchecked, &[], 0, &mut wait).unwrap();
        assert!(first.is_empty() && unchecked);
        e.polls.push_back(out(&KEY, 0));
        let second = encode_on(&mut e, &mut track, &mut unchecked, &[], 1, &mut wait).unwrap();
        assert_eq!(second.iter().map(|p| (p.pts, p.keyframe)).collect::<Vec<_>>(), [(0, true)]);
        assert!(!unchecked, "the first frame's parameter sets were checked");
        assert_eq!(waits.get(), 0, "nothing waited");
        let frame_time = e.taken[1];
        e.polls.push_back(out(&P, frame_time));
        let rest = flush_on(&mut e, &mut track, &mut unchecked, &mut wait).unwrap();
        assert_eq!(rest.iter().map(|p| (p.pts, p.keyframe)).collect::<Vec<_>>(), [(1, false)]);
    }

    #[test]
    fn a_record_from_another_instance_is_checked_against_the_first_frame() {
        // A first frame without parameter sets cannot confirm the record.
        let mut e = Script::default();
        e.polls.push_back(out(&P, 0));
        let r = encode_on(&mut e, &mut h264_track(), &mut true, &[], 0, &mut |_| true);
        assert!(matches!(&r, Err(Error::Failed(why)) if why.contains("no parameter sets")), "{r:?}");
        // A first frame whose parameter sets differ from the record is an error.
        let mut e = Script::default();
        e.polls.push_back(out(&[0, 0, 0, 1, 0x67, 9, 0, 0, 0, 1, 0x68, 2, 0, 0, 0, 1, 0x65, 3], 0));
        let r = encode_on(&mut e, &mut h264_track(), &mut true, &[], 0, &mut |_| true);
        assert!(matches!(&r, Err(Error::Failed(why)) if why.contains("changed")), "{r:?}");
        // The same frame without parameter sets is fine when the record came from this encoder.
        let mut e = Script::default();
        e.polls.push_back(out(&P, 0));
        assert_eq!(encode_on(&mut e, &mut h264_track(), &mut false, &[], 0, &mut |_| true).unwrap().len(), 1);
        assert!(!carries_header(Codec::Hevc, &[]) && !carries_header(Codec::Av1, &[]));
        assert!(carries_header(Codec::H264, &KEY) && !carries_header(Codec::H264, &P));
    }

    #[test]
    fn a_full_input_queue_waits_for_an_output_and_gives_up_after_the_limit() {
        let mut e = Script::default();
        e.submits.extend([Ok(Submitted::Full), Ok(Submitted::Full)]);
        e.polls.extend([Polled::Empty, out(&KEY, 0)]);
        let mut track = h264_track();
        let mut pauses = 0;
        let packets = encode_on(&mut e, &mut track, &mut false, &[], 0, &mut |pause| {
            pauses += usize::from(pause);
            true
        })
        .unwrap();
        assert_eq!((pauses, packets.len(), e.taken.len()), (1, 1, 1), "one pause, then the output freed the queue");
        // An encoder that never frees its queue ends the call once the wait is over, and the
        // frame it did not take is forgotten.
        let mut stuck = Script::default();
        stuck.submits.extend((0..4).map(|_| Ok(Submitted::Full)));
        let mut budget = 2;
        let r = encode_on(&mut stuck, &mut track, &mut false, &[], 1, &mut |_| {
            budget -= 1;
            budget >= 0
        });
        assert!(matches!(r, Err(Error::Failed(why)) if why.contains("took no frame")));
        assert!(track.pending.is_empty(), "the frame the encoder did not take is not expected back");
        // Outputs that keep coming while the queue stays full count toward the limit too.
        let mut busy = Script::default();
        busy.submits.extend((0..8).map(|_| Ok(Submitted::Full)));
        busy.polls.extend((0..8).flat_map(|t| [out(&P, t), Polled::Empty]));
        let (mut budget, mut got) = (2, Vec::new());
        let r = amf_ffi::encode(&mut busy, &[], 0, 1, &mut got, &mut |_| {
            budget -= 1;
            budget >= 0
        });
        assert!(r.is_err_and(|why| why.contains("took no frame")));
        assert_eq!(got.len(), 3, "the outputs collected before the limit are kept");
        let mut failing = Script::default();
        failing.submits.push_back(Err("SubmitInput failed (AMF_FAIL)".into()));
        assert!(encode_on(&mut failing, &mut track, &mut false, &[], 2, &mut |_| true).is_err());
        assert!(track.pending.is_empty());
    }

    #[test]
    fn a_drain_collects_every_frame_until_the_end_and_a_stalled_one_is_an_error() {
        let mut e = Script::default();
        e.drains.push_back(Submitted::Full);
        // The first output frees the queue; the last one is late once.
        e.polls.extend([out(&KEY, 0), Polled::Empty, Polled::Empty, out(&P, 1)]);
        let mut got = Vec::new();
        let mut pauses = 0;
        amf_ffi::drain(&mut e, &mut got, &mut |pause| {
            pauses += usize::from(pause);
            true
        })
        .unwrap();
        assert_eq!((got.len(), pauses), (2, 1));
        let mut budget = 2;
        let r = amf_ffi::drain(&mut NeverEnds(Script::default()), &mut Vec::new(), &mut |_| {
            budget -= 1;
            budget >= 0
        });
        assert!(r.is_err_and(|why| why.contains("last frames")));
        // Outputs that never reach the end of the stream count toward the limit too.
        let mut endless = NeverEnds(Script::default());
        endless.0.polls.extend((0..8).map(|t| out(&P, t)));
        let (mut budget, mut got) = (2, Vec::new());
        let r = amf_ffi::drain(&mut endless, &mut got, &mut |_| {
            budget -= 1;
            budget >= 0
        });
        assert!(r.is_err_and(|why| why.contains("last frames")));
        assert_eq!(got.len(), 3, "the limit ends the drain while outputs still come");
        let mut early = Script::default();
        early.polls.push_back(Polled::Ended);
        assert!(amf_ffi::encode(&mut early, &[], 0, 1, &mut Vec::new(), &mut |_| true).is_err(), "an end before the drain");
    }

    /// A scripted encoder whose polls never report the end of the stream.
    struct NeverEnds(Script);

    impl Engine for NeverEnds {
        fn submit(&mut self, f: &[u8], t: i64, d: i64) -> Result<Submitted, String> {
            self.0.submit(f, t, d)
        }
        fn poll(&mut self) -> Result<Polled, String> {
            Ok(self.0.polls.pop_front().unwrap_or(Polled::Empty))
        }
        fn drain(&mut self) -> Result<Submitted, String> {
            self.0.drain()
        }
    }

    #[test]
    fn a_system_without_an_amf_encoder_says_so() {
        // Off Windows, and on Windows without an AMD GPU or driver, opening is a `NoDevice` error,
        // never a panic; with an AMD encoder, it opens and writes its record, or says that the
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
        assert_eq!(driver(), Err(Error::NoDevice("AMF runs on Windows only".into())));
    }

    #[test]
    fn an_encoder_round_trips_frames_when_an_amd_encoder_is_present() {
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
