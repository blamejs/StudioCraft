//! The hardware H.264 and HEVC encoders of Apple's VideoToolbox on macOS. Frames go in as NV12 in
//! system memory and come out as 4-byte length-prefixed samples, with the parameter sets in the
//! `avcC` or `hvcC` record ([`Encoder::codec_private`]).
//!
//! - The session requires a hardware encoder, and one that VideoToolbox reports as software is
//!   not used.
//! - Quality mode sets the session's quality to `(100 - 2 × level) / 100`. An encoder without a
//!   quality setting gets an average bitrate of a tenth of a bit per pixel per frame instead.
//!   Bitrate mode is constant bitrate where macOS offers it (macOS 13 and later), and otherwise an
//!   average bitrate with a one-second ceiling at the same rate. An encoder that refuses the
//!   ceiling does not open.
//! - Frame reordering is off, so there are no B-frames and each packet's decoding time is its
//!   presentation time. A frame that comes out before one that went in earlier is an error.
//! - The session says BT.709 at limited range.
//! - The parameter sets come with the first compressed frame, so [`Encoder::new`] encodes one gray
//!   frame in a second session first and takes them from its output. A frame whose parameter sets
//!   differ from the record is an error.

use std::collections::VecDeque;

use crate::mf::set_types;
use crate::vt_ffi::{OpenError, Output, Params, Session};
use crate::{Codec, avc, hevc};
pub use crate::{Config, Error, Input, Packet, Rate};

impl From<OpenError> for Error {
    fn from(e: OpenError) -> Error {
        match e {
            OpenError::NoDevice(why) => Error::NoDevice(why),
            OpenError::Unsupported(why) => Error::Unsupported(why),
            OpenError::Busy(why) => Error::Busy(why),
        }
    }
}

/// Whether VideoToolbox is on this system: macOS only.
pub fn driver() -> Result<(), Error> {
    if cfg!(target_os = "macos") { Ok(()) } else { Err(Error::NoDevice("VideoToolbox runs on macOS only".into())) }
}

/// Whether VideoToolbox opens a hardware encoder that writes `codec` for a 256 × 144 stream at 30
/// frames per second. It opens the session and closes it again.
pub fn available(codec: Codec) -> Result<(), Error> {
    driver()?;
    let cfg = Config { codec, input: Input::Nv12, width: 256, height: 144, fps: (30, 1), rate: Rate::Quality(23), keyint: 60 };
    Session::open(&params(&cfg)).map(drop).map_err(Error::from)
}

fn params(cfg: &Config) -> Params {
    Params { codec: cfg.codec, width: cfg.width, height: cfg.height, fps: cfg.fps, rate: cfg.rate, gop: cfg.keyint.max(1) }
}

/// The widest and tallest frame this crate sends to a VideoToolbox encoder.
const MAX_SIDE: u32 = 8192;

/// Check `cfg` without VideoToolbox: sizes, frame rate, rate and input.
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
    if i32::try_from(cfg.fps.0).is_err() {
        return bad(format!("the frame rate {}/{} has a numerator above {}", cfg.fps.0, cfg.fps.1, i32::MAX));
    }
    if cfg.input != Input::Nv12 {
        return bad("the VideoToolbox encoders take NV12 frames only".into());
    }
    match cfg.rate {
        Rate::Quality(q) if !(1..=51).contains(&q) => bad(format!("the quality level {q} is outside 1 to 51")),
        Rate::Cbr(0) => bad("a bitrate of 0 kbit/s".into()),
        Rate::Cbr(kbps) if kbps > i32::MAX as u32 / 1000 => bad(format!("a bitrate of {kbps} kbit/s")),
        _ => Ok(()),
    }
}

/// The NAL units of a 4-byte length-prefixed sample.
fn units(data: &[u8]) -> Result<Vec<&[u8]>, String> {
    let mut out = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        let (len, tail) = rest.split_first_chunk::<4>().ok_or("a sample ends inside a NAL unit length")?;
        let len = u32::from_be_bytes(*len) as usize;
        let (nal, tail) = tail.split_at_checked(len).ok_or("a NAL unit runs past the end of its sample")?;
        if nal.is_empty() {
            return Err("a sample holds an empty NAL unit".into());
        }
        out.push(nal);
        rest = tail;
    }
    Ok(out)
}

/// The parameter sets a format description lists, one of each type in the order the record takes
/// them, or an error naming the missing ones.
fn sorted_sets(codec: Codec, sets: &[Vec<u8>]) -> Result<Vec<Vec<u8>>, Error> {
    let mut found = Vec::new();
    let mut missing = Vec::new();
    for &(t, name) in set_types(codec) {
        match sets.iter().find(|s| crate::annexb::nal_type(codec, s) == Some(t) && s.len() > 1) {
            Some(s) => found.push(s.clone()),
            None => missing.push(name),
        }
    }
    if missing.is_empty() { Ok(found) } else { Err(Error::Failed(format!("the encoder returned no {}", missing.join(" or ")))) }
}

/// The `avcC` or `hvcC` record made of the parameter sets `sets`.
fn record_of(cfg: &Config, sets: &[Vec<u8>]) -> Result<Vec<u8>, Error> {
    match (cfg.codec, sets) {
        (Codec::H264, [sps, pps]) => avc::avcc(sps, pps),
        (Codec::Hevc, [vps, sps, pps]) => hevc::hvcc(vps, sps, pps, (cfg.width, cfg.height), 8),
        _ => Err("the encoder returned no parameter sets".into()),
    }
    .map_err(Error::Failed)
}

/// The frames of one stream between the session and its packets: the time each frame goes in at,
/// and the timestamp and parameter sets each packet must come out with.
struct Track {
    codec: Codec,
    fps: (u32, u32),
    sets: Vec<Vec<u8>>,
    frames: u64,
    /// Frames sent and not returned, oldest first: their session time and timestamp.
    pending: VecDeque<(i64, i64)>,
}

impl Track {
    fn new(codec: Codec, fps: (u32, u32), sets: Vec<Vec<u8>>) -> Track {
        Track { codec, fps, sets, frames: 0, pending: VecDeque::new() }
    }

    /// The session time of frame `n`, in units of 1 / the frame rate's numerator.
    fn time_of(&self, n: u64) -> i64 {
        i64::try_from(i128::from(n) * i128::from(self.fps.1)).unwrap_or(i64::MAX)
    }

    /// The time and duration the next frame goes in with, remembered with its timestamp `pts`.
    fn send(&mut self, pts: i64) -> (i64, i64) {
        let time = self.time_of(self.frames);
        self.frames = self.frames.saturating_add(1);
        self.pending.push_back((time, pts));
        (time, i64::from(self.fps.1.max(1)))
    }

    /// The packet of one output sample. A sample at a later time than the oldest frame sent means
    /// the encoder dropped the frames before it; a sample at an earlier time is out of order.
    fn packet(&mut self, o: Output) -> Result<Packet, Error> {
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
        if sorted_sets(self.codec, &o.sets).ok().as_ref() != Some(&self.sets) {
            return Err(Error::Failed("the encoder changed its parameter sets".into()));
        }
        let nals = units(&o.data).map_err(Error::Failed)?;
        let types = set_types(self.codec);
        let mut keyframe = false;
        let mut data = Vec::with_capacity(o.data.len());
        for n in nals {
            let t = crate::annexb::nal_type(self.codec, n);
            if let Some(i) = t.and_then(|t| types.iter().position(|&(ty, _)| ty == t))
                && self.sets.get(i).map(Vec::as_slice) != Some(n)
            {
                let name = types.get(i).map_or("parameter sets", |&(_, name)| name);
                return Err(Error::Failed(format!("the encoder changed its {name}")));
            }
            let dropped = match (self.codec, t) {
                (_, None) => true,
                (Codec::H264, Some(t)) => matches!(t, 7..=9),
                (Codec::Hevc, Some(t)) => matches!(t, 32..=37),
            };
            keyframe |= match (self.codec, t) {
                (Codec::H264, Some(t)) => t == 5,
                (Codec::Hevc, Some(t)) => (16..=21).contains(&t),
                (_, None) => false,
            };
            if !dropped {
                data.extend_from_slice(&(n.len() as u32).to_be_bytes());
                data.extend_from_slice(n);
            }
        }
        if data.is_empty() {
            return Err(Error::Failed("the encoder produced no picture data".into()));
        }
        Ok(Packet { data, pts, keyframe })
    }
}

/// A VideoToolbox hardware encoder.
pub struct Encoder {
    session: Session,
    cfg: Config,
    record: Vec<u8>,
    track: Track,
}

impl Encoder {
    /// Open an encoder for `cfg`, or say why this system cannot run it.
    pub fn new(cfg: &Config) -> Result<Encoder, Error> {
        check(cfg)?;
        let p = params(cfg);
        let sets = first_frame(cfg, &p)?;
        let record = record_of(cfg, &sets)?;
        let session = Session::open(&p)?;
        Ok(Encoder { session, cfg: *cfg, record, track: Track::new(cfg.codec, cfg.fps, sets) })
    }

    /// The `avcC` (H.264) or `hvcC` (HEVC) record a container stores with the track.
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
        let sent = self.session.encode(frame, time, duration, &mut outputs);
        if sent.is_err() {
            self.track.pending.pop_back();
        }
        let packets = outputs.into_iter().map(|o| self.track.packet(o)).collect::<Result<Vec<_>, _>>()?;
        sent.map_err(Error::Failed)?;
        Ok(packets)
    }

    /// End the stream: every frame still inside comes out.
    pub fn flush(&mut self) -> Result<Vec<Packet>, Error> {
        let mut outputs = Vec::new();
        self.session.drain(&mut outputs).map_err(Error::Failed)?;
        outputs.into_iter().map(|o| self.track.packet(o)).collect()
    }
}

/// The parameter sets of one gray frame encoded in a separate session for `cfg`, in the order the
/// record takes them.
fn first_frame(cfg: &Config, p: &Params) -> Result<Vec<Vec<u8>>, Error> {
    let mut s = Session::open(p)?;
    let (w, h) = (cfg.width as usize, cfg.height as usize);
    let gray = [vec![16u8; w * h], vec![128u8; w * h / 2]].concat();
    let (time, duration) = Track::new(cfg.codec, cfg.fps, Vec::new()).send(0);
    let mut out = Vec::new();
    s.encode(&gray, time, duration, &mut out).map_err(Error::Failed)?;
    s.drain(&mut out).map_err(Error::Failed)?;
    let o = out.first().ok_or_else(|| Error::Failed("the encoder returned no frame".into()))?;
    sorted_sets(cfg.codec, &o.sets)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vt_ffi;

    fn cfg(codec: Codec) -> Config {
        Config { codec, input: Input::Nv12, width: 256, height: 144, fps: (30, 1), rate: Rate::Quality(23), keyint: 60 }
    }

    /// A length-prefixed sample of the NAL units `nals`.
    fn sample(nals: &[&[u8]]) -> Vec<u8> {
        nals.iter().flat_map(|n| [&(n.len() as u32).to_be_bytes()[..], *n].concat()).collect()
    }

    const SPS: &[u8] = &[0x67, 1];
    const PPS: &[u8] = &[0x68, 2];
    const IDR: &[u8] = &[0x65, 3];
    const P: &[u8] = &[0x41, 4];

    fn sets() -> Vec<Vec<u8>> {
        vec![SPS.to_vec(), PPS.to_vec()]
    }

    fn track() -> Track {
        Track::new(Codec::H264, (30000, 1001), sets())
    }

    fn output(nals: &[&[u8]], time: Option<i64>) -> Output {
        Output { data: sample(nals), time, sets: sets() }
    }

    #[test]
    fn configurations_the_encoder_cannot_take_are_refused_before_videotoolbox() {
        let ok = cfg(Codec::Hevc);
        assert_eq!(check(&ok), Ok(()));
        for bad in [
            Config { width: 255, ..ok },
            Config { height: 0, ..ok },
            Config { width: 8194, ..ok },
            Config { fps: (30, 0), ..ok },
            Config { fps: (0, 1), ..ok },
            Config { fps: (i32::MAX as u32 + 1, 1), ..ok },
            Config { input: Input::P010, ..ok },
            Config { rate: Rate::Quality(0), ..ok },
            Config { rate: Rate::Quality(52), ..ok },
            Config { rate: Rate::Cbr(0), ..ok },
            Config { rate: Rate::Cbr(i32::MAX as u32 / 1000 + 1), ..ok },
        ] {
            assert!(matches!(check(&bad), Err(Error::Unsupported(_))), "{bad:?}");
            assert!(matches!(Encoder::new(&bad), Err(Error::Unsupported(_))), "{bad:?} reached VideoToolbox");
        }
    }

    #[test]
    fn levels_map_to_the_sessions_quality_and_rates_fit_its_numbers() {
        assert_eq!([1, 23, 50, 51].map(vt_ffi::quality), [0.98, 0.54, 0.0, 0.0]);
        assert_eq!([1, 6000, u32::MAX].map(vt_ffi::bitrate_bps), [1000, 6_000_000, i32::MAX]);
        let p = params(&Config { width: 1920, height: 1080, fps: (60, 1), ..cfg(Codec::H264) });
        assert_eq!(vt_ffi::nominal_bitrate(&p), 12_441_600);
        assert_eq!(vt_ffi::nominal_bitrate(&Params { width: u32::MAX, height: u32::MAX, fps: (u32::MAX, 1), ..p }), i32::MAX);
        assert_eq!(vt_ffi::nominal_bitrate(&Params { width: 2, height: 2, ..p }), 24);
    }

    #[test]
    fn sample_times_are_read_in_the_sessions_timescale() {
        assert_eq!(vt_ffi::rescale(1001, 30000, 30000), Some(1001));
        assert_eq!(vt_ffi::rescale(1, 1000, 30000), Some(30));
        assert_eq!(vt_ffi::rescale(1, 3, 2), Some(1), "two thirds rounds up");
        assert_eq!(vt_ffi::rescale(-1, 3, 2), Some(-1));
        assert_eq!(vt_ffi::rescale(1, 0, 30), None);
        assert_eq!(vt_ffi::rescale(i64::MAX, 1, i32::MAX), None);
    }

    #[test]
    fn an_encoder_taken_by_other_sessions_is_busy() {
        let busy = vt_ffi::open_error(vt_ffi::ENCODER_NOT_AVAILABLE_NOW, "taken".into(), OpenError::NoDevice);
        assert_eq!(Error::from(busy), Error::Busy("taken".into()));
        let prepare = vt_ffi::open_error(vt_ffi::ENCODER_NOT_AVAILABLE_NOW, "taken".into(), OpenError::Unsupported);
        assert_eq!(Error::from(prepare), Error::Busy("taken".into()));
        assert_eq!(Error::from(vt_ffi::open_error(-12902, "bad".into(), OpenError::NoDevice)), Error::NoDevice("bad".into()));
        assert_eq!(Error::from(vt_ffi::open_error(-12902, "bad".into(), OpenError::Unsupported)), Error::Unsupported("bad".into()));
    }

    #[test]
    fn frames_go_in_at_whole_frame_times() {
        let mut t = track();
        let sent: Vec<_> = (0..3).map(|pts| t.send(pts * 1001)).collect();
        assert_eq!(sent, [(0, 1001), (1001, 1001), (2002, 1001)]);
        assert_eq!(Track::new(Codec::Hevc, (u32::MAX, u32::MAX), Vec::new()).time_of(u64::MAX), i64::MAX);
    }

    #[test]
    fn packets_take_the_timestamps_their_frames_went_in_with() {
        let mut t = track();
        let times: Vec<_> = (0..3).map(|pts| t.send(100 + pts).0).collect();
        let key = t.packet(output(&[SPS, PPS, IDR], Some(times[0]))).unwrap();
        assert_eq!((key.pts, key.keyframe), (100, true));
        assert_eq!(key.data, [0, 0, 0, 2, 0x65, 3], "the parameter sets are in the record, not the packet");
        assert_eq!(t.packet(output(&[P], None)).unwrap().pts, 101);
        t.send(103);
        let p = t.packet(output(&[P], Some(t.time_of(3)))).unwrap();
        assert_eq!((p.pts, p.keyframe), (103, false), "a later sample skips a dropped frame");
        assert!(t.pending.is_empty());
        assert!(t.packet(output(&[P], None)).is_err(), "more frames than went in");
    }

    #[test]
    fn a_frame_out_of_order_or_with_other_parameter_sets_is_an_error() {
        let mut t = track();
        let (first, _) = t.send(0);
        t.send(1);
        assert!(matches!(t.packet(output(&[P], Some(first - 1))), Err(Error::Failed(why)) if why.contains("out of order")));
        let mut t = track();
        t.send(0);
        let other = Output { sets: vec![vec![0x67, 9], PPS.to_vec()], ..output(&[IDR], Some(0)) };
        assert!(matches!(t.packet(other), Err(Error::Failed(why)) if why.contains("parameter sets")));
        t.send(1);
        let in_band = t.packet(output(&[&[0x68, 7], P], None));
        assert!(matches!(in_band, Err(Error::Failed(why)) if why.contains("changed its PPS")));
    }

    #[test]
    fn hostile_samples_are_errors() {
        for data in [&[0u8, 0, 0][..], &[0, 0, 0, 5, 0x65], &[0, 0, 0, 0], &[0, 0, 0, 1, 0x09]] {
            let mut t = track();
            t.send(0);
            assert!(t.packet(Output { data: data.to_vec(), time: None, sets: sets() }).is_err(), "{data:?}");
        }
        assert_eq!(units(&sample(&[SPS, IDR])).unwrap(), vec![SPS, IDR]);
        assert!(units(&[]).unwrap().is_empty());
    }

    #[test]
    fn parameter_sets_are_taken_one_of_each_type_in_record_order() {
        let hevc = [vec![0x44, 1, 4], vec![0x40, 1, 1], vec![0x42, 1, 3], vec![0x42, 1, 9]];
        assert_eq!(sorted_sets(Codec::Hevc, &hevc).unwrap(), vec![vec![0x40, 1, 1], vec![0x42, 1, 3], vec![0x44, 1, 4]]);
        assert!(matches!(sorted_sets(Codec::H264, &[SPS.to_vec()]), Err(Error::Failed(why)) if why.contains("PPS")));
        assert!(sorted_sets(Codec::Hevc, &[]).is_err());
    }

    #[test]
    fn a_system_without_a_videotoolbox_encoder_says_so() {
        // Off macOS (and on a Mac without a hardware encoder) opening is a `NoDevice` error, never a
        // panic; with one, it opens and writes its record, or says it does not take the stream.
        for codec in [Codec::H264, Codec::Hevc] {
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
        #[cfg(not(target_os = "macos"))]
        assert_eq!(driver(), Err(Error::NoDevice("VideoToolbox runs on macOS only".into())));
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
