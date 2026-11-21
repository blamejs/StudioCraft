//! The VA-API hardware H.264 and HEVC encoders on 64-bit Linux (Intel's and Mesa's drivers among
//! others). Frames go in as NV12 in system memory and come out as 4-byte length-prefixed samples,
//! with the parameter sets in the `avcC` or `hvcC` record ([`Encoder::codec_private`]). AV1 is
//! [`Error::Unsupported`].
//!
//! - The encoder runs on the first `/dev/dri/renderD*` node, in number order, whose driver has an
//!   encode entry point for the codec: H.264 High, Main or Constrained Baseline, HEVC Main.
//! - Quality mode is constant QP at the level. A driver without constant QP gets a variable
//!   bitrate of a tenth of a bit per pixel per frame instead. Bitrate mode is constant bitrate with
//!   a one-second buffer. A driver without constant bitrate does not open for it.
//! - Every picture is an IDR picture or a P picture that refers to the picture before it, so there
//!   are no B-frames and each packet's decoding time is its presentation time.
//! - HEVC takes widths and heights that are multiples of the driver's smallest coding block
//!   (8 × 8 unless the driver lists another).
//! - A stream whose picture size or picture rate is more than level 6.2 of the codec allows is
//!   refused before VA-API is called.
//! - The driver writes the parameter sets, so [`Encoder::new`] encodes one gray frame first and
//!   takes them from its output; the stream that follows starts again at an IDR picture. A
//!   picture whose parameter sets differ from the record is an error.
//!
//! Up to four frames are in flight: [`Encoder::encode`] copies a frame into a free surface and
//! submits it, and reads the oldest frame's coded data only when every surface is taken. Reading
//! is the one call that waits for the GPU, and it waits for a frame submitted three frames before.
//! With libva 2.9 or later and a driver that has `vaSyncSurface2`, a frame the GPU has not
//! finished after two seconds is an error.

use crate::mf::set_types;
use crate::vaapi_ffi::{self, OpenError, Params, Picture, Session};
use crate::{Codec, annexb, avc, hevc};
pub use crate::{Color, Config, Error, Input, Packet, Rate};

impl From<OpenError> for Error {
    fn from(e: OpenError) -> Error {
        match e {
            OpenError::NoDevice(why) | OpenError::NoEncoder(why) => Error::NoDevice(why),
            OpenError::Unsupported(why) => Error::Unsupported(why),
            OpenError::Busy(why) => Error::Busy(why),
        }
    }
}

/// Whether libva is on this system: 64-bit Linux with `libva.so.2` and `libva-drm.so.2`. The
/// libraries are loaded once per process, so later calls cost nothing.
pub fn driver() -> Result<(), Error> {
    vaapi_ffi::driver().map_err(Error::NoDevice)
}

/// Whether a VA-API driver opens an encoder that writes `codec` for a 256 × 192 stream at 30
/// frames per second. It opens the encoder and closes it again.
pub fn available(codec: Codec) -> Result<(), Error> {
    driver()?;
    // Both sides are multiples of 64, the largest smallest coding block an HEVC driver reports.
    let cfg = Config { codec, input: Input::Nv12, width: 256, height: 192, fps: (30, 1), rate: Rate::Quality(23), keyint: 60, color: Color::Bt709 };
    check(&cfg)?;
    Session::open(&params(&cfg)).map(drop).map_err(Error::from)
}

/// Copy the packed NV12 `frame` of `width` × `height` into `dst`, a surface's mapped image of the
/// coded size `coded` (width, height) whose luma and chroma planes start at `offsets` and step
/// `pitches` bytes a row. The columns and rows past the frame repeat its last column and row.
/// [`Encoder`] copies each frame with it.
pub fn copy_nv12(
    frame: &[u8],
    width: usize,
    height: usize,
    coded: [usize; 2],
    dst: &mut [u8],
    pitches: [usize; 2],
    offsets: [usize; 2],
) -> Result<(), Error> {
    vaapi_ffi::copy_nv12(frame, width, height, coded, dst, pitches, offsets).map_err(Error::Failed)
}

fn params(cfg: &Config) -> Params {
    Params { codec: cfg.codec, width: cfg.width, height: cfg.height, fps: cfg.fps, rate: cfg.rate, gop: cfg.keyint.max(1) }
}

/// The widest and tallest frame this crate sends to a VA-API encoder.
const MAX_SIDE: u32 = 8192;

/// Why an AV1 stream does not open.
const NO_AV1: &str = "this crate's VA-API encoder writes H.264 and HEVC only";

/// Check `cfg` without VA-API: sizes, frame rate, rate and input.
fn check(cfg: &Config) -> Result<(), Error> {
    let bad = |e: String| Err(Error::Unsupported(e));
    if cfg.codec == Codec::Av1 {
        return bad(NO_AV1.into());
    }
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
    if !vaapi_ffi::fits_a_level(cfg.codec, w, h, cfg.fps) {
        let codec = if cfg.codec == Codec::H264 { "H.264" } else { "HEVC" };
        return bad(format!("{w}x{h} at {}/{} frames per second is more than {codec} level 6.2 allows", cfg.fps.0, cfg.fps.1));
    }
    if cfg.input != Input::Nv12 {
        return bad("the VA-API encoders take NV12 frames only".into());
    }
    if cfg.color != Color::Bt709 {
        return bad("the VA-API encoders write BT.709 only".into());
    }
    match cfg.rate {
        Rate::Quality(q) if !(1..=51).contains(&q) => bad(format!("the quality level {q} is outside 1 to 51")),
        Rate::Cbr(0) => bad("a bitrate of 0 kbit/s".into()),
        Rate::Cbr(kbps) if kbps > u32::MAX / 1000 => bad(format!("a bitrate of {kbps} kbit/s")),
        _ => Ok(()),
    }
}

/// The pictures of a stream: an IDR picture every `keyint` pictures and P pictures between.
struct Gop {
    keyint: u32,
    since_idr: u32,
    idrs: u32,
}

impl Gop {
    fn new(keyint: u32) -> Gop {
        Gop { keyint: keyint.max(1), since_idr: 0, idrs: 0 }
    }

    fn next(&mut self) -> Picture {
        let pic = Picture { idr: self.since_idr == 0, order: self.since_idr, idr_id: (self.idrs % 65536) as u16 };
        self.since_idr += 1;
        if self.since_idr >= self.keyint {
            self.restart();
        }
        pic
    }

    /// Make the next picture an IDR picture with a number its neighbor does not have.
    fn restart(&mut self) {
        self.since_idr = 0;
        self.idrs = self.idrs.wrapping_add(1);
    }
}

/// The `avcC` or `hvcC` record made of the parameter sets `sets`.
fn record_of(cfg: &Config, sets: &[Vec<u8>]) -> Result<Vec<u8>, Error> {
    match (cfg.codec, sets) {
        (Codec::H264, [sps, pps]) => avc::avcc(sps, pps),
        (Codec::Hevc, [vps, sps, pps]) => hevc::hvcc(vps, sps, pps, (cfg.width, cfg.height), 8),
        (Codec::Av1, _) => Err(NO_AV1.into()),
        _ => Err("the encoder returned no parameter sets".into()),
    }
    .map_err(Error::Failed)
}

/// The packet of one coded picture, `data` in Annex B, that went in with `pts` as an IDR picture
/// when `idr` is set. Its parameter sets must be `sets` (SPS and PPS, or VPS, SPS and PPS), the
/// ones in the record; the packet goes out without them. [`Encoder`] makes each packet with it.
pub fn packet(codec: Codec, sets: &[Vec<u8>], data: &[u8], pts: i64, idr: bool) -> Result<Packet, Error> {
    if codec == Codec::Av1 {
        return Err(Error::Unsupported(NO_AV1.into()));
    }
    let types = set_types(codec);
    let mut keyframe = false;
    let mut out = Vec::with_capacity(data.len());
    for n in annexb::nals(data) {
        let t = annexb::nal_type(codec, n);
        if let Some(i) = t.and_then(|t| types.iter().position(|&(ty, _)| ty == t))
            && sets.get(i).map(Vec::as_slice) != Some(n)
        {
            let name = types.get(i).map_or("parameter sets", |&(_, name)| name);
            return Err(Error::Failed(format!("the encoder changed its {name}")));
        }
        let (dropped, key) = match (codec, t) {
            (_, None) | (Codec::Av1, _) => (true, false),
            (Codec::H264, Some(t)) => (matches!(t, 7..=9), t == 5),
            (Codec::Hevc, Some(t)) => (matches!(t, 32..=37), (16..=21).contains(&t)),
        };
        keyframe |= key;
        if !dropped {
            let len = u32::try_from(n.len()).map_err(|_| Error::Failed("a NAL unit longer than 4 GiB".into()))?;
            out.extend_from_slice(&len.to_be_bytes());
            out.extend_from_slice(n);
        }
    }
    if out.is_empty() {
        return Err(Error::Failed("the encoder produced no picture data".into()));
    }
    if idr && !keyframe {
        return Err(Error::Failed("the encoder wrote no IDR picture where one was asked for".into()));
    }
    Ok(Packet { data: out, pts, keyframe })
}

/// A VA-API hardware encoder.
pub struct Encoder {
    session: Session,
    cfg: Config,
    record: Vec<u8>,
    sets: Vec<Vec<u8>>,
    gop: Gop,
    /// The coded data of the frame being read, kept between frames so reading allocates nothing.
    coded: Vec<u8>,
}

impl Encoder {
    /// Open an encoder for `cfg`, or say why this system cannot run it.
    pub fn new(cfg: &Config) -> Result<Encoder, Error> {
        check(cfg)?;
        let mut session = Session::open(&params(cfg))?;
        let mut gop = Gop::new(cfg.keyint);
        // The gray frame and the buffer for its coded data are allocated fallibly: an 8192 × 8192
        // stream needs 96 MiB for each.
        let luma = (cfg.width as usize).saturating_mul(cfg.height as usize);
        let size = luma.saturating_add(luma / 2);
        let no_memory = |e| Error::Failed(format!("no memory for a {}x{} frame: {e}", cfg.width, cfg.height));
        let mut gray = Vec::new();
        gray.try_reserve_exact(size).map_err(no_memory)?;
        gray.resize(luma, 16u8);
        gray.resize(size, 128u8);
        session.submit(&gray, &gop.next(), (0, true)).map_err(Error::Failed)?;
        let mut coded = Vec::new();
        coded.try_reserve_exact(size).map_err(no_memory)?;
        if session.collect(&mut coded).map_err(Error::Failed)?.is_none() {
            return Err(Error::Failed("the encoder returned no frame".into()));
        }
        let sets = annexb::parameter_sets(cfg.codec, &coded, set_types(cfg.codec)).map_err(Error::Failed)?;
        let record = record_of(cfg, &sets)?;
        gop.restart();
        Ok(Encoder { session, cfg: *cfg, record, sets, gop, coded })
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
        let mut packets = Vec::new();
        if self.session.full() {
            self.take(&mut packets)?;
        }
        let pic = self.gop.next();
        self.session.submit(frame, &pic, (pts, pic.idr)).map_err(Error::Failed)?;
        Ok(packets)
    }

    /// End the stream: every frame still inside comes out. A frame that cannot be read is an
    /// error; the frames after it stay inside, each with its own timestamp, for the next call.
    pub fn flush(&mut self) -> Result<Vec<Packet>, Error> {
        let mut packets = Vec::with_capacity(vaapi_ffi::DEPTH);
        while self.take(&mut packets)? {}
        Ok(packets)
    }

    /// Read the oldest frame in flight into `packets`; `false` when none is in flight. The
    /// session gives each frame back with the timestamp it went in with, so a frame that fails to
    /// read leaves the later frames' timestamps as they were.
    fn take(&mut self, packets: &mut Vec<Packet>) -> Result<bool, Error> {
        self.coded.clear();
        let Some((pts, idr)) = self.session.collect(&mut self.coded).map_err(Error::Failed)? else { return Ok(false) };
        packets.push(packet(self.cfg.codec, &self.sets, &self.coded, pts, idr)?);
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vaapi_ffi::abi::*;
    use crate::vaapi_ffi::{
        Blocks, Control, Slots, Waited, control, copy_nv12, frame_rate, h264_level, h264_picture, h264_sequence, h264_slice, hevc_blocks, hevc_level,
        hevc_picture, hevc_sequence, hevc_slice, open_error, rate_buffers, waited,
    };

    fn cfg(codec: Codec) -> Config {
        Config { codec, input: Input::Nv12, width: 256, height: 144, fps: (30, 1), rate: Rate::Quality(23), keyint: 60, color: Color::Bt709 }
    }

    fn p(codec: Codec, width: u32, height: u32, fps: (u32, u32)) -> Params {
        Params { codec, width, height, fps, rate: Rate::Quality(23), gop: 120 }
    }

    const SPS: &[u8] = &[0x67, 1];
    const PPS: &[u8] = &[0x68, 2];
    const IDR: &[u8] = &[0x65, 3];
    const P: &[u8] = &[0x41, 4];

    fn annex_b(nals: &[&[u8]]) -> Vec<u8> {
        nals.iter().flat_map(|n| [&[0, 0, 0, 1][..], n].concat()).collect()
    }

    #[test]
    fn configurations_the_encoder_cannot_take_are_refused_before_va_api() {
        let ok = cfg(Codec::Hevc);
        assert_eq!(check(&ok), Ok(()));
        for largest in [(Codec::Hevc, (60, 1)), (Codec::H264, (120, 1))] {
            let c = Config { codec: largest.0, width: 7680, height: 4320, fps: largest.1, ..ok };
            assert_eq!(check(&c), Ok(()), "{c:?} fits level 6.2");
        }
        for bad in [
            Config { width: 255, ..ok },
            Config { height: 0, ..ok },
            Config { width: 8194, ..ok },
            Config { width: 8192, height: 8192, ..ok },
            Config { width: 8192, height: 4352, fps: (121, 1), ..ok },
            Config { width: 7680, height: 4320, fps: (130, 1), ..ok },
            Config { codec: Codec::H264, width: 8192, height: 8192, ..ok },
            Config { codec: Codec::H264, width: 7680, height: 4320, fps: (240, 1), ..ok },
            Config { fps: (30, 0), ..ok },
            Config { fps: (0, 1), ..ok },
            Config { input: Input::P010, ..ok },
            Config { color: Color::Hlg, ..ok },
            Config { color: Color::Pq { mastering: None, light: None }, ..ok },
            Config { rate: Rate::Quality(0), ..ok },
            Config { rate: Rate::Quality(52), ..ok },
            Config { rate: Rate::Cbr(0), ..ok },
            Config { rate: Rate::Cbr(u32::MAX / 1000 + 1), ..ok },
            Config { codec: Codec::Av1, ..ok },
        ] {
            assert!(matches!(check(&bad), Err(Error::Unsupported(_))), "{bad:?}");
            assert!(matches!(Encoder::new(&bad), Err(Error::Unsupported(_))), "{bad:?} reached VA-API");
        }
        assert!(matches!(available(Codec::Av1), Err(Error::NoDevice(_) | Error::Unsupported(_))));
        assert!(matches!(packet(Codec::Av1, &[], &[0x12, 0], 0, true), Err(Error::Unsupported(_))));
    }

    #[test]
    fn an_idr_picture_comes_every_keyint_pictures_and_neighbors_have_other_numbers() {
        let mut g = Gop::new(3);
        let pics: Vec<_> = (0..7).map(|_| g.next()).collect();
        assert_eq!(
            pics.iter().map(|p| (p.idr, p.order)).collect::<Vec<_>>(),
            [(true, 0), (false, 1), (false, 2), (true, 0), (false, 1), (false, 2), (true, 0)]
        );
        assert_eq!([pics[0].idr_id, pics[3].idr_id, pics[6].idr_id], [0, 1, 2]);
        let mut g = Gop::new(60);
        let gray = g.next();
        g.restart();
        let first = g.next();
        assert!(first.idr && first.order == 0 && first.idr_id != gray.idr_id, "the stream starts again at a new IDR picture");
        let mut every = Gop::new(0);
        assert!((0..3).all(|_| every.next().idr), "an interval of 0 or 1 makes every picture an IDR picture");
    }

    #[test]
    fn quality_is_constant_qp_or_a_variable_bitrate_and_bitrate_is_constant() {
        assert_eq!(control(Rate::Quality(23), VA_RC_CQP | VA_RC_CBR, 9), Ok(Control::Cqp(23)));
        assert_eq!(control(Rate::Quality(23), VA_RC_VBR | VA_RC_CBR, 9), Ok(Control::Vbr(9)));
        assert!(control(Rate::Quality(23), VA_RC_CBR, 9).is_err());
        assert_eq!(control(Rate::Cbr(6000), VA_RC_CBR, 9), Ok(Control::Cbr(6_000_000)));
        assert!(control(Rate::Cbr(6000), VA_RC_CQP | VA_RC_VBR, 9).is_err());
        assert!(control(Rate::Cbr(6000), VA_ATTRIB_NOT_SUPPORTED, 9).is_err(), "no rate control attribute");
        assert_eq!(control(Rate::Cbr(u32::MAX / 1000), VA_RC_CBR, 9), Ok(Control::Cbr(u32::MAX / 1000 * 1000)));
        let nominal = vaapi_ffi::nominal_bitrate(&p(Codec::H264, 1920, 1080, (60, 1)));
        assert_eq!(nominal, 12_441_600);
        let fast = vaapi_ffi::nominal_bitrate(&p(Codec::Hevc, 4096, 4096, (240, 1)));
        assert_eq!(fast, 240_000_000, "at most the bitrate HEVC level 6.2 Main tier takes");
        assert_eq!(hevc_level(4096, 4096, (240, 1), fast), 186);
        assert_eq!(vaapi_ffi::nominal_bitrate(&p(Codec::H264, 8192, 4320, (240, 1))), 800_000_000);
    }

    #[test]
    fn levels_take_the_picture_size_rate_and_bitrate() {
        assert_eq!(h264_level(80 * 45, (30, 1), 0, true), 31, "720p30");
        assert_eq!(h264_level(120 * 68, (60, 1), 0, true), 42, "1080p60");
        assert_eq!(h264_level(240 * 135, (60, 1), 0, true), 52, "2160p60");
        assert_eq!(h264_level(120 * 68, (30, 1), 60_000_000, true), 41, "1080p30 at 60 Mbit/s fits 4.1's High profile bitrate");
        assert_eq!(h264_level(120 * 68, (30, 1), 60_000_000, false), 50, "Main profile's 4.1 and 4.2 allow 50 Mbit/s");
        assert_eq!(h264_level(120 * 68, (30, 1), 50_000_000, false), 41);
        assert_eq!(h264_level(u64::MAX, (u32::MAX, 1), u32::MAX, true), 62);
        assert_eq!(hevc_level(1920, 1080, (60, 1), 0), 123);
        assert_eq!(hevc_level(3840, 2160, (60, 1), 0), 153);
        assert_eq!(hevc_level(1280, 720, (30, 1), 0), 93);
        assert_eq!(hevc_level(8192, 8192, (240, 1), u32::MAX), 186);
    }

    #[test]
    fn frame_rates_pack_into_the_framerate_word() {
        assert_eq!(frame_rate((60, 1)), 60);
        assert_eq!(frame_rate((30000, 1001)), 1001 << 16 | 30000);
        assert_eq!(frame_rate((120_000, 2)), 60_000, "reduced to a whole rate");
        assert_eq!(frame_rate((60_000, 1001)), 1001 << 16 | 60_000);
        assert_eq!(frame_rate((120_000, 1001)), 342 << 16 | 40999, "the closest fraction with 16-bit terms");
        assert_eq!(frame_rate((100_001, 3)), 1 << 16 | 33334);
        assert_eq!(frame_rate((1, 100_000)), 65535 << 16 | 1, "a rate below 1/65535 is its smallest fraction");
        assert_eq!(frame_rate((u32::MAX, 1)), 0xffff, "a whole rate wider than 16 bits would read as a fraction");
    }

    #[test]
    fn hevc_block_sizes_come_from_the_driver_or_the_defaults() {
        assert_eq!(hevc_blocks(VA_ATTRIB_NOT_SUPPORTED), Blocks { ctb: 5, min_cb: 3, min_tb: 2, max_tb: 5, depth_inter: 3, depth_intra: 3 });
        // 64 × 64 coding tree blocks, 8 × 8 coding blocks, transforms 4 × 4 to 32 × 32, depth 2.
        let value = 3 | 3 << 6 | 2 << 10 | 2 << 14;
        assert_eq!(hevc_blocks(value), Blocks { ctb: 6, min_cb: 3, min_tb: 2, max_tb: 5, depth_inter: 2, depth_intra: 2 });
    }

    #[test]
    fn h264_parameters_describe_one_slice_of_ip_pictures() {
        let params = p(Codec::H264, 1920, 1080, (60, 1));
        let seq = h264_sequence(&params, VAProfileH264High, Control::Cqp(23));
        assert_eq!((seq.picture_width_in_mbs, seq.picture_height_in_mbs, seq.level_idc), (120, 68, 42));
        let p30 = p(Codec::H264, 1920, 1080, (30, 1));
        assert_eq!(h264_sequence(&p30, VAProfileH264High, Control::Cbr(60_000_000)).level_idc, 41);
        assert_eq!(h264_sequence(&p30, VAProfileH264Main, Control::Cbr(60_000_000)).level_idc, 50, "the level follows the profile");
        assert_eq!((seq.frame_cropping_flag, seq.frame_crop_right_offset, seq.frame_crop_bottom_offset), (1, 0, 4), "1088 rows cropped to 1080");
        assert_eq!((seq.intra_period, seq.intra_idr_period, seq.ip_period, seq.max_num_ref_frames), (120, 120, 1, 1));
        assert_eq!(seq.seq_fields, 1 | 1 << 2 | 1 << 5 | 12 << 6 | 12 << 12);
        assert_eq!(h264_sequence(&p(Codec::H264, 1280, 720, (30, 1)), VAProfileH264High, Control::Cqp(23)).frame_cropping_flag, 0);
        assert_eq!(seq.bits_per_second, 0);
        assert_eq!(h264_sequence(&params, VAProfileH264High, Control::Cbr(6_000_000)).bits_per_second, 6_000_000);

        let idr = Picture { idr: true, order: 0, idr_id: 1 };
        let pic = h264_picture(&idr, VAProfileH264High, Control::Cqp(23), 10, 9, 5);
        assert_eq!((pic.CurrPic.picture_id, pic.coded_buf, pic.frame_num, pic.pic_init_qp), (10, 5, 0, 23));
        assert_eq!(pic.pic_fields, 1 | 1 << 1 | 1 << 3 | 1 << 8 | 1 << 9, "IDR, reference, CABAC, 8x8 transform, deblocking control");
        let main = h264_picture(&idr, VAProfileH264Main, Control::Cqp(23), 10, 9, 5);
        assert_eq!(main.pic_fields, 1 | 1 << 1 | 1 << 3 | 1 << 9, "no 8x8 transform in Main");
        assert!(pic.ReferenceFrames.iter().all(|r| r.picture_id == VA_INVALID_SURFACE));
        let slice = h264_slice(&idr, 8160, 9);
        assert_eq!((slice.slice_type, slice.num_macroblocks, slice.idr_pic_id, slice.pic_order_cnt_lsb), (2, 8160, 1, 0));
        assert!(slice.RefPicList0.iter().all(|r| r.picture_id == VA_INVALID_SURFACE));

        let p5 = Picture { idr: false, order: 5, idr_id: 1 };
        let pic = h264_picture(&p5, VAProfileH264ConstrainedBaseline, Control::Cbr(1000), 11, 10, 6);
        assert_eq!((pic.frame_num, pic.pic_init_qp, pic.CurrPic.TopFieldOrderCnt), (5, 26, 10));
        assert_eq!(pic.pic_fields, 1 << 1 | 1 << 9, "no CABAC or 8x8 transform in Constrained Baseline");
        assert_eq!((pic.ReferenceFrames[0].picture_id, pic.ReferenceFrames[0].frame_idx, pic.ReferenceFrames[0].TopFieldOrderCnt), (10, 4, 8));
        assert_eq!(pic.ReferenceFrames[0].flags, VA_PICTURE_H264_SHORT_TERM_REFERENCE);
        let slice = h264_slice(&p5, 8160, 10);
        assert_eq!((slice.slice_type, slice.pic_order_cnt_lsb, slice.RefPicList0[0].picture_id), (0, 10, 10));
        assert_eq!(slice.RefPicList1[0].picture_id, VA_INVALID_SURFACE);
        let late = h264_slice(&Picture { idr: false, order: 40_000, idr_id: 0 }, 1, 1);
        assert_eq!(late.pic_order_cnt_lsb, 14464, "the order count wraps at 16 bits");
    }

    #[test]
    fn hevc_parameters_describe_one_slice_of_ip_pictures() {
        let params = p(Codec::Hevc, 1920, 1080, (60, 1));
        let b = hevc_blocks(VA_ATTRIB_NOT_SUPPORTED);
        let seq = hevc_sequence(&params, b, Control::Cqp(23));
        assert_eq!((seq.general_profile_idc, seq.general_level_idc, seq.general_tier_flag), (1, 123, 0));
        assert_eq!((seq.pic_width_in_luma_samples, seq.pic_height_in_luma_samples), (1920, 1080));
        assert_eq!((seq.log2_min_luma_coding_block_size_minus3, seq.log2_diff_max_min_luma_coding_block_size), (0, 2));
        assert_eq!((seq.log2_min_transform_block_size_minus2, seq.log2_diff_max_min_transform_block_size), (0, 3));
        assert_eq!(seq.seq_fields, 1 | 1 << 16, "4:2:0, low delay");

        let idr = Picture { idr: true, order: 0, idr_id: 0 };
        let pic = hevc_picture(&idr, Control::Cqp(23), 10, 9, 5);
        assert_eq!((pic.nal_unit_type, pic.pic_init_qp, pic.coded_buf, pic.collocated_ref_pic_index), (19, 23, 5, 0xff));
        assert_eq!(pic.pic_fields, 1 | 1 << 1 | 1 << 4, "IDR, intra, reference");
        assert!(pic.reference_frames.iter().all(|r| r.picture_id == VA_INVALID_SURFACE));
        let slice = hevc_slice(&idr, 510, 9);
        assert_eq!((slice.slice_type, slice.num_ctu_in_slice, slice.max_num_merge_cand), (2, 510, 5));
        assert_eq!(slice.slice_fields & 1, 1, "the one slice is the last one");

        let p7 = Picture { idr: false, order: 7, idr_id: 0 };
        let pic = hevc_picture(&p7, Control::Vbr(1000), 11, 10, 6);
        assert_eq!((pic.nal_unit_type, pic.pic_init_qp, pic.decoded_curr_pic.pic_order_cnt), (1, 26, 7));
        assert_eq!(pic.pic_fields, 2 << 1 | 1 << 4 | 1 << 9, "P, reference, QP changes within the picture");
        assert_eq!((pic.reference_frames[0].picture_id, pic.reference_frames[0].pic_order_cnt), (10, 6));
        let slice = hevc_slice(&p7, 510, 10);
        assert_eq!((slice.slice_type, slice.ref_pic_list0[0].picture_id, slice.ref_pic_list0[1].picture_id), (1, 10, VA_INVALID_SURFACE));
    }

    #[test]
    fn rate_controlled_streams_get_a_one_second_buffer() {
        assert!(rate_buffers(Control::Cqp(23)).is_none());
        let (rc, hrd) = rate_buffers(Control::Cbr(8_000_000)).unwrap();
        assert_eq!(
            (rc.type_, rc.data.bits_per_second, rc.data.target_percentage, rc.data.window_size),
            (VAEncMiscParameterTypeRateControl, 8_000_000, 100, 1000)
        );
        assert_eq!((hrd.type_, hrd.data.buffer_size, hrd.data.initial_buffer_fullness), (VAEncMiscParameterTypeHRD, 8_000_000, 6_000_000));
    }

    #[test]
    fn frames_are_copied_into_pitched_planes() {
        // A 4 × 2 frame in a 6 × 4 coded surface with 8-byte rows; the chroma plane starts at 32.
        let (w, h, coded) = (4, 2, [6, 4]);
        let frame: Vec<u8> = (0..12).collect();
        let mut dst = vec![0xee; 64];
        copy_nv12(&frame, w, h, coded, &mut dst, [8, 8], [0, 32]).unwrap();
        assert_eq!(&dst[0..6], &[0, 1, 2, 3, 3, 3], "the last column repeats");
        assert_eq!(&dst[6..8], &[0xee; 2], "bytes past the coded width are left alone");
        assert_eq!(&dst[8..14], &[4, 5, 6, 7, 7, 7]);
        assert_eq!((&dst[16..22], &dst[24..30]), (&dst[8..14], &dst[8..14]), "the last row repeats");
        assert_eq!(&dst[32..38], &[8, 9, 10, 11, 10, 11], "the last chroma pair repeats");
        assert_eq!(&dst[40..46], &[8, 9, 10, 11, 10, 11]);
        assert_eq!(&dst[46..], &[0xee; 18]);
        let mut exact = vec![0xee; 64];
        copy_nv12(&frame, w, h, [4, 2], &mut exact, [8, 8], [0, 32]).unwrap();
        assert_eq!((&exact[0..4], &exact[4..8], &exact[32..36]), (&[0, 1, 2, 3][..], &[0xee; 4][..], &[8, 9, 10, 11][..]));
        assert!(copy_nv12(&frame, w, h, coded, &mut dst[..45], [8, 8], [0, 32]).is_err(), "a mapped image too small");
        assert!(copy_nv12(&frame, w, h, coded, &mut dst, [5, 8], [0, 32]).is_err(), "rows narrower than the coded width");
        assert!(copy_nv12(&frame, w, h, [2, 4], &mut dst, [8, 8], [0, 32]).is_err(), "a coded size smaller than the frame");
        assert!(copy_nv12(&frame, w, h, [5, 4], &mut dst, [8, 8], [0, 32]).is_err(), "an odd coded width");
        assert!(copy_nv12(&frame[1..], w, h, coded, &mut dst, [8, 8], [0, 32]).is_err(), "a short frame");
        assert!(copy_nv12(&frame, w, h, coded, &mut dst, [8, 8], [usize::MAX - 2, 32]).is_err(), "an offset past the end");
        assert!(copy_nv12(&frame, usize::MAX, usize::MAX, coded, &mut dst, [8, 8], [0, 32]).is_err(), "a size that overflows");
        assert!(copy_nv12(&[], 0, 0, coded, &mut dst, [8, 8], [0, 32]).is_err(), "an empty frame");
        assert!(copy_nv12(&frame[..6], 4, 1, coded, &mut dst, [8, 8], [0, 32]).is_err(), "an odd height");
    }

    #[test]
    fn surfaces_are_whole_macroblocks_for_h264_and_coded_buffers_fit_a_picture() {
        assert_eq!(vaapi_ffi::coded_size(Codec::H264, 1920, 1080), (1920, 1088));
        assert_eq!(vaapi_ffi::coded_size(Codec::Hevc, 1920, 1080), (1920, 1080));
        assert_eq!(vaapi_ffi::coded_bytes(1920, 1088), 1920 * 1088 * 2 + 65536, "more than a raw 4:2:0 picture");
        assert_eq!(vaapi_ffi::coded_bytes(u32::MAX, u32::MAX), u32::MAX);
    }

    #[test]
    fn no_reconstructed_surface_is_reused_while_a_frame_in_flight_uses_it() {
        // While frame n goes in, the frames n - DEPTH + 1 to n - 1 may still be encoding.
        for n in vaapi_ffi::DEPTH..vaapi_ffi::DEPTH + 2 * vaapi_ffi::RECON {
            let (written, read) = vaapi_ffi::recon_slots(n);
            assert_eq!(read, vaapi_ffi::recon_slots(n - 1).0, "frame {n} reads what frame {} wrote", n - 1);
            for earlier in n + 1 - vaapi_ffi::DEPTH..n {
                let (w, r) = vaapi_ffi::recon_slots(earlier);
                assert!(written != w && written != r, "frame {n} writes a surface frame {earlier} uses");
            }
        }
    }

    #[test]
    fn a_frame_that_fails_to_read_leaves_the_later_frames_their_own_timestamps() {
        let mut slots = Slots::new();
        assert_eq!(slots.pop(), None);
        for pts in 0..vaapi_ffi::DEPTH as i64 {
            assert_eq!(slots.push((pts, pts == 0)), Ok(pts as usize));
        }
        assert!(slots.full() && slots.free().is_none());
        assert!(slots.push((9, false)).is_err(), "every slot is taken");
        // The oldest frame leaves its slot before its read, which then fails: the next read gets
        // the next frame's slot and timestamp.
        assert_eq!(slots.pop(), Some((0, (0, true))));
        assert_eq!(slots.free(), Some(0), "the freed slot takes the next frame");
        assert_eq!(slots.push((4, false)), Ok(0));
        let rest: Vec<_> = std::iter::from_fn(|| slots.pop()).collect();
        assert_eq!(rest, [(1, (1, false)), (2, (2, false)), (3, (3, false)), (0, (4, false))]);
        assert_eq!(slots.free(), Some(1), "no frame is in flight");
    }

    #[test]
    fn after_a_failure_on_the_gpu_no_frame_goes_in_and_the_frames_inside_keep_their_tags() {
        let mut slots = Slots::new();
        slots.push((0, true)).unwrap();
        slots.push((1, false)).unwrap();
        assert!(!slots.stalled());
        // A picture failed after vaBeginPicture, or a wait failed: the GPU may still use a surface.
        slots.stall();
        assert!(slots.stalled(), "the session's objects are left in place when it drops");
        assert_eq!(slots.free(), None);
        assert!(slots.push((2, false)).is_err(), "no frame goes in after it");
        assert_eq!(slots.pop(), Some((0, (0, true))));
        assert_eq!(slots.pop(), Some((1, (1, false))));
        assert_eq!(slots.pop(), None);
        assert!(slots.stalled(), "reading every frame out does not clear it");
    }

    #[test]
    fn a_wait_for_the_gpu_times_out_where_the_driver_can() {
        assert_eq!(waited(VA_STATUS_SUCCESS), Waited::Done);
        assert_eq!(waited(VA_STATUS_ERROR_TIMEDOUT), Waited::TimedOut);
        assert_eq!(waited(VA_STATUS_ERROR_UNIMPLEMENTED), Waited::Unbounded, "a driver without vaSyncSurface2");
        assert_eq!(waited(VA_STATUS_ERROR_HW_BUSY), Waited::Failed);
        assert_eq!(vaapi_ffi::SYNC_TIMEOUT_NS, 2_000_000_000);
    }

    #[test]
    fn segments_that_overflowed_or_that_the_driver_marks_bad_are_refused() {
        assert_eq!(vaapi_ffi::segment_error(0), None);
        assert_eq!(vaapi_ffi::segment_error(26), None, "the low byte holds the picture's average QP");
        assert!(vaapi_ffi::segment_error(VA_CODED_BUF_STATUS_SLICE_OVERFLOW_MASK).is_some());
        assert!(vaapi_ffi::segment_error(VA_CODED_BUF_STATUS_BAD_BITSTREAM | 26).is_some());
    }

    #[test]
    fn an_encoder_taken_by_other_programs_is_busy() {
        assert!(matches!(Error::from(open_error(VA_STATUS_ERROR_HW_BUSY, "taken".into(), OpenError::Unsupported)), Error::Busy(_)));
        assert!(matches!(Error::from(open_error(VA_STATUS_ERROR_MAX_NUM_EXCEEDED, "taken".into(), OpenError::Unsupported)), Error::Busy(_)));
        let other = open_error(VA_STATUS_ERROR_RESOLUTION_NOT_SUPPORTED, "too big".into(), OpenError::Unsupported);
        assert_eq!(Error::from(other), Error::Unsupported("too big".into()));
    }

    #[test]
    fn a_busy_render_node_outranks_later_nodes_that_cannot_encode_the_stream() {
        use vaapi_ffi::telling;
        let (none, unsupported, busy) = (|| OpenError::NoDevice("a".into()), || OpenError::Unsupported("b".into()), || OpenError::Busy("c".into()));
        assert_eq!(Error::from(telling(busy(), unsupported())), Error::Busy("c".into()), "a busy node, then a node refusing the size");
        assert_eq!(Error::from(telling(telling(busy(), none()), unsupported())), Error::Busy("c".into()));
        assert_eq!(Error::from(telling(none(), unsupported())), Error::Unsupported("b".into()));
        assert_eq!(Error::from(telling(unsupported(), none())), Error::Unsupported("b".into()));
        assert_eq!(Error::from(telling(unsupported(), busy())), Error::Busy("c".into()));
        assert_eq!(Error::from(telling(none(), OpenError::NoDevice("later".into()))), Error::NoDevice("a".into()), "the first of a kind is kept");
    }

    #[test]
    fn no_encoder_on_any_render_node_stands_and_other_failures_are_tried_again() {
        use vaapi_ffi::no_session;
        let lacks = |n: &str| OpenError::NoEncoder(format!("{n} has no HEVC encoder"));
        let (why, stands) = no_session([lacks("renderD128"), lacks("renderD129")]);
        assert!(stands, "every driver initialized and lists no encoder");
        assert_eq!(Error::from(why), Error::NoDevice("renderD128 has no HEVC encoder".into()));
        let (why, stands) = no_session([lacks("renderD128"), OpenError::NoDevice("renderD129: vaInitialize failed".into())]);
        assert!(!stands, "a node that did not open may open later");
        assert_eq!(Error::from(why), Error::NoDevice("renderD128 has no HEVC encoder".into()));
        assert!(!no_session([lacks("renderD128"), OpenError::Busy("taken".into())]).1);
        assert!(!no_session([OpenError::Unsupported("vaCreateContext".into())]).1, "a driver that failed to start");
        let (why, stands) = no_session([]);
        assert!(!stands && matches!(why, OpenError::NoDevice(_)), "no render node");
    }

    #[test]
    fn packets_leave_the_parameter_sets_in_the_record() {
        let sets = vec![SPS.to_vec(), PPS.to_vec()];
        let key = packet(Codec::H264, &sets, &annex_b(&[&[0x09, 0x10], SPS, PPS, IDR]), 7, true).unwrap();
        assert_eq!((key.pts, key.keyframe), (7, true));
        assert_eq!(key.data, [0, 0, 0, 2, 0x65, 3], "the delimiter and parameter sets are dropped");
        let p = packet(Codec::H264, &sets, &annex_b(&[P]), 8, false).unwrap();
        assert_eq!((p.pts, p.keyframe, p.data.len()), (8, false, 6));
        let changed = packet(Codec::H264, &sets, &annex_b(&[SPS, &[0x68, 9], IDR]), 9, true);
        assert!(matches!(changed, Err(Error::Failed(why)) if why.contains("changed its PPS")));
        let no_key = packet(Codec::H264, &sets, &annex_b(&[P]), 10, true);
        assert!(matches!(no_key, Err(Error::Failed(why)) if why.contains("no IDR picture")));
        assert!(packet(Codec::H264, &sets, &annex_b(&[SPS, PPS]), 11, false).is_err(), "no picture data");
        assert!(packet(Codec::H264, &sets, &[], 12, false).is_err());
        let hevc_sets = vec![vec![0x40, 1, 1], vec![0x42, 1, 2], vec![0x44, 1, 3]];
        let cra = packet(Codec::Hevc, &hevc_sets, &annex_b(&[&[0x40, 1, 1], &[0x26, 1, 5]]), 0, true).unwrap();
        assert_eq!((cra.keyframe, cra.data), (true, vec![0, 0, 0, 3, 0x26, 1, 5]));
    }

    #[test]
    fn a_system_without_a_va_api_encoder_says_so() {
        // Off 64-bit Linux, and on a system without libva or a driver with an encoder, opening is
        // a `NoDevice` or `Unsupported` error, never a panic; with one, it opens and writes its
        // record.
        for codec in [Codec::H264, Codec::Hevc] {
            match Encoder::new(&cfg(codec)) {
                Ok(e) => assert!(!e.codec_private().is_empty()),
                Err(Error::NoDevice(why) | Error::Unsupported(why) | Error::Busy(why)) => assert!(!why.is_empty()),
                Err(other) => panic!("{codec:?}: {other}"),
            }
            match available(codec) {
                Ok(()) => {}
                Err(Error::NoDevice(why) | Error::Unsupported(why) | Error::Busy(why)) => assert!(!why.is_empty()),
                Err(other) => panic!("{codec:?}: {other}"),
            }
        }
        #[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
        assert_eq!(driver(), Err(Error::NoDevice("VA-API runs on 64-bit Linux only".into())));
    }

    #[test]
    fn an_encoder_round_trips_frames_when_a_hardware_encoder_is_present() {
        for codec in [Codec::H264, Codec::Hevc] {
            let Ok(mut e) = Encoder::new(&Config { keyint: 4, ..cfg(codec) }) else { continue };
            let frame = vec![128u8; Input::Nv12.frame_bytes(256, 144)];
            let mut packets = Vec::new();
            for pts in 0..10 {
                packets.extend(e.encode(&frame, pts).unwrap());
            }
            packets.extend(e.flush().unwrap());
            assert_eq!(packets.iter().map(|p| p.pts).collect::<Vec<_>>(), (0..10).collect::<Vec<_>>());
            assert_eq!(packets.iter().map(|p| p.keyframe).collect::<Vec<_>>(), (0..10).map(|i| i % 4 == 0).collect::<Vec<_>>());
            assert!(e.encode(&frame[1..], 10).is_err(), "a short frame");
            assert!(e.encode(&frame, -1).is_err(), "a negative timestamp");
        }
    }
}
