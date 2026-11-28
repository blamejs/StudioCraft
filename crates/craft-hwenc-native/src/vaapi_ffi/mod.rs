//! Every call into VA-API: one encode session on a DRM render node ([`session`], 64-bit Linux).
//! The parameter buffers the session sends are built here by safe functions that compile and are
//! tested on every platform. On other systems opening an encoder is an error that says VA-API runs
//! on 64-bit Linux only.

#![cfg_attr(not(all(target_os = "linux", target_pointer_width = "64")), allow(dead_code))]

pub(crate) mod abi;
#[cfg(test)]
mod abi_tests;
pub(crate) mod av1;
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
mod session;

#[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
pub(crate) use none::{Session, driver};
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
pub(crate) use session::{Session, driver};

use abi::*;

pub(crate) use crate::vaapi::PrimeFrame;
use crate::{Codec, Rate};

/// The `/dev/dri/renderD*` nodes in number order, each with its device number (`st_rdev`); none
/// without `/dev/dri`, and an error when it cannot be read.
pub(crate) fn render_nodes() -> Result<Vec<(String, u64)>, String> {
    let dir = match std::fs::read_dir("/dev/dri") {
        Ok(dir) => dir,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("/dev/dri: {e}")),
    };
    let mut nodes: Vec<(u32, String, u64)> = dir
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let n = name.strip_prefix("renderD")?.parse().ok()?;
            let path = format!("/dev/dri/{name}");
            Some((n, path.clone(), device_number(&path)))
        })
        .collect();
    nodes.sort();
    Ok(nodes.into_iter().map(|(_, path, dev)| (path, dev)).collect())
}

/// The device number of the device file at `path`; 0 where it cannot be read or the system has
/// none.
fn device_number(path: &str) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path).map_or(0, |m| m.rdev())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        0
    }
}

/// Frames a session holds between going in and their coded data being read: each has its own
/// input surface and coded buffer.
pub(crate) const DEPTH: usize = 4;
/// Reconstructed pictures, one more than [`DEPTH`]. Frame `n` writes surface `n % RECON` and reads
/// `(n - 1) % RECON`; the frames still in flight, `n - 3` to `n - 1`, write and read only surfaces
/// `n - 4` to `n - 1`, so no surface is written while an earlier frame may still use it.
pub(crate) const RECON: usize = DEPTH + 1;

/// The reconstructed surfaces frame `n` writes and reads, as indices among the [`RECON`] ones.
pub(crate) fn recon_slots(n: usize) -> (usize, usize) {
    (n % RECON, (n + RECON - 1) % RECON)
}

/// What a frame in flight carries out with its coded data: its timestamp and whether it went in
/// as an IDR picture.
pub(crate) type Tag = (i64, bool);

/// The [`DEPTH`] frame slots of a session, oldest frame first, each with the [`Tag`] its frame
/// went in with. A frame leaves its slot with its tag before its coded data is read, so a read
/// that fails drops that frame alone and every later frame keeps its own tag.
#[derive(Debug)]
pub(crate) struct Slots {
    tags: [Tag; DEPTH],
    head: usize,
    count: usize,
    stalled: bool,
}

impl Slots {
    pub(crate) fn new() -> Slots {
        Slots { tags: [(0, false); DEPTH], head: 0, count: 0, stalled: false }
    }

    /// The GPU may still be using a surface after a failure (a wait that failed or timed out, or
    /// a picture that failed after `vaBeginPicture`): no frame goes in from now on, and the
    /// session's objects are not destroyed.
    pub(crate) fn stall(&mut self) {
        self.stalled = true;
    }

    pub(crate) fn stalled(&self) -> bool {
        self.stalled
    }

    /// Whether every slot holds a frame.
    pub(crate) fn full(&self) -> bool {
        self.count >= DEPTH
    }

    /// How many frames are in the slots.
    pub(crate) fn len(&self) -> usize {
        self.count
    }

    /// The slot the next frame goes into, if one is free and the slots are not stalled.
    pub(crate) fn free(&self) -> Option<usize> {
        (!self.full() && !self.stalled).then_some((self.head + self.count) % DEPTH)
    }

    /// Put a frame tagged `tag` into the free slot [`Slots::free`] gave.
    pub(crate) fn push(&mut self, tag: Tag) -> Result<usize, String> {
        if self.stalled {
            return Err("an earlier frame failed on the GPU".into());
        }
        let slot = self.free().ok_or("every frame slot of the encoder is in use")?;
        self.tags[slot] = tag;
        self.count += 1;
        Ok(slot)
    }

    /// The slot of the oldest frame, left in place.
    #[cfg_attr(not(all(target_os = "linux", target_pointer_width = "64")), allow(dead_code))]
    pub(crate) fn oldest(&self) -> Option<usize> {
        (self.count > 0).then_some(self.head)
    }

    /// Take the oldest frame out: its slot and tag.
    pub(crate) fn pop(&mut self) -> Option<(usize, Tag)> {
        if self.count == 0 {
            return None;
        }
        let slot = self.head;
        self.head = (self.head + 1) % DEPTH;
        self.count -= 1;
        Some((slot, self.tags[slot]))
    }
}

/// How long reading a frame waits for the GPU (`vaSyncSurface2`) before it is an error.
pub(crate) const SYNC_TIMEOUT_NS: u64 = 2_000_000_000;

/// What a `vaSyncSurface2` status says about the wait for a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Waited {
    /// The frame is coded.
    Done,
    /// The GPU did not finish it within [`SYNC_TIMEOUT_NS`].
    TimedOut,
    /// The driver has no `vaSyncSurface2`; `vaSyncSurface` waits instead, without a limit.
    Unbounded,
    /// Any other error.
    Failed,
}

pub(crate) fn waited(status: VAStatus) -> Waited {
    match status {
        VA_STATUS_SUCCESS => Waited::Done,
        VA_STATUS_ERROR_TIMEDOUT => Waited::TimedOut,
        VA_STATUS_ERROR_UNIMPLEMENTED => Waited::Unbounded,
        _ => Waited::Failed,
    }
}

/// What one session encodes.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Params {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub fps: (u32, u32),
    pub rate: Rate,
    /// Frames from one IDR picture to the next.
    pub gop: u32,
}

/// Why a session did not open.
#[derive(Debug)]
pub(crate) enum OpenError {
    NoDevice(String),
    /// The render node's driver initialized and lists no encode entry point for the codec.
    NoEncoder(String),
    Unsupported(String),
    Busy(String),
}

/// Of two render nodes' errors, the one that says more: a busy encoder, which may be free at the
/// next try, ahead of a driver that encodes the codec but not this stream, ahead of no encoder.
/// The earlier error is kept between two of the same kind.
pub(crate) fn telling(kept: OpenError, new: OpenError) -> OpenError {
    let rank = |e: &OpenError| match e {
        OpenError::NoDevice(_) | OpenError::NoEncoder(_) => 0,
        OpenError::Unsupported(_) => 1,
        OpenError::Busy(_) => 2,
    };
    if rank(&new) > rank(&kept) { new } else { kept }
}

/// The error of a session that opened on none of the render nodes, from each node's error, and
/// whether it stands until the process ends: there was a node, and every node's driver
/// initialized and lists no encoder for the codec. A node that failed to open, was busy or
/// refused the stream is tried again at the next open.
pub(crate) fn no_session(errors: impl IntoIterator<Item = OpenError>) -> (OpenError, bool) {
    let (mut why, mut stands) = (None::<OpenError>, true);
    for e in errors {
        stands &= matches!(e, OpenError::NoEncoder(_));
        why = Some(match why {
            Some(kept) => telling(kept, e),
            None => e,
        });
    }
    match why {
        Some(why) => (why, stands),
        None => (OpenError::NoDevice("no DRM render node (/dev/dri/renderD*)".into()), false),
    }
}

/// The status of an open call that failed: every encoder of the GPU is taken (busy), or `other`.
pub(crate) fn open_error(status: VAStatus, why: String, other: fn(String) -> OpenError) -> OpenError {
    match status {
        VA_STATUS_ERROR_HW_BUSY | VA_STATUS_ERROR_MAX_NUM_EXCEEDED => OpenError::Busy(why),
        _ => other(why),
    }
}

/// How the session's rate control runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Control {
    /// Constant QP at this value.
    Cqp(u8),
    /// Constant bitrate in bit/s with a one-second buffer.
    Cbr(u32),
    /// Variable bitrate in bit/s with a one-second buffer.
    Vbr(u32),
}

/// The rate control for `rate` among the driver's `modes` (`VAConfigAttribRateControl`): quality
/// is constant QP at the level, or a variable `nominal` bitrate when the driver has no constant
/// QP; bitrate mode is constant bitrate.
pub(crate) fn control(rate: Rate, modes: u32, nominal: u32) -> Result<Control, String> {
    let modes = if modes == VA_ATTRIB_NOT_SUPPORTED { 0 } else { modes };
    match rate {
        Rate::Quality(q) if modes & VA_RC_CQP != 0 => Ok(Control::Cqp(u8::try_from(q.clamp(1, 51)).unwrap_or(51))),
        Rate::Quality(_) if modes & VA_RC_VBR != 0 => Ok(Control::Vbr(nominal)),
        Rate::Quality(_) => Err("the driver offers neither constant QP nor variable bitrate".into()),
        Rate::Cbr(kbps) if modes & VA_RC_CBR != 0 => Ok(Control::Cbr(bitrate_bps(kbps))),
        Rate::Cbr(_) => Err("the driver offers no constant bitrate".into()),
    }
}

impl Control {
    /// The `VAConfigAttribRateControl` value of this mode.
    pub(crate) fn mode(self) -> u32 {
        match self {
            Control::Cqp(_) => VA_RC_CQP,
            Control::Cbr(_) => VA_RC_CBR,
            Control::Vbr(_) => VA_RC_VBR,
        }
    }

    /// The bitrate the sequence parameters carry: 0 for constant QP.
    fn bits_per_second(self) -> u32 {
        match self {
            Control::Cqp(_) => 0,
            Control::Cbr(bps) | Control::Vbr(bps) => bps,
        }
    }

    /// The QP each picture starts at: the constant QP, else 26, the midpoint the standards use.
    fn initial_qp(self) -> u8 {
        match self {
            Control::Cqp(qp) => qp,
            _ => 26,
        }
    }
}

/// `kbps` in bit/s, at most `u32::MAX`.
pub(crate) fn bitrate_bps(kbps: u32) -> u32 {
    u32::try_from(u64::from(kbps) * 1000).unwrap_or(u32::MAX)
}

/// A tenth of a bit per pixel per frame: the variable bitrate quality mode runs at when the driver
/// has no constant QP. It is at most the bitrate of the highest level every profile the encoder
/// writes takes: 800 Mbit/s for H.264 level 6.2 (Table A-1's MaxBR), 240 Mbit/s for HEVC level 6.2
/// Main tier (Table A.9) and 160 Mbit/s for AV1 level 6.3 Main tier (Annex A.3).
pub(crate) fn nominal_bitrate(p: &Params) -> u32 {
    let pixels = u64::from(p.width) * u64::from(p.height);
    let bps = pixels.saturating_mul(u64::from(p.fps.0)) / u64::from(p.fps.1.max(1)) / 10;
    let most: u64 = match p.codec {
        Codec::H264 => 800_000_000,
        Codec::Hevc => 240_000_000,
        Codec::Av1 => u64::from(av1::MAX_MAIN_BPS),
    };
    u32::try_from(bps.min(most)).unwrap_or(u32::MAX).max(1)
}

/// The `VAEncMiscParameterFrameRate::framerate` word of `fps`: the numerator in the low 16 bits and
/// the denominator in the high 16 bits. A whole rate is the rate itself. A fraction whose reduced
/// terms are wider than 16 bits is the closest fraction whose terms fit (`120000/1001` becomes
/// `40999/342`, less than 0.1 ppm slower). A whole rate above 65535 is 65535, since a word with
/// high bits set is read as a fraction.
pub(crate) fn frame_rate(fps: (u32, u32)) -> u32 {
    let (num, den) = (u64::from(fps.0), u64::from(fps.1.max(1)));
    let g = gcd(num, den).max(1);
    let (num, den) = (num / g, den / g);
    if den == 1 {
        return num.clamp(1, 0xffff) as u32;
    }
    let (n, d) = if num <= 0xffff && den <= 0xffff { (num, den) } else { closest_fraction(num, den, 0xffff) };
    ((d as u32) << 16) | (n.max(1) as u32)
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// The fraction closest to `num / den` whose terms are at most `limit`, from the convergents and
/// semiconvergents of its continued fraction.
fn closest_fraction(num: u64, den: u64, limit: u64) -> (u64, u64) {
    let (mut p0, mut q0, mut p1, mut q1) = (0u64, 1u64, 1u64, 0u64);
    let (mut n, mut d) = (num, den);
    while d != 0 {
        let a = n / d;
        let (p2, q2) = (a.saturating_mul(p1).saturating_add(p0), a.saturating_mul(q1).saturating_add(q0));
        if p2 > limit || q2 > limit {
            if q1 == 0 {
                // The whole part alone is wider than `limit`.
                return (limit, 1);
            }
            // The largest semiconvergent that fits, if it is closer than the last convergent.
            let k = (limit - p0).checked_div(p1).unwrap_or(a).min((limit - q0) / q1).min(a);
            let (ps, qs) = (k * p1 + p0, k * q1 + q0);
            // |p/q - num/den| scaled by den: |p·den - num·q| / q.
            let off = |p: u64, q: u64| (u128::from(p) * u128::from(den)).abs_diff(u128::from(num) * u128::from(q));
            let closer = ps > 0 && qs > 0 && off(ps, qs) * u128::from(q1) < off(p1, q1) * u128::from(qs);
            return if closer { (ps, qs) } else { (p1, q1) };
        }
        (p0, q0, p1, q1) = (p1, q1, p2, q2);
        (n, d) = (d, n % d);
    }
    (p1, q1)
}

/// The H.264 `level_idc` for a stream of `mbs` macroblocks per picture at `fps` and `bps`: the
/// lowest level of Rec. ITU-T H.264 Table A-1 whose frame size, macroblock rate and bitrate take
/// it, else 6.2. The bitrate limit is 1.25 × MaxBR for High profile (`high`) and MaxBR for Main
/// and Constrained Baseline (Table A-2's cpbBrVclFactor).
pub(crate) fn h264_level(mbs: u64, fps: (u32, u32), bps: u32, high: bool) -> u8 {
    let factor = if high { 1250 } else { 1000 };
    // (level_idc, MaxMBPS, MaxFS, MaxBR in 1000 bit/s)
    const LEVELS: [(u8, u64, u64, u64); 12] = [
        (30, 40_500, 1_620, 10_000),
        (31, 108_000, 3_600, 14_000),
        (32, 216_000, 5_120, 20_000),
        (40, 245_760, 8_192, 20_000),
        (41, 245_760, 8_192, 50_000),
        (42, 522_240, 8_704, 50_000),
        (50, 589_824, 22_080, 135_000),
        (51, 983_040, 36_864, 240_000),
        (52, 2_073_600, 36_864, 240_000),
        (60, 4_177_920, 139_264, 240_000),
        (61, 8_355_840, 139_264, 480_000),
        (62, 16_711_680, 139_264, 800_000),
    ];
    let rate = mbs.saturating_mul(u64::from(fps.0)).div_ceil(u64::from(fps.1.max(1)));
    LEVELS.iter().find(|&&(_, mbps, fs, br)| mbs <= fs && rate <= mbps && u64::from(bps) <= br * factor).map_or(62, |&(level, ..)| level)
}

/// The HEVC `general_level_idc` (30 × the level) for a `width` × `height` stream at `fps` and
/// `bps`: the lowest Main tier level of Rec. ITU-T H.265 Tables A.8 and A.9 whose picture size,
/// sample rate and bitrate take it, else 6.2.
pub(crate) fn hevc_level(width: u32, height: u32, fps: (u32, u32), bps: u32) -> u8 {
    // (general_level_idc, MaxLumaPs, MaxLumaSr, MaxBR in 1000 bit/s)
    const LEVELS: [(u8, u64, u64, u64); 9] = [
        (93, 983_040, 33_177_600, 10_000),
        (120, 2_228_224, 66_846_720, 12_000),
        (123, 2_228_224, 133_693_440, 20_000),
        (150, 8_912_896, 267_386_880, 25_000),
        (153, 8_912_896, 534_773_760, 40_000),
        (156, 8_912_896, 1_069_547_520, 60_000),
        (180, 35_651_584, 1_069_547_520, 60_000),
        (183, 35_651_584, 2_139_095_040, 120_000),
        (186, 35_651_584, 4_278_190_080, 240_000),
    ];
    let ps = u64::from(width) * u64::from(height);
    let sr = ps.saturating_mul(u64::from(fps.0)).div_ceil(u64::from(fps.1.max(1)));
    LEVELS.iter().find(|&&(_, mps, msr, br)| ps <= mps && sr <= msr && u64::from(bps) <= br * 1000).map_or(186, |&(level, ..)| level)
}

/// Whether a `width` × `height` stream at `fps` fits the highest level the encoder writes: Rec.
/// ITU-T H.264 Table A-1's MaxFS and MaxMBPS at level 6.2 (in macroblocks of the coded size), Rec.
/// ITU-T H.265 Table A.8's MaxLumaPs and MaxLumaSr at level 6.2, or AV1 Annex A.3's MaxPicSize,
/// MaxHSize, MaxVSize and MaxDisplayRate at level 6.3.
pub(crate) fn fits_a_level(codec: Codec, width: u32, height: u32, fps: (u32, u32)) -> bool {
    let per_second = |n: u64| n.saturating_mul(u64::from(fps.0)).div_ceil(u64::from(fps.1.max(1)));
    match codec {
        Codec::H264 => {
            let (cw, ch) = coded_size(Codec::H264, width, height);
            let mbs = u64::from(cw / 16) * u64::from(ch / 16);
            mbs <= 139_264 && per_second(mbs) <= 16_711_680
        }
        Codec::Hevc => {
            let samples = u64::from(width) * u64::from(height);
            samples <= 35_651_584 && per_second(samples) <= 4_278_190_080
        }
        Codec::Av1 => av1::fits_a_level(width, height, fps),
    }
}

/// The HEVC block sizes a session uses, as log2 of the size in luma samples.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Blocks {
    pub ctb: u8,
    pub min_cb: u8,
    pub min_tb: u8,
    pub max_tb: u8,
    pub depth_inter: u8,
    pub depth_intra: u8,
}

/// The block sizes of the driver's `VAConfigAttribEncHEVCBlockSizes` value: its largest coding
/// tree block, its smallest coding block and its transform sizes and depths. A driver without the
/// attribute gets 32 × 32 coding tree blocks, 8 × 8 coding blocks and transforms from 4 × 4 to
/// 32 × 32, three levels deep.
pub(crate) fn hevc_blocks(value: u32) -> Blocks {
    if value == VA_ATTRIB_NOT_SUPPORTED {
        return Blocks { ctb: 5, min_cb: 3, min_tb: 2, max_tb: 5, depth_inter: 3, depth_intra: 3 };
    }
    let field = |shift: u32| (value >> shift & 3) as u8;
    let ctb = field(0) + 3;
    let min_cb = (field(4) + 3).min(ctb);
    let max_tb = (field(6) + 2).min(ctb).min(5);
    let min_tb = (field(8) + 2).min(max_tb);
    Blocks { ctb, min_cb, min_tb, max_tb, depth_inter: field(10), depth_intra: field(14) }
}

/// One picture of the stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Picture {
    /// An IDR picture: no reference, and the decoder can start here.
    pub idr: bool,
    /// Pictures since the last IDR picture, 0 for the IDR picture itself.
    pub order: u32,
    /// The IDR picture's number, different for neighboring IDR pictures.
    pub idr_id: u16,
}

/// Why a coded segment with `status` cannot go into a stream, if it cannot: the picture did not
/// fit in its buffer, or the driver reported an error in what it wrote.
pub(crate) fn segment_error(status: u32) -> Option<&'static str> {
    if status & VA_CODED_BUF_STATUS_SLICE_OVERFLOW_MASK != 0 {
        Some("a coded picture did not fit in its buffer")
    } else if status & VA_CODED_BUF_STATUS_BAD_BITSTREAM != 0 {
        Some("the driver reported an error in the picture it coded")
    } else {
        None
    }
}

/// The size a session's surfaces are made at: the picture rounded up to whole macroblocks for
/// H.264, the picture itself for HEVC (whose size is a multiple of its smallest coding block) and
/// AV1 (whose frame header gives the size).
pub(crate) fn coded_size(codec: Codec, width: u32, height: u32) -> (u32, u32) {
    match codec {
        Codec::H264 => (width.div_ceil(16) * 16, height.div_ceil(16) * 16),
        Codec::Hevc | Codec::Av1 => (width, height),
    }
}

/// The bytes each coded buffer holds: two bytes per pixel and 64 KiB for headers, more than the raw
/// 4:2:0 picture's 1.5, so a picture at the highest quality level fits. A picture that does not fit
/// is an error, never a cut stream.
pub(crate) fn coded_bytes(width: u32, height: u32) -> u32 {
    let bytes = (u64::from(width) * u64::from(height)).saturating_mul(2).saturating_add(64 << 10);
    u32::try_from(bytes).unwrap_or(u32::MAX)
}

/// H.264 sequence parameters: frame_num and the picture order count in 16 bits, one reference
/// frame, and cropping to the picture size.
pub(crate) fn h264_sequence(p: &Params, profile: VAProfile, ctl: Control) -> VAEncSequenceParameterBufferH264 {
    let (cw, ch) = coded_size(Codec::H264, p.width, p.height);
    let (wmb, hmb) = (cw / 16, ch / 16);
    let level = h264_level(u64::from(wmb) * u64::from(hmb), p.fps, ctl.bits_per_second(), profile == VAProfileH264High);
    // chroma_format_idc 1 (4:2:0), frame_mbs_only_flag, direct_8x8_inference_flag,
    // log2_max_frame_num_minus4 12, pic_order_cnt_type 0, log2_max_pic_order_cnt_lsb_minus4 12.
    let seq_fields = 1 | 1 << 2 | 1 << 5 | 12 << 6 | 12 << 12;
    let (crop_right, crop_bottom) = ((cw - p.width) / 2, (ch - p.height) / 2);
    VAEncSequenceParameterBufferH264 {
        seq_parameter_set_id: 0,
        level_idc: level,
        intra_period: p.gop,
        intra_idr_period: p.gop,
        ip_period: 1,
        bits_per_second: ctl.bits_per_second(),
        max_num_ref_frames: 1,
        picture_width_in_mbs: u16::try_from(wmb).unwrap_or(u16::MAX),
        picture_height_in_mbs: u16::try_from(hmb).unwrap_or(u16::MAX),
        seq_fields,
        bit_depth_luma_minus8: 0,
        bit_depth_chroma_minus8: 0,
        num_ref_frames_in_pic_order_cnt_cycle: 0,
        offset_for_non_ref_pic: 0,
        offset_for_top_to_bottom_field: 0,
        offset_for_ref_frame: [0; 256],
        frame_cropping_flag: u8::from(crop_right != 0 || crop_bottom != 0),
        frame_crop_left_offset: 0,
        frame_crop_right_offset: crop_right,
        frame_crop_top_offset: 0,
        frame_crop_bottom_offset: crop_bottom,
        vui_parameters_present_flag: 0,
        vui_fields: 0,
        aspect_ratio_idc: 0,
        sar_width: 0,
        sar_height: 0,
        num_units_in_tick: 0,
        time_scale: 0,
        va_reserved: [0; VA_PADDING_LOW],
    }
}

/// The H.264 reference entry of reconstructed surface `surface` written by the picture `order`
/// pictures after the IDR picture.
fn h264_reference(surface: VASurfaceID, order: u32) -> VAPictureH264 {
    let poc = i32::try_from(u64::from(order) * 2 % 65536).unwrap_or(0);
    VAPictureH264 {
        picture_id: surface,
        frame_idx: order % 65536,
        flags: VA_PICTURE_H264_SHORT_TERM_REFERENCE,
        TopFieldOrderCnt: poc,
        BottomFieldOrderCnt: poc,
        va_reserved: [0; VA_PADDING_LOW],
    }
}

/// H.264 picture parameters of `pic`, written to reconstructed surface `recon` and coded buffer
/// `coded`; a P picture refers to `reference`, the picture before it. CABAC for Main and High,
/// 8 × 8 transforms for High.
pub(crate) fn h264_picture(
    pic: &Picture,
    profile: VAProfile,
    ctl: Control,
    recon: VASurfaceID,
    reference: VASurfaceID,
    coded: VABufferID,
) -> VAEncPictureParameterBufferH264 {
    let mut refs = [VAPictureH264::INVALID; 16];
    if !pic.idr {
        refs[0] = h264_reference(reference, pic.order.wrapping_sub(1));
    }
    let cabac = profile != VAProfileH264ConstrainedBaseline;
    let high = profile == VAProfileH264High;
    // idr_pic_flag (bit 0), reference_pic_flag 1 (bits 1 and 2), entropy_coding_mode_flag (bit 3),
    // transform_8x8_mode_flag (bit 8), deblocking_filter_control_present_flag (bit 9).
    let pic_fields = u32::from(pic.idr) | 1 << 1 | u32::from(cabac) << 3 | u32::from(high) << 8 | 1 << 9;
    VAEncPictureParameterBufferH264 {
        CurrPic: VAPictureH264 { flags: 0, ..h264_reference(recon, pic.order) },
        ReferenceFrames: refs,
        coded_buf: coded,
        pic_parameter_set_id: 0,
        seq_parameter_set_id: 0,
        last_picture: 0,
        frame_num: (pic.order % 65536) as u16,
        pic_init_qp: ctl.initial_qp(),
        num_ref_idx_l0_active_minus1: 0,
        num_ref_idx_l1_active_minus1: 0,
        chroma_qp_index_offset: 0,
        second_chroma_qp_index_offset: 0,
        pic_fields,
        va_reserved: [0; VA_PADDING_LOW],
    }
}

/// The one H.264 slice of `pic`, covering `mbs` macroblocks: I for an IDR picture, else P from
/// `reference`.
pub(crate) fn h264_slice(pic: &Picture, mbs: u32, reference: VASurfaceID) -> VAEncSliceParameterBufferH264 {
    let mut list0 = [VAPictureH264::INVALID; 32];
    if !pic.idr {
        list0[0] = h264_reference(reference, pic.order.wrapping_sub(1));
    }
    VAEncSliceParameterBufferH264 {
        macroblock_address: 0,
        num_macroblocks: mbs,
        macroblock_info: VA_INVALID_ID,
        slice_type: if pic.idr { 2 } else { 0 },
        pic_parameter_set_id: 0,
        idr_pic_id: pic.idr_id,
        pic_order_cnt_lsb: (u64::from(pic.order) * 2 % 65536) as u16,
        delta_pic_order_cnt_bottom: 0,
        delta_pic_order_cnt: [0; 2],
        direct_spatial_mv_pred_flag: 0,
        num_ref_idx_active_override_flag: 0,
        num_ref_idx_l0_active_minus1: 0,
        num_ref_idx_l1_active_minus1: 0,
        RefPicList0: list0,
        RefPicList1: [VAPictureH264::INVALID; 32],
        luma_log2_weight_denom: 0,
        chroma_log2_weight_denom: 0,
        luma_weight_l0_flag: 0,
        luma_weight_l0: [0; 32],
        luma_offset_l0: [0; 32],
        chroma_weight_l0_flag: 0,
        chroma_weight_l0: [[0; 2]; 32],
        chroma_offset_l0: [[0; 2]; 32],
        luma_weight_l1_flag: 0,
        luma_weight_l1: [0; 32],
        luma_offset_l1: [0; 32],
        chroma_weight_l1_flag: 0,
        chroma_weight_l1: [[0; 2]; 32],
        chroma_offset_l1: [[0; 2]; 32],
        cabac_init_idc: 0,
        slice_qp_delta: 0,
        disable_deblocking_filter_idc: 0,
        slice_alpha_c0_offset_div2: 0,
        slice_beta_offset_div2: 0,
        va_reserved: [0; VA_PADDING_LOW],
    }
}

/// HEVC Main sequence parameters at Main tier with the block sizes `b`, low delay (P pictures
/// only), without AMP, SAO, PCM, scaling lists or temporal motion vector prediction.
pub(crate) fn hevc_sequence(p: &Params, b: Blocks, ctl: Control) -> VAEncSequenceParameterBufferHEVC {
    // chroma_format_idc 1 (4:2:0), low_delay_seq.
    let seq_fields = 1 | 1 << 16;
    VAEncSequenceParameterBufferHEVC {
        general_profile_idc: 1,
        general_level_idc: hevc_level(p.width, p.height, p.fps, ctl.bits_per_second()),
        general_tier_flag: 0,
        intra_period: p.gop,
        intra_idr_period: p.gop,
        ip_period: 1,
        bits_per_second: ctl.bits_per_second(),
        pic_width_in_luma_samples: u16::try_from(p.width).unwrap_or(u16::MAX),
        pic_height_in_luma_samples: u16::try_from(p.height).unwrap_or(u16::MAX),
        seq_fields,
        log2_min_luma_coding_block_size_minus3: b.min_cb - 3,
        log2_diff_max_min_luma_coding_block_size: b.ctb - b.min_cb,
        log2_min_transform_block_size_minus2: b.min_tb - 2,
        log2_diff_max_min_transform_block_size: b.max_tb - b.min_tb,
        max_transform_hierarchy_depth_inter: b.depth_inter,
        max_transform_hierarchy_depth_intra: b.depth_intra,
        pcm_sample_bit_depth_luma_minus1: 0,
        pcm_sample_bit_depth_chroma_minus1: 0,
        log2_min_pcm_luma_coding_block_size_minus3: 0,
        log2_max_pcm_luma_coding_block_size_minus3: 0,
        vui_parameters_present_flag: 0,
        vui_fields: 0,
        aspect_ratio_idc: 0,
        sar_width: 0,
        sar_height: 0,
        vui_num_units_in_tick: 0,
        vui_time_scale: 0,
        min_spatial_segmentation_idc: 0,
        max_bytes_per_pic_denom: 0,
        max_bits_per_min_cu_denom: 0,
        scc_fields: 0,
        va_reserved: [0; VA_PADDING_MEDIUM - 1],
    }
}

/// The HEVC reference entry of reconstructed surface `surface`, written by the picture `order`
/// pictures after the IDR picture.
fn hevc_reference(surface: VASurfaceID, order: u32) -> VAPictureHEVC {
    VAPictureHEVC { picture_id: surface, pic_order_cnt: i32::try_from(order).unwrap_or(i32::MAX), flags: 0, va_reserved: [0; VA_PADDING_LOW] }
}

/// HEVC picture parameters of `pic`, written to reconstructed surface `recon` and coded buffer
/// `coded`; a P picture refers to `reference`, the picture before it. A rate-controlled stream
/// lets the encoder change the QP within the picture.
pub(crate) fn hevc_picture(
    pic: &Picture,
    ctl: Control,
    recon: VASurfaceID,
    reference: VASurfaceID,
    coded: VABufferID,
) -> VAEncPictureParameterBufferHEVC {
    let mut refs = [VAPictureHEVC::INVALID; 15];
    if !pic.idr {
        refs[0] = hevc_reference(reference, pic.order.wrapping_sub(1));
    }
    let cu_qp_delta = !matches!(ctl, Control::Cqp(_));
    // idr_pic_flag, coding_type (1 I, 2 P), reference_pic_flag, cu_qp_delta_enabled_flag.
    let coding_type: u32 = if pic.idr { 1 } else { 2 };
    let pic_fields = u32::from(pic.idr) | coding_type << 1 | 1 << 4 | u32::from(cu_qp_delta) << 9;
    VAEncPictureParameterBufferHEVC {
        decoded_curr_pic: hevc_reference(recon, pic.order),
        reference_frames: refs,
        coded_buf: coded,
        collocated_ref_pic_index: 0xff,
        last_picture: 0,
        pic_init_qp: ctl.initial_qp(),
        diff_cu_qp_delta_depth: 0,
        pps_cb_qp_offset: 0,
        pps_cr_qp_offset: 0,
        num_tile_columns_minus1: 0,
        num_tile_rows_minus1: 0,
        column_width_minus1: [0; 19],
        row_height_minus1: [0; 21],
        log2_parallel_merge_level_minus2: 0,
        ctu_max_bitsize_allowed: 0,
        num_ref_idx_l0_default_active_minus1: 0,
        num_ref_idx_l1_default_active_minus1: 0,
        slice_pic_parameter_set_id: 0,
        // IDR_W_RADL for an IDR picture, TRAIL_R for the others.
        nal_unit_type: if pic.idr { 19 } else { 1 },
        pic_fields,
        hierarchical_level_plus1: 0,
        va_byte_reserved: 0,
        scc_fields: 0,
        va_reserved: [0; VA_PADDING_HIGH - 1],
    }
}

/// The one HEVC slice of `pic`, covering `ctus` coding tree blocks: I for an IDR picture, else P
/// from `reference`.
pub(crate) fn hevc_slice(pic: &Picture, ctus: u32, reference: VASurfaceID) -> VAEncSliceParameterBufferHEVC {
    let mut list0 = [VAPictureHEVC::INVALID; 15];
    if !pic.idr {
        list0[0] = hevc_reference(reference, pic.order.wrapping_sub(1));
    }
    // last_slice_of_pic_flag, collocated_from_l0_flag.
    let slice_fields = 1 | 1 << 13;
    VAEncSliceParameterBufferHEVC {
        slice_segment_address: 0,
        num_ctu_in_slice: ctus,
        slice_type: if pic.idr { 2 } else { 1 },
        slice_pic_parameter_set_id: 0,
        num_ref_idx_l0_active_minus1: 0,
        num_ref_idx_l1_active_minus1: 0,
        ref_pic_list0: list0,
        ref_pic_list1: [VAPictureHEVC::INVALID; 15],
        luma_log2_weight_denom: 0,
        delta_chroma_log2_weight_denom: 0,
        delta_luma_weight_l0: [0; 15],
        luma_offset_l0: [0; 15],
        delta_chroma_weight_l0: [[0; 2]; 15],
        chroma_offset_l0: [[0; 2]; 15],
        delta_luma_weight_l1: [0; 15],
        luma_offset_l1: [0; 15],
        delta_chroma_weight_l1: [[0; 2]; 15],
        chroma_offset_l1: [[0; 2]; 15],
        max_num_merge_cand: 5,
        slice_qp_delta: 0,
        slice_cb_qp_offset: 0,
        slice_cr_qp_offset: 0,
        slice_beta_offset_div2: 0,
        slice_tc_offset_div2: 0,
        slice_fields,
        pred_weight_table_bit_offset: 0,
        pred_weight_table_bit_length: 0,
        va_reserved: [0; VA_PADDING_MEDIUM - 2],
    }
}

/// The rate control and buffer parameters of a rate-controlled stream: the bitrate as both target
/// and peak over a one-second window, and a one-second buffer that starts three quarters full.
/// Constant QP has none.
pub(crate) fn rate_buffers(
    ctl: Control,
) -> Option<(VAEncMiscParameterBuffer<VAEncMiscParameterRateControl>, VAEncMiscParameterBuffer<VAEncMiscParameterHRD>)> {
    let bps = match ctl {
        Control::Cqp(_) => return None,
        Control::Cbr(bps) | Control::Vbr(bps) => bps,
    };
    let rc = VAEncMiscParameterRateControl { bits_per_second: bps, target_percentage: 100, window_size: 1000, ..Default::default() };
    let hrd = VAEncMiscParameterHRD { initial_buffer_fullness: bps / 4 * 3, buffer_size: bps, va_reserved: [0; VA_PADDING_LOW] };
    Some((
        VAEncMiscParameterBuffer { type_: VAEncMiscParameterTypeRateControl, data: rc },
        VAEncMiscParameterBuffer { type_: VAEncMiscParameterTypeHRD, data: hrd },
    ))
}

/// The frame rate parameter buffer of `fps`.
pub(crate) fn frame_rate_buffer(fps: (u32, u32)) -> VAEncMiscParameterBuffer<VAEncMiscParameterFrameRate> {
    VAEncMiscParameterBuffer {
        type_: VAEncMiscParameterTypeFrameRate,
        data: VAEncMiscParameterFrameRate { framerate: frame_rate(fps), ..Default::default() },
    }
}

/// Copy the packed NV12 `frame` of `width` × `height` into the mapped image `dst` of the coded
/// size `coded` (width, height), whose luma and chroma planes start at `offsets` and step
/// `pitches` bytes a row. The columns and rows past the frame repeat its last column and row, so
/// the padding the encoder codes holds the frame's edge, never what the surface held before.
pub(crate) fn copy_nv12(
    frame: &[u8],
    width: usize,
    height: usize,
    coded: [usize; 2],
    dst: &mut [u8],
    pitches: [usize; 2],
    offsets: [usize; 2],
) -> Result<(), String> {
    let luma = width.checked_mul(height).filter(|&n| n > 0 && width.is_multiple_of(2) && height.is_multiple_of(2));
    let Some(luma) = luma.filter(|&n| n.checked_add(n / 2) == Some(frame.len())) else {
        return Err(format!("a frame of {} bytes is not an NV12 frame of {width}x{height}", frame.len()));
    };
    let [cw, ch] = coded;
    if cw < width || ch < height || !cw.is_multiple_of(2) || !ch.is_multiple_of(2) {
        return Err(format!("the coded size {cw}x{ch} does not hold a frame of {width}x{height}"));
    }
    let (y, uv) = frame.split_at(luma);
    // The plane, its rows in the frame and in the surface, the bytes a pixel takes, the pitch and the offset.
    let planes = [(y, height, ch, 1, pitches[0], offsets[0]), (uv, height / 2, ch / 2, 2, pitches[1], offsets[1])];
    for (plane, rows, coded_rows, px, pitch, offset) in planes {
        if pitch < cw {
            return Err(format!("the surface's rows of {pitch} bytes are narrower than {cw}"));
        }
        let end = (coded_rows - 1).checked_mul(pitch).and_then(|n| n.checked_add(offset)?.checked_add(cw));
        if end.is_none_or(|end| end > dst.len()) {
            return Err("the surface's image is smaller than the frame".into());
        }
        // The rows past the frame repeat its last row, copied from the frame: a mapped surface
        // can be uncached memory, which is slow to read back.
        let src_rows = plane.chunks_exact(width).take(rows);
        let last = plane.chunks_exact(width).nth(rows - 1).unwrap_or(&[]);
        for (r, src) in src_rows.chain(std::iter::repeat_n(last, coded_rows - rows)).enumerate() {
            let at = offset + r * pitch;
            let row = &mut dst[at..at + cw];
            row[..width].copy_from_slice(src);
            let edge = [src[width - px], src[width - 1]];
            for pad in row[width..].chunks_exact_mut(px) {
                pad.copy_from_slice(&edge[2 - px..]);
            }
        }
    }
    Ok(())
}

/// Input surfaces a session imports from DMA-BUFs, each under its own key.
pub(crate) const IMPORTS: usize = 8;

/// The descriptor `vaCreateSurfaces` imports `frame`, an NV12 picture of `width` x `height` in the
/// DMA-BUF `fd`, from: one memory object and one NV12 layer with its two planes. Each plane, as
/// the exporting driver reports its bytes, must lie inside the buffer. In a linear layout each
/// luma row must also hold the picture's width, each chroma row its whole interleaved pairs, and
/// every row must lie inside its plane's reported bytes, the last one without a whole pitch after
/// it; a tiled layout's
/// rows are laid out by its modifier, and
/// the reported plane sizes are its planes' extent. A picture that breaks these is an error before
/// VA-API is called.
pub(crate) fn prime_descriptor(fd: std::ffi::c_int, width: u32, height: u32, frame: &PrimeFrame) -> Result<VADRMPRIMESurfaceDescriptor, String> {
    let bad = |why: &str| Err(format!("a DMA-BUF that {why} is not an NV12 picture of {width}x{height}"));
    if fd < 0 {
        return bad("has no descriptor");
    }
    let Ok(size) = u32::try_from(frame.size) else { return bad("is larger than 4 GiB") };
    if width == 0 || height == 0 || frame.pitches.contains(&0) || frame.plane_sizes.contains(&0) {
        return bad("has an empty plane");
    }
    let linear = frame.modifier == DRM_FORMAT_MOD_LINEAR;
    if linear && (frame.pitches[0] < width || u64::from(frame.pitches[1]) < 2 * u64::from(width.div_ceil(2))) {
        return bad("has rows shorter than the picture");
    }
    let reported = |plane: usize| u64::from(frame.offsets[plane]).checked_add(frame.plane_sizes[plane]).is_some_and(|end| end <= frame.size);
    // The last row of `rows` rows `pitch` bytes apart ends `row_bytes` into it, inside the plane's
    // reported bytes, which lie inside the buffer (`reported`): it needs no whole pitch after it.
    let in_plane = |plane: usize, rows: u32, row_bytes: u64| {
        u64::from(frame.pitches[plane]) * u64::from(rows.saturating_sub(1)) + row_bytes <= frame.plane_sizes[plane]
    };
    let rows_fit = || in_plane(0, height, u64::from(width)) && in_plane(1, height.div_ceil(2), 2 * u64::from(width.div_ceil(2)));
    if !reported(0) || !reported(1) || (linear && !rows_fit()) {
        return bad("ends before its planes do");
    }
    let mut desc = VADRMPRIMESurfaceDescriptor { fourcc: VA_FOURCC_NV12, width, height, num_objects: 1, num_layers: 1, ..Default::default() };
    desc.objects[0] = VADRMPRIMEObject { fd, size, drm_format_modifier: frame.modifier };
    desc.layers[0] = VADRMPRIMELayer {
        drm_format: DRM_FORMAT_NV12,
        num_planes: 2,
        object_index: [0; 4],
        offset: [frame.offsets[0], frame.offsets[1], 0, 0],
        pitch: [frame.pitches[0], frame.pitches[1], 0, 0],
    };
    Ok(desc)
}

/// The size of the surface a session for `p` imports `frame` as: the image's own size, which must
/// hold the session's coded size, padded rows and columns included.
pub(crate) fn import_size(p: &Params, frame: &PrimeFrame) -> Result<(u32, u32), String> {
    let (cw, ch) = coded_size(p.codec, p.width, p.height);
    if frame.width < cw || frame.height < ch {
        return Err(format!("a DMA-BUF image of {}x{} is smaller than the coded size {cw}x{ch}", frame.width, frame.height));
    }
    Ok((frame.width, frame.height))
}

/// `drm_fourcc.h`'s linear layout modifier.
pub(crate) const DRM_FORMAT_MOD_LINEAR: u64 = 0;

/// The render node among `nodes` (its path and its device number) whose device number is `dev`.
pub(crate) fn node_with(dev: u64, nodes: impl IntoIterator<Item = (String, u64)>) -> Option<String> {
    nodes.into_iter().find(|&(_, d)| d == dev).map(|(path, _)| path)
}

#[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
mod none {
    use super::{OpenError, Params, Picture, PrimeFrame, Tag};

    const WHY: &str = "VA-API runs on 64-bit Linux only";

    /// Whether libva loads: never off 64-bit Linux.
    pub(crate) fn driver() -> Result<(), String> {
        Err(WHY.into())
    }

    /// Off 64-bit Linux no session opens.
    pub(crate) enum Session {}

    impl Session {
        pub(crate) fn open(_: &Params) -> Result<Session, OpenError> {
            Err(OpenError::NoDevice(WHY.into()))
        }
        pub(crate) fn open_on(_: &Params, _: &str) -> Result<Session, OpenError> {
            Err(OpenError::NoDevice(WHY.into()))
        }
        pub(crate) fn full(&self) -> bool {
            match *self {}
        }
        pub(crate) fn in_flight(&self) -> usize {
            match *self {}
        }
        pub(crate) fn sequence_obu(&self) -> &[u8] {
            match *self {}
        }
        pub(crate) fn submit(&mut self, _: &[u8], _: &Picture, _: Tag) -> Result<(), String> {
            match *self {}
        }
        pub(crate) fn import(&mut self, _: usize, _: std::ffi::c_int, _: &PrimeFrame) -> Result<(), String> {
            match *self {}
        }
        pub(crate) fn imported(&self, _: usize) -> bool {
            match *self {}
        }
        pub(crate) fn submit_imported(&mut self, _: usize, _: &Picture, _: Tag) -> Result<(), String> {
            match *self {}
        }
        pub(crate) fn collect(&mut self, _: &mut Vec<u8>) -> Result<Option<Tag>, String> {
            match *self {}
        }
        pub(crate) fn oldest_coded(&mut self) -> Option<bool> {
            match *self {}
        }
        pub(crate) fn wait_all(&mut self) -> bool {
            match *self {}
        }
    }
}
