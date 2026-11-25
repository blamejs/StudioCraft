//! AV1 on VA-API. The driver codes the tiles; this crate writes the sequence header OBU and each
//! frame header OBU (AV1 Bitstream and Decoding Process Specification 5.5 and 5.9) and hands them
//! to the driver as packed headers, with parameter buffers that describe the same choices.
//!
//! - The stream is Main profile, 8-bit 4:2:0, BT.709 in limited range, with 64 × 64 superblocks,
//!   8-bit order hints and one operating point at Main tier.
//! - Every frame is shown. A key frame refreshes all eight reference slots, and every inter frame
//!   refers to the frame before it through every reference and refreshes all eight slots again,
//!   so each slot always holds the frame before. Each frame starts from the default probabilities
//!   (`primary_ref_frame` is none).
//! - The stream turns off every optional coding tool: CDEF, loop restoration, superres, filter
//!   intra, intra edge filtering, compound prediction, warped and global motion, palette, intra
//!   block copy, segmentation, quantizer matrices and delta quantizers. A driver that requires one
//!   of the tools does not open.
//! - Tiles are uniform and as few as the frame size and frame rate allow: each at most 4096
//!   pixels wide, at most 4096 × 2304 pixels in area and at most 588,251,136 pixels a second.
//! - The quality level `q` (1 to 51) is the base quantizer index 5 × `q`, at most 255, which is
//!   this crate's own mapping. The deblocking level is the quantizer index divided by 6 (rounded),
//!   at most 63, for every plane and direction: also this crate's own choice. Under rate control
//!   the driver writes the quantizer index and deblocking levels into the frame header at the bit
//!   offsets the picture parameters give.

use craft_media::bitstream::BitWriter;

use super::abi::*;
use super::{Control, Params, Picture};

/// OBU types (AV1 6.2.2).
const OBU_SEQUENCE_HEADER: u8 = 1;
const OBU_FRAME_HEADER: u8 = 3;
/// `OrderHintBits`.
pub(crate) const ORDER_HINT_BITS: u32 = 8;
/// `primary_ref_frame` when a frame takes no state from a reference.
pub(crate) const PRIMARY_REF_NONE: u8 = 7;
/// `interpolation_filter` value that lets each block choose (AV1 6.8.9).
pub(crate) const SWITCHABLE: u8 = 4;
/// `TxMode` values (AV1 6.8.21).
pub(crate) const TX_MODE_LARGEST: u8 = 1;
pub(crate) const TX_MODE_SELECT: u8 = 2;

/// Rows of AV1 Annex A.3's level tables: `seq_level_idx`, MaxPicSize, MaxHSize, MaxVSize,
/// MaxDisplayRate, MaxHeaderRate and MainMbps in bit/s. Levels 5.3 and 6.3 differ from 5.2 and
/// 6.2 only in MaxDecodeRate.
const LEVELS: [(u8, u64, u32, u32, u64, u64, u64); 14] = [
    (0, 147_456, 2048, 1152, 4_423_680, 150, 1_500_000),
    (1, 278_784, 2816, 1584, 8_363_520, 150, 3_000_000),
    (4, 665_856, 4352, 2448, 19_975_680, 150, 6_000_000),
    (5, 1_065_024, 5504, 3096, 31_950_720, 150, 10_000_000),
    (8, 2_359_296, 6144, 3456, 70_778_880, 300, 12_000_000),
    (9, 2_359_296, 6144, 3456, 141_557_760, 300, 20_000_000),
    (12, 8_912_896, 8192, 4352, 267_386_880, 300, 30_000_000),
    (13, 8_912_896, 8192, 4352, 534_773_760, 300, 40_000_000),
    (14, 8_912_896, 8192, 4352, 1_069_547_520, 300, 60_000_000),
    (15, 8_912_896, 8192, 4352, 1_069_547_520, 300, 60_000_000),
    (16, 35_651_584, 16384, 8704, 1_069_547_520, 300, 60_000_000),
    (17, 35_651_584, 16384, 8704, 2_139_095_040, 300, 100_000_000),
    (18, 35_651_584, 16384, 8704, 4_278_190_080, 300, 160_000_000),
    (19, 35_651_584, 16384, 8704, 4_278_190_080, 300, 160_000_000),
];

/// The highest bitrate a Main tier level allows: level 6.3's 160 Mbit/s.
pub(crate) const MAX_MAIN_BPS: u32 = 160_000_000;

/// The narrowest and shortest frame the AV1 encoder takes, in pixels.
pub(crate) const MIN_SIDE: u32 = 16;

/// Luma samples a second of a `width` × `height` stream at `fps`.
fn display_rate(width: u32, height: u32, fps: (u32, u32)) -> u64 {
    (u64::from(width) * u64::from(height)).saturating_mul(u64::from(fps.0)).div_ceil(u64::from(fps.1.max(1)))
}

/// Whether `fps`, one frame header a frame, is at most `most` frame headers a second.
fn headers_fit(fps: (u32, u32), most: u64) -> bool {
    u64::from(fps.0) <= most.saturating_mul(u64::from(fps.1.max(1)))
}

/// The `seq_level_idx` of a `width` × `height` stream at `fps` and `bps`: the lowest level whose
/// picture size, picture width and height, display rate, frame header rate and Main tier bitrate
/// take it, else 6.3.
pub(crate) fn level(width: u32, height: u32, fps: (u32, u32), bps: u32) -> u8 {
    let (size, rate) = (u64::from(width) * u64::from(height), display_rate(width, height, fps));
    LEVELS
        .iter()
        .find(|&&(_, pic, w, h, display, headers, mbps)| {
            size <= pic && width <= w && height <= h && rate <= display && headers_fit(fps, headers) && u64::from(bps) <= mbps
        })
        .map_or(19, |&(level, ..)| level)
}

/// Whether a `width` × `height` stream at `fps` fits level 6.3, the highest level.
pub(crate) fn fits_a_level(width: u32, height: u32, fps: (u32, u32)) -> bool {
    let (_, pic, w, h, display, headers, _) = LEVELS[LEVELS.len() - 1];
    u64::from(width) * u64::from(height) <= pic
        && width <= w
        && height <= h
        && display_rate(width, height, fps) <= display
        && headers_fit(fps, headers)
}

/// What the driver's AV1 attributes let a stream use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Tools {
    /// `TxMode` of every frame: [`TX_MODE_SELECT`] or [`TX_MODE_LARGEST`].
    pub tx_mode: u8,
    /// `interpolation_filter` of inter frames: 0 to 3 for one filter, or [`SWITCHABLE`].
    pub filter: u8,
    /// Bytes of each tile size the driver writes (`TileSizeBytes`).
    pub tile_size_bytes: u8,
    /// Bytes of the `obu_size` field of each frame header OBU.
    pub obu_size_bytes: u8,
    /// The most tiles a frame may have.
    pub max_tiles: u32,
}

/// The tools of the driver's `VAConfigAttribEncAV1`, `VAConfigAttribEncAV1Ext1` and
/// `VAConfigAttribEncAV1Ext2` values. An attribute the driver does not have takes the defaults:
/// TX_MODE_SELECT, the regular eight-tap filter, 4-byte tile and OBU sizes and no tile limit. A
/// `tx_mode_support` of 0 is read as not reported. A driver that requires a coding tool the stream
/// turns off, or that has neither TX_MODE_SELECT nor TX_MODE_LARGEST, is an error.
pub(crate) fn tools(features: u32, ext1: u32, ext2: u32) -> Result<Tools, String> {
    if features != VA_ATTRIB_NOT_SUPPORTED && (0..14).any(|i| (features >> (2 * i)) & 3 == VA_FEATURE_REQUIRED) {
        return Err("the AV1 encoder requires a coding tool this crate's AV1 streams turn off".into());
    }
    let filters = if ext1 == VA_ATTRIB_NOT_SUPPORTED { 0 } else { ext1 & 0x1f };
    let filter = if filters & 0x10 != 0 { SWITCHABLE } else { (0..4u8).find(|&f| (filters >> f) & 1 != 0).unwrap_or(0) };
    if ext2 == VA_ATTRIB_NOT_SUPPORTED {
        return Ok(Tools { tx_mode: TX_MODE_SELECT, filter, tile_size_bytes: 4, obu_size_bytes: 4, max_tiles: u32::MAX });
    }
    let tx = (ext2 >> 4) & 7;
    let tx_mode = match tx {
        0 => TX_MODE_SELECT,
        _ if tx & 4 != 0 => TX_MODE_SELECT,
        _ if tx & 2 != 0 => TX_MODE_LARGEST,
        _ => return Err("the AV1 encoder codes 4 × 4 transforms only".into()),
    };
    Ok(Tools {
        tx_mode,
        filter,
        tile_size_bytes: (ext2 & 3) as u8 + 1,
        obu_size_bytes: ((ext2 >> 2) & 3) as u8 + 1,
        max_tiles: ((ext2 >> 7) & 0x1fff) + 1,
    })
}

/// The smallest `k` with `blk << k` at least `target` (AV1 5.9.15's `tile_log2`).
fn tile_log2(blk: u32, target: u32) -> u8 {
    let mut k = 0;
    while (u64::from(blk) << k) < u64::from(target) {
        k += 1;
    }
    k
}

/// The most luma samples one tile may hold, `MAX_TILE_AREA` (AV1 3).
const MAX_TILE_AREA: u64 = 4096 * 2304;

/// The most luma samples one tile may code a second in a stream without temporal layers (AV1
/// Annex A.3: `MaxTileSizeInLumaSamples` × `NumFrameHeadersSec`).
const MAX_TILE_RATE: u64 = 588_251_136;

/// The tiles of a frame: uniform, with the fewest that keep every tile within the size and rate
/// limits (AV1 5.9.15 and Annex A.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Tiles {
    /// Superblocks across and down the frame.
    pub sb_cols: u32,
    pub sb_rows: u32,
    pub cols_log2: u8,
    pub rows_log2: u8,
    /// `minLog2TileCols` and `minLog2TileRows`: from these an `increment_tile_*_log2` bit of 1
    /// is coded for each step up to `cols_log2` and `rows_log2`.
    pub min_cols_log2: u8,
    pub min_rows_log2: u8,
    /// `maxLog2TileCols` and `maxLog2TileRows`: below these an `increment_tile_*_log2` bit of 0
    /// ends the steps.
    pub max_cols_log2: u8,
    pub max_rows_log2: u8,
    /// Superblocks across each tile but the last column, and down each tile but the last row.
    pub width_sb: u32,
    pub height_sb: u32,
    /// `TileCols` and `TileRows`.
    pub cols: u32,
    pub rows: u32,
}

impl Tiles {
    /// The tiles of a `width` × `height` frame at `fps`: the first uniform grid whose largest
    /// tile is at most 4096 pixels wide, spans at most 2,304 superblocks, holds at most
    /// [`MAX_TILE_AREA`] luma samples and codes at
    /// most [`MAX_TILE_RATE`] luma samples a second, counting whole 4 × 4 blocks, and whose last
    /// column and row keep at least 8 pixels of the frame. Grids are tried by `cols_log2 + rows_log2`,
    /// smallest first, then by `cols_log2`, smallest first. `None` when no uniform grid keeps
    /// within the limits.
    pub(crate) fn of(width: u32, height: u32, fps: (u32, u32)) -> Option<Tiles> {
        let (mi_cols, mi_rows) = (2 * (width.div_ceil(8)), 2 * (height.div_ceil(8)));
        let (sb_cols, sb_rows) = (mi_cols.div_ceil(16), mi_rows.div_ceil(16));
        // MAX_TILE_WIDTH and MAX_TILE_AREA in 64 × 64 superblocks, MAX_TILE_COLS and MAX_TILE_ROWS.
        let (max_width_sb, max_area_sb) = (4096 >> 6, (MAX_TILE_AREA >> 12) as u32);
        let min_cols_log2 = tile_log2(max_width_sb, sb_cols);
        let max_cols_log2 = tile_log2(1, sb_cols.min(64));
        let max_rows_log2 = tile_log2(1, sb_rows.min(64));
        let min_tiles_log2 = min_cols_log2.max(tile_log2(max_area_sb, sb_rows.saturating_mul(sb_cols)));
        for total in min_tiles_log2..=max_cols_log2 + max_rows_log2 {
            for cols_log2 in min_cols_log2..=total.min(max_cols_log2) {
                let rows_log2 = total - cols_log2;
                if rows_log2 > max_rows_log2 {
                    continue;
                }
                let width_sb = (sb_cols + (1 << cols_log2) - 1) >> cols_log2;
                let height_sb = (sb_rows + (1 << rows_log2) - 1) >> rows_log2;
                // The first tile is the largest. Its size counts whole 4 × 4 blocks (`TileWidth` and
                // `TileHeight`), so a frame smaller than a superblock keeps its size rounded up to 8.
                let samples = (u64::from(width_sb) * 64).min(u64::from(mi_cols) * 4) * (u64::from(height_sb) * 64).min(u64::from(mi_rows) * 4);
                let per_second = samples.saturating_mul(u64::from(fps.0));
                // `tileWidthSb * tileHeightSb` is at most `maxTileAreaSb` (the tile_info
                // semantics), counted before the frame's edge crops the tile.
                let area_sb = u64::from(width_sb) * u64::from(height_sb);
                if width_sb > max_width_sb
                    || area_sb > u64::from(max_area_sb)
                    || samples > MAX_TILE_AREA
                    || per_second > MAX_TILE_RATE.saturating_mul(u64::from(fps.1.max(1)))
                {
                    continue;
                }
                // The last column and row keep at least 8 pixels of the frame (`CroppedTileWidth` and
                // `CroppedTileHeight`). A frame narrower than 8 pixels has one tile and is not split.
                let (cols, rows) = (sb_cols.div_ceil(width_sb.max(1)), sb_rows.div_ceil(height_sb.max(1)));
                let cropped = |size: u32, each: u32, n: u32| u64::from(size).saturating_sub(u64::from(n.saturating_sub(1)) * u64::from(each) * 64);
                if cropped(width, width_sb, cols) < u64::from(width.min(8)) || cropped(height, height_sb, rows) < u64::from(height.min(8)) {
                    continue;
                }
                return Some(Tiles {
                    sb_cols,
                    sb_rows,
                    cols_log2,
                    rows_log2,
                    min_cols_log2,
                    min_rows_log2: min_tiles_log2.saturating_sub(cols_log2),
                    max_cols_log2,
                    max_rows_log2,
                    width_sb,
                    height_sb,
                    cols,
                    rows,
                });
            }
        }
        None
    }

    pub(crate) fn count(&self) -> u32 {
        self.cols * self.rows
    }

    /// Superblocks across each tile column and down each tile row, minus one, as the picture
    /// parameters list them.
    fn sizes(&self) -> ([u16; 63], [u16; 63]) {
        let list = |n: u32, each: u32, total: u32| {
            let mut out = [0u16; 63];
            for (i, v) in out.iter_mut().enumerate().take(n as usize) {
                let start = i as u32 * each;
                *v = (each.min(total - start) - 1) as u16;
            }
            out
        };
        (list(self.cols, self.width_sb, self.sb_cols), list(self.rows, self.height_sb, self.sb_rows))
    }
}

/// Append an OBU of `kind` holding `payload` to `out`, with `obu_has_size_field` set and its size
/// as `leb128()` in `size_bytes` bytes (AV1 4.10.5 allows sizes written in more bytes than they
/// need).
fn obu(kind: u8, payload: &[u8], size_bytes: u8, out: &mut Vec<u8>) {
    out.push((kind << 3) | 0x02);
    let mut v = payload.len() as u64;
    for i in 0..size_bytes {
        let more = if i + 1 < size_bytes { 0x80 } else { 0 };
        out.push((v & 0x7f) as u8 | more);
        v >>= 7;
    }
    out.extend_from_slice(payload);
}

/// The bits `v` takes, at least 1.
fn bits(v: u32) -> u32 {
    (32 - v.leading_zeros()).max(1)
}

/// The sequence header OBU of a `width` × `height` stream at `seq_level_idx` `level` (AV1 5.5),
/// with its size in the fewest bytes.
pub(crate) fn sequence_header(width: u32, height: u32, level: u8) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.write_bits(0, 3); // seq_profile: Main
    w.write_bit(false); // still_picture
    w.write_bit(false); // reduced_still_picture_header
    w.write_bit(false); // timing_info_present_flag
    w.write_bit(false); // initial_display_delay_present_flag
    w.write_bits(0, 5); // operating_points_cnt_minus_1
    w.write_bits(0, 12); // operating_point_idc[0]
    w.write_bits(u32::from(level), 5); // seq_level_idx[0]
    if level > 7 {
        w.write_bit(false); // seq_tier[0]: Main
    }
    let (wb, hb) = (bits(width - 1), bits(height - 1));
    w.write_bits(wb - 1, 4); // frame_width_bits_minus_1
    w.write_bits(hb - 1, 4); // frame_height_bits_minus_1
    w.write_bits(width - 1, wb); // max_frame_width_minus_1
    w.write_bits(height - 1, hb); // max_frame_height_minus_1
    w.write_bit(false); // frame_id_numbers_present_flag
    w.write_bit(false); // use_128x128_superblock
    w.write_bit(false); // enable_filter_intra
    w.write_bit(false); // enable_intra_edge_filter
    w.write_bit(false); // enable_interintra_compound
    w.write_bit(false); // enable_masked_compound
    w.write_bit(false); // enable_warped_motion
    w.write_bit(false); // enable_dual_filter
    w.write_bit(true); // enable_order_hint
    w.write_bit(false); // enable_jnt_comp
    w.write_bit(false); // enable_ref_frame_mvs
    w.write_bit(false); // seq_choose_screen_content_tools
    w.write_bit(false); // seq_force_screen_content_tools
    w.write_bits(ORDER_HINT_BITS - 1, 3); // order_hint_bits_minus_1
    w.write_bit(false); // enable_superres
    w.write_bit(false); // enable_cdef
    w.write_bit(false); // enable_restoration
    // color_config()
    w.write_bit(false); // high_bitdepth
    w.write_bit(false); // mono_chrome
    w.write_bit(true); // color_description_present_flag
    w.write_bits(1, 8); // color_primaries: BT.709
    w.write_bits(1, 8); // transfer_characteristics: BT.709
    w.write_bits(1, 8); // matrix_coefficients: BT.709
    w.write_bit(false); // color_range: limited
    w.write_bits(0, 2); // chroma_sample_position: unknown
    w.write_bit(false); // separate_uv_delta_q
    w.write_bit(false); // film_grain_params_present
    w.write_bit(true); // trailing_one_bit
    let payload = w.finish();
    let mut out = Vec::with_capacity(payload.len() + 2);
    obu(OBU_SEQUENCE_HEADER, &payload, if payload.len() < 128 { 1 } else { 2 }, &mut out);
    out
}

/// The quantizer index a frame starts at: 5 × the constant QP, or 5 × 26 under rate control.
pub(crate) fn base_qindex(ctl: Control) -> u8 {
    let qp = match ctl {
        Control::Cqp(qp) => qp,
        Control::Cbr(_) | Control::Vbr(_) => 26,
    };
    u8::try_from(u32::from(qp) * 5).unwrap_or(255)
}

/// The deblocking level of a quantizer index: the index divided by 6, rounded, at most 63.
pub(crate) fn filter_level(qindex: u8) -> u8 {
    ((u32::from(qindex) + 3) / 6).min(63) as u8
}

/// A frame header OBU and where the fields the driver may rewrite are, in bits from its first
/// byte.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FrameHeader {
    pub obu: Vec<u8>,
    /// `base_q_idx`.
    pub qindex_bit: u32,
    /// `loop_filter_params()`.
    pub loop_filter_bit: u32,
    /// Where `cdef_params()` would be; the stream codes no CDEF fields.
    pub cdef_bit: u32,
    /// The bits of the OBU up to and including its trailing one bit.
    pub bits: u32,
}

/// The frame header OBU of `pic` (AV1 5.9): a key frame for an IDR picture, else an inter frame
/// that refers to the frame before it. `tiles` and `tools` must be the session's, and `ctl` its
/// rate control.
pub(crate) fn frame_header(tiles: &Tiles, tools: &Tools, pic: &Picture, ctl: Control) -> FrameHeader {
    let key = pic.idr;
    let mut w = BitWriter::new();
    w.write_bit(false); // show_existing_frame
    w.write_bits(if key { 0 } else { 1 }, 2); // frame_type: KEY_FRAME or INTER_FRAME
    w.write_bit(true); // show_frame
    if !key {
        w.write_bit(false); // error_resilient_mode (always 1 for a shown key frame, not coded)
    }
    w.write_bit(false); // disable_cdf_update
    // allow_screen_content_tools is seq_force_screen_content_tools (0) and is not coded.
    w.write_bit(false); // frame_size_override_flag
    w.write_bits(pic.order % (1 << ORDER_HINT_BITS), ORDER_HINT_BITS); // order_hint
    if !key {
        w.write_bits(u32::from(PRIMARY_REF_NONE), 3); // primary_ref_frame
        w.write_bits(0xff, 8); // refresh_frame_flags (all frames for a shown key frame, not coded)
    }
    if key {
        // frame_size() and superres_params() code nothing without a size override or superres.
        w.write_bit(false); // render_and_frame_size_different
    } else {
        w.write_bit(false); // frame_refs_short_signaling
        for _ in 0..7 {
            w.write_bits(0, 3); // ref_frame_idx[i]: slot 0
        }
        w.write_bit(false); // render_and_frame_size_different
        w.write_bit(false); // allow_high_precision_mv
        if tools.filter == SWITCHABLE {
            w.write_bit(true); // is_filter_switchable
        } else {
            w.write_bit(false); // is_filter_switchable
            w.write_bits(u32::from(tools.filter), 2); // interpolation_filter
        }
        w.write_bit(false); // is_motion_mode_switchable
    }
    w.write_bit(false); // disable_frame_end_update_cdf
    // tile_info()
    w.write_bit(true); // uniform_tile_spacing_flag
    for _ in tiles.min_cols_log2..tiles.cols_log2 {
        w.write_bit(true); // increment_tile_cols_log2
    }
    if tiles.cols_log2 < tiles.max_cols_log2 {
        w.write_bit(false); // increment_tile_cols_log2
    }
    for _ in tiles.min_rows_log2..tiles.rows_log2 {
        w.write_bit(true); // increment_tile_rows_log2
    }
    if tiles.rows_log2 < tiles.max_rows_log2 {
        w.write_bit(false); // increment_tile_rows_log2
    }
    if tiles.cols_log2 > 0 || tiles.rows_log2 > 0 {
        w.write_bits(0, u32::from(tiles.cols_log2 + tiles.rows_log2)); // context_update_tile_id
        w.write_bits(u32::from(tools.tile_size_bytes) - 1, 2); // tile_size_bytes_minus_1
    }
    let prefix = 8 * (1 + u32::from(tools.obu_size_bytes));
    let at = |w: &BitWriter| prefix + w.bit_len() as u32;
    // quantization_params()
    let qindex_bit = at(&w);
    let qindex = base_qindex(ctl);
    w.write_bits(u32::from(qindex), 8); // base_q_idx
    w.write_bit(false); // DeltaQYDc: delta_coded
    w.write_bit(false); // DeltaQUDc: delta_coded (diff_uv_delta is 0 without separate_uv_delta_q)
    w.write_bit(false); // DeltaQUAc: delta_coded
    w.write_bit(false); // using_qmatrix
    w.write_bit(false); // segmentation_enabled
    w.write_bit(false); // delta_q_present (coded since base_q_idx > 0)
    // loop_filter_params()
    let loop_filter_bit = at(&w);
    let level = filter_level(qindex);
    w.write_bits(u32::from(level), 6); // loop_filter_level[0]
    w.write_bits(u32::from(level), 6); // loop_filter_level[1]
    if level != 0 {
        w.write_bits(u32::from(level), 6); // loop_filter_level[2]
        w.write_bits(u32::from(level), 6); // loop_filter_level[3]
    }
    w.write_bits(0, 3); // loop_filter_sharpness
    w.write_bit(false); // loop_filter_delta_enabled
    // cdef_params() and lr_params() code nothing without enable_cdef and enable_restoration.
    let cdef_bit = at(&w);
    w.write_bit(tools.tx_mode == TX_MODE_SELECT); // tx_mode_select
    if !key {
        w.write_bit(false); // reference_select
    }
    // skip_mode_params() and allow_warped_motion code nothing here.
    w.write_bit(false); // reduced_tx_set
    if !key {
        for _ in 0..7 {
            w.write_bit(false); // is_global
        }
    }
    w.write_bit(true); // trailing_one_bit
    let bits = at(&w);
    let payload = w.finish();
    let mut out = Vec::with_capacity(payload.len() + 5);
    obu(OBU_FRAME_HEADER, &payload, tools.obu_size_bytes, &mut out);
    FrameHeader { obu: out, qindex_bit, loop_filter_bit, cdef_bit, bits }
}

/// AV1 sequence parameters at `seq_level_idx` `level`: Main profile, Main tier, 8-bit 4:2:0 with
/// order hints and no optional tools.
pub(crate) fn av1_sequence(p: &Params, level: u8, ctl: Control) -> VAEncSequenceParameterBufferAV1 {
    // enable_order_hint (bit 8), subsampling_x (bit 17), subsampling_y (bit 18).
    let seq_fields = 1 << 8 | 1 << 17 | 1 << 18;
    VAEncSequenceParameterBufferAV1 {
        seq_profile: 0,
        seq_level_idx: level,
        seq_tier: 0,
        hierarchical_flag: 0,
        intra_period: p.gop,
        ip_period: 1,
        bits_per_second: ctl.bits_per_second(),
        seq_fields,
        order_hint_bits_minus_1: (ORDER_HINT_BITS - 1) as u8,
        va_reserved: [0; VA_PADDING_HIGH],
    }
}

/// AV1 picture parameters of `pic`, whose frame header is `header`, written to reconstructed
/// surface `recon` and coded buffer `coded`; an inter frame refers to `reference`, the frame before
/// it, which every reference slot holds. Under rate control the driver gets the bit offsets of the
/// fields it rewrites.
pub(crate) fn av1_picture(
    p: &Params,
    tiles: &Tiles,
    tools: &Tools,
    pic: &Picture,
    ctl: Control,
    header: &FrameHeader,
    recon: VASurfaceID,
    reference: VASurfaceID,
    coded: VABufferID,
) -> VAEncPictureParameterBufferAV1 {
    let key = pic.idr;
    let qindex = base_qindex(ctl);
    let level = filter_level(qindex);
    // frame_type (bits 0 and 1), error_resilient_mode (bit 2), force_integer_mv (bit 15): a shown
    // key frame is error resilient, and an intra frame's motion vectors are whole pixels.
    let picture_flags = if key { 1 << 2 | 1 << 15 } else { 1 };
    // tx_mode (bits 7 and 8); reference_mode 0 is single reference.
    let mode_control_flags = u32::from(tools.tx_mode) << 7;
    // LAST_FRAME (ref_frame_idx[0]) is the one reference searched.
    let ref_frame_ctrl_l0 = if key { 0 } else { 1 };
    let (widths, heights) = tiles.sizes();
    let rate_controlled = !matches!(ctl, Control::Cqp(_));
    let identity = VAEncWarpedMotionParamsAV1 { wmmat: [0, 0, 1 << 16, 0, 0, 1 << 16, 0, 0], ..Default::default() };
    VAEncPictureParameterBufferAV1 {
        frame_width_minus_1: u16::try_from(p.width - 1).unwrap_or(u16::MAX),
        frame_height_minus_1: u16::try_from(p.height - 1).unwrap_or(u16::MAX),
        reconstructed_frame: recon,
        coded_buf: coded,
        reference_frames: [if key { VA_INVALID_SURFACE } else { reference }; 8],
        ref_frame_idx: [0; 7],
        hierarchical_level_plus1: 0,
        primary_ref_frame: PRIMARY_REF_NONE,
        order_hint: (pic.order % (1 << ORDER_HINT_BITS)) as u8,
        refresh_frame_flags: 0xff,
        reserved8bits1: 0,
        ref_frame_ctrl_l0,
        ref_frame_ctrl_l1: 0,
        picture_flags,
        seg_id_block_size: 0,
        num_tile_groups_minus1: 0,
        temporal_id: 0,
        filter_level: [level; 2],
        filter_level_u: level,
        filter_level_v: level,
        loop_filter_flags: 0,
        superres_scale_denominator: 8,
        interpolation_filter: tools.filter,
        ref_deltas: [0; 8],
        mode_deltas: [0; 2],
        base_qindex: qindex,
        y_dc_delta_q: 0,
        u_dc_delta_q: 0,
        u_ac_delta_q: 0,
        v_dc_delta_q: 0,
        v_ac_delta_q: 0,
        min_base_qindex: 1,
        max_base_qindex: 255,
        qmatrix_flags: 0,
        reserved16bits1: 0,
        mode_control_flags,
        segments: VAEncSegParamAV1::default(),
        tile_cols: tiles.cols as u8,
        tile_rows: tiles.rows as u8,
        reserved16bits2: 0,
        width_in_sbs_minus_1: widths,
        height_in_sbs_minus_1: heights,
        context_update_tile_id: 0,
        cdef_damping_minus_3: 0,
        cdef_bits: 0,
        cdef_y_strengths: [0; 8],
        cdef_uv_strengths: [0; 8],
        loop_restoration_flags: 0,
        wm: [identity; 7],
        bit_offset_qindex: if rate_controlled { header.qindex_bit } else { 0 },
        bit_offset_segmentation: 0,
        bit_offset_loopfilter_params: if rate_controlled { header.loop_filter_bit } else { 0 },
        bit_offset_cdef_params: if rate_controlled { header.cdef_bit } else { 0 },
        size_in_bits_cdef_params: 0,
        // The OBU's size follows its one-byte header.
        byte_offset_frame_hdr_obu_size: u32::from(rate_controlled),
        size_in_bits_frame_hdr_obu: if rate_controlled { header.bits } else { 0 },
        // obu_has_size_field (bit 1): the driver's tile group OBUs carry their sizes.
        tile_group_obu_hdr_info: 0x02,
        number_skip_frames: 0,
        reserved16bits3: 0,
        skip_frames_reduced_size: 0,
        va_reserved: [0; VA_PADDING_HIGH],
    }
}

/// The one tile group of a frame: every tile of `tiles`.
pub(crate) fn av1_tile_group(tiles: &Tiles) -> VAEncTileGroupBufferAV1 {
    VAEncTileGroupBufferAV1 {
        tg_start: 0,
        tg_end: u8::try_from(tiles.count().saturating_sub(1)).unwrap_or(u8::MAX),
        va_reserved: [0; VA_PADDING_LOW],
    }
}

/// The packed header parameter buffer of a header of `bytes` bytes, of `type_`
/// ([`VAEncPackedHeaderSequence`] or [`VAEncPackedHeaderPicture`]).
pub(crate) fn packed_header(type_: u32, bytes: usize) -> VAEncPackedHeaderParameterBuffer {
    VAEncPackedHeaderParameterBuffer {
        type_,
        bit_length: u32::try_from(bytes.saturating_mul(8)).unwrap_or(u32::MAX),
        has_emulation_bytes: 0,
        va_reserved: [0; VA_PADDING_LOW],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Rate;
    use crate::av1::{self as stream, SequenceHeader};
    use craft_media::bitstream::BitReader;

    fn p(width: u32, height: u32) -> Params {
        Params { codec: crate::Codec::Av1, width, height, fps: (30, 1), rate: Rate::Quality(23), gop: 60 }
    }

    /// The tiles of a `width` × `height` frame at 30 frames per second.
    fn tiles(width: u32, height: u32) -> Tiles {
        Tiles::of(width, height, (30, 1)).unwrap()
    }

    const DEFAULT: Tools = Tools { tx_mode: TX_MODE_SELECT, filter: SWITCHABLE, tile_size_bytes: 4, obu_size_bytes: 4, max_tiles: u32::MAX };

    /// The fields of a frame header this crate writes, read from AV1 5.9 by a reader of its own,
    /// with the bit positions of `base_q_idx`, `loop_filter_params()` and the end of the trailing
    /// one bit, counted from the OBU's first byte.
    #[derive(Debug, PartialEq, Eq)]
    struct Read {
        frame_type: u32,
        order_hint: u32,
        primary_ref_frame: u32,
        refresh: u32,
        filter: Option<u32>,
        tile_log2: (u32, u32),
        base_q_idx: u32,
        levels: Vec<u32>,
        tx_mode_select: bool,
        at: (usize, usize, usize),
    }

    fn read(obu: &[u8], sb: (u32, u32)) -> Read {
        assert_eq!(obu[0], (3 << 3) | 0x02, "a frame header OBU with its size");
        let mut size = 0;
        let mut n = 1;
        loop {
            size |= usize::from(obu[n] & 0x7f) << (7 * (n - 1));
            n += 1;
            if obu[n - 1] & 0x80 == 0 {
                break;
            }
        }
        assert_eq!(obu.len(), n + size, "the size covers the payload");
        let mut b = BitReader::new(&obu[n..]);
        let mut f = |bits: u32| b.read_bits(bits).unwrap();
        assert_eq!(f(1), 0, "show_existing_frame");
        let frame_type = f(2);
        assert_eq!(f(1), 1, "show_frame");
        let key = frame_type == 0;
        if !key {
            assert_eq!(f(1), 0, "error_resilient_mode");
        }
        assert_eq!(f(1), 0, "disable_cdf_update");
        assert_eq!(f(1), 0, "frame_size_override_flag");
        let order_hint = f(8);
        let (mut primary_ref_frame, mut refresh, mut filter) = (7, 0xff, None);
        if !key {
            primary_ref_frame = f(3);
            refresh = f(8);
        }
        if key {
            assert_eq!(f(1), 0, "render_and_frame_size_different");
        } else {
            assert_eq!(f(1), 0, "frame_refs_short_signaling");
            for _ in 0..7 {
                assert_eq!(f(3), 0, "ref_frame_idx");
            }
            assert_eq!(f(1), 0, "render_and_frame_size_different");
            assert_eq!(f(1), 0, "allow_high_precision_mv");
            filter = Some(if f(1) == 1 { 4 } else { f(2) });
            assert_eq!(f(1), 0, "is_motion_mode_switchable");
        }
        assert_eq!(f(1), 0, "disable_frame_end_update_cdf");
        assert_eq!(f(1), 1, "uniform_tile_spacing_flag");
        let log2 = |target: u32| (0u32..).find(|&k| 1u32 << k >= target).unwrap_or(0);
        let (max_cols, max_rows) = (log2(sb.0.min(64)), log2(sb.1.min(64)));
        let min_cols: u32 = (0..).find(|&k| 64u32 << k >= sb.0).unwrap_or(0);
        let min_tiles = min_cols.max((0..).find(|&k| 2304u32 << k >= sb.0 * sb.1).unwrap_or(0));
        let mut cols = min_cols;
        while cols < max_cols && f(1) == 1 {
            cols += 1;
        }
        let mut rows = min_tiles.saturating_sub(cols);
        while rows < max_rows && f(1) == 1 {
            rows += 1;
        }
        if cols + rows > 0 {
            assert_eq!(f(cols + rows), 0, "context_update_tile_id");
            f(2);
        }
        let q_at = 8 * n + b.position();
        let mut f = |bits: u32| b.read_bits(bits).unwrap();
        let base_q_idx = f(8);
        for field in ["DeltaQYDc", "DeltaQUDc", "DeltaQUAc", "using_qmatrix", "segmentation_enabled", "delta_q_present"] {
            assert_eq!(f(1), 0, "{field}");
        }
        let lf_at = 8 * n + b.position();
        let mut f = |bits: u32| b.read_bits(bits).unwrap();
        let mut levels = vec![f(6), f(6)];
        if levels.iter().any(|&l| l != 0) {
            levels.extend([f(6), f(6)]);
        }
        assert_eq!(f(3), 0, "loop_filter_sharpness");
        assert_eq!(f(1), 0, "loop_filter_delta_enabled");
        let tx_mode_select = f(1) == 1;
        if !key {
            assert_eq!(f(1), 0, "reference_select");
        }
        assert_eq!(f(1), 0, "reduced_tx_set");
        if !key {
            for _ in 0..7 {
                assert_eq!(f(1), 0, "is_global");
            }
        }
        assert_eq!(f(1), 1, "trailing_one_bit");
        let end = 8 * n + b.position();
        while b.bits_left() > 0 {
            assert_eq!(b.read_bits(1).unwrap(), 0, "zero bits to the end of the OBU");
        }
        Read {
            frame_type,
            order_hint,
            primary_ref_frame,
            refresh,
            filter,
            tile_log2: (cols, rows),
            base_q_idx,
            levels,
            tx_mode_select,
            at: (q_at, lf_at, end),
        }
    }

    #[test]
    fn the_sequence_header_is_main_profile_4_2_0_at_the_level() {
        // Worked out bit by bit from AV1 5.5: profile 0, one operating point at level 2.0, 255
        // and 143 in 8-bit size fields, order hints in 8 bits and no other tools, then BT.709
        // limited range 4:2:0 and the trailing one bit.
        let obu = sequence_header(256, 144, 0);
        let payload = [0x00, 0x00, 0x00, 0x03, 0xbf, 0xfc, 0x78, 0x04, 0x38, 0x20, 0x20, 0x20, 0x20, 0x80];
        assert_eq!(obu, [&[0x0a, 14][..], &payload[..]].concat());
        let h = SequenceHeader::parse(&payload).unwrap();
        assert_eq!((h.profile, h.level_idx_0, h.tier_0, h.max_width, h.max_height, h.bit_depth), (0, 0, false, 256, 144, 8));
        assert!(!h.monochrome && h.subsampling_x && h.subsampling_y);

        // Above level 3.1 the tier is coded; 8192 wide needs 13-bit width fields.
        let big = sequence_header(8192, 4352, 19);
        let obus = stream::obus(&big).unwrap();
        let h = SequenceHeader::parse(obus[0].payload).unwrap();
        assert_eq!((h.level_idx_0, h.tier_0, h.max_width, h.max_height), (19, false, 8192, 4352));
        let s = stream::Stream::new(&big, 8).unwrap();
        assert_eq!(s.sequence_header().level_idx_0, 19);
        assert_eq!(&s.record()[..4], &[0x81, 0x13, 0x0c, 0x00], "av1C: version 1, Main profile at 6.3, 8-bit 4:2:0");
    }

    #[test]
    fn frame_headers_read_back_with_the_offsets_the_driver_rewrites() {
        let tiles = self::tiles(256, 144);
        let key = frame_header(&tiles, &DEFAULT, &Picture { idr: true, order: 0, idr_id: 0 }, Control::Cqp(23));
        // Worked out bit by bit from AV1 5.9: a shown key frame, base_q_idx 115, deblocking level
        // 19 in all four fields, TX_MODE_SELECT, with a 4-byte obu_size of 8.
        assert_eq!(key.obu, [0x1a, 0x88, 0x80, 0x80, 0x00, 0x10, 0x00, 0x8e, 0x60, 0x26, 0x9a, 0x69, 0x85]);
        let r = read(&key.obu, (4, 3));
        assert_eq!((r.frame_type, r.order_hint, r.base_q_idx, r.levels, r.tx_mode_select), (0, 0, 115, vec![19; 4], true));
        assert_eq!(r.at, (59, 73, 104));
        assert_eq!((key.qindex_bit, key.loop_filter_bit, key.cdef_bit, key.bits), (59, 73, 101, 104));
        assert_eq!(key.obu[59 / 8] << (59 % 8) | key.obu[59 / 8 + 1] >> (8 - 59 % 8), 115, "base_q_idx starts at bit 59");

        let inter = frame_header(&tiles, &DEFAULT, &Picture { idr: false, order: 1, idr_id: 0 }, Control::Cqp(23));
        assert_eq!(inter.obu, [0x1a, 0x8e, 0x80, 0x80, 0x00, 0x30, 0x03, 0xff, 0xc0, 0x00, 0x00, 0x24, 0x73, 0x01, 0x34, 0xd3, 0x4c, 0x20, 0x08]);
        let r = read(&inter.obu, (4, 3));
        assert_eq!((r.frame_type, r.order_hint, r.primary_ref_frame, r.refresh, r.filter), (1, 1, 7, 0xff, Some(4)));
        assert_eq!(r.at, (96, 110, 149));
        assert_eq!((inter.qindex_bit, inter.loop_filter_bit, inter.cdef_bit, inter.bits), (96, 110, 138, 149));

        // Four tiles, one filter, TX_MODE_LARGEST, 2-byte OBU sizes, rate control and a wrapped
        // order hint.
        let tools = Tools { tx_mode: TX_MODE_LARGEST, filter: 1, tile_size_bytes: 2, obu_size_bytes: 2, max_tiles: 64 };
        let tiles = self::tiles(7680, 4320);
        let h = frame_header(&tiles, &tools, &Picture { idr: false, order: 300, idr_id: 0 }, Control::Cbr(20_000_000));
        let r = read(&h.obu, (120, 68));
        assert_eq!((r.order_hint, r.filter, r.tile_log2, r.base_q_idx, r.levels, r.tx_mode_select), (44, Some(1), (1, 1), 130, vec![22; 4], false));
        assert_eq!(r.at, (h.qindex_bit as usize, h.loop_filter_bit as usize, h.bits as usize));
        assert_eq!(h.obu.len(), 3 + (h.bits as usize - 24).div_ceil(8));

        // Past the syntax's fewest tiles, the increment bits step up to the grid the session uses.
        for (width, height, fps, log2) in [(7680, 4320, (120, 1), (1, 2)), (4032, 4672, (30, 1), (0, 2)), (3840, 2160, (120, 1), (0, 1))] {
            let tiles = Tiles::of(width, height, fps).unwrap();
            let h = frame_header(&tiles, &DEFAULT, &Picture { idr: true, order: 0, idr_id: 0 }, Control::Cqp(23));
            let r = read(&h.obu, (tiles.sb_cols, tiles.sb_rows));
            assert_eq!((r.tile_log2, (u32::from(tiles.cols_log2), u32::from(tiles.rows_log2))), (log2, log2), "{width}x{height} at {fps:?}");
            assert_eq!(r.at, (h.qindex_bit as usize, h.loop_filter_bit as usize, h.bits as usize));
        }
    }

    #[test]
    fn levels_take_the_picture_size_rate_and_bitrate() {
        assert_eq!(level(256, 144, (30, 1), 0), 0, "2.0");
        assert_eq!(level(1280, 720, (30, 1), 0), 5, "3.1");
        assert_eq!(level(1920, 1080, (60, 1), 0), 9, "4.1");
        assert_eq!(level(3840, 2160, (60, 1), 0), 13, "5.1");
        assert_eq!(level(1920, 1080, (30, 1), 50_000_000), 14, "1080p30 at 50 Mbit/s needs 5.2's Main tier bitrate");
        assert_eq!(level(6144, 256, (30, 1), 0), 8, "4.0's widest picture");
        assert_eq!(level(6146, 256, (30, 1), 0), 12, "past 4.0's MaxHSize");
        assert_eq!(level(8192, 4352, (120, 1), 0), 18);
        assert_eq!(level(8192, 4352, (120, 1), u32::MAX), 19, "no level takes the bitrate: 6.3");
        assert!(fits_a_level(8192, 4352, (120, 1)));
        assert!(!fits_a_level(8192, 4352, (121, 1)));
        assert!(!fits_a_level(8192, 8192, (1, 1)));
        assert!(!fits_a_level(16386, 2, (1, 1)), "past MaxHSize");
        assert!(fits_a_level(7680, 4320, (120, 1)) && !fits_a_level(7680, 4320, (130, 1)));
        // Levels 2.0 to 3.1 take 150 frame headers a second and the others 300.
        assert_eq!(level(160, 120, (30, 1), 0), 0, "2.0");
        assert_eq!(level(160, 120, (150, 1), 0), 0, "150 frame headers a second");
        assert_eq!(level(160, 120, (200, 1), 0), 8, "200 frame headers a second need 4.0");
        assert_eq!(level(160, 120, (300_001, 2000), 0), 8, "a rate just above 150");
        assert!(fits_a_level(160, 120, (300, 1)) && !fits_a_level(160, 120, (301, 1)));
        assert!(!fits_a_level(160, 120, (300_001, 1000)), "a rate just above 300");
        assert_eq!(super::super::nominal_bitrate(&Params { width: 4096, height: 4096, fps: (240, 1), ..p(4096, 4096) }), MAX_MAIN_BPS);
    }

    #[test]
    fn tiles_are_uniform_and_as_few_as_the_size_allows() {
        let one = tiles(1920, 1080);
        assert_eq!((one.sb_cols, one.sb_rows, one.cols, one.rows, one.max_cols_log2, one.max_rows_log2), (30, 17, 1, 1, 5, 5));
        let t = tiles(4096, 2304);
        assert_eq!((t.cols, t.rows), (1, 1), "MAX_TILE_WIDTH and MAX_TILE_AREA exactly");
        let wide = tiles(4160, 64);
        assert_eq!((wide.cols_log2, wide.cols, wide.width_sb, wide.rows), (1, 2, 33, 1));
        let tall = tiles(4096, 2368);
        assert_eq!((tall.cols, tall.rows_log2, tall.rows), (1, 1, 2), "past MAX_TILE_AREA the frame splits in rows");
        let t = tiles(7680, 4320);
        assert_eq!((t.cols_log2, t.rows_log2, t.cols, t.rows, t.width_sb, t.height_sb, t.count()), (1, 1, 2, 2, 60, 34, 4));
        let (w, h) = t.sizes();
        assert_eq!((&w[..3], &h[..3]), (&[59, 59, 0][..], &[33, 33, 0][..]));
        let (w, _) = tiles(4160, 64).sizes();
        assert_eq!(&w[..3], &[32, 31, 0], "the last column takes the rest");
        let tiny = tiles(2, 2);
        assert_eq!((tiny.sb_cols, tiny.sb_rows, tiny.count(), tiny.max_cols_log2), (1, 1, 1, 0));

        // Two rows of 37 superblocks make 4032 × 2368 tiles, past MAX_TILE_AREA: four rows of 19.
        let rounded = tiles(4032, 4672);
        assert_eq!((rounded.cols_log2, rounded.rows_log2, rounded.min_rows_log2, rounded.height_sb, rounded.count()), (0, 2, 1, 19, 4));
        // Two rows of 63 × 37 superblocks hold 3984 × 2368 = 9,434,112 samples, inside
        // MAX_TILE_AREA, but 2,331 superblocks, past the 2,304 a uniform tile may span: four rows.
        let cropped = tiles(3984, 4672);
        assert_eq!((cropped.cols_log2, cropped.rows_log2, cropped.width_sb, cropped.height_sb, cropped.count()), (0, 2, 63, 19, 4));
        // At 120 frames per second a tile codes at most 4,902,092 luma samples a frame.
        assert_eq!(Tiles::of(3840, 2160, (60, 1)).map(|t| t.count()), Some(1), "8,294,400 samples at 60 frames per second");
        let fast = Tiles::of(3840, 2160, (120, 1)).unwrap();
        assert_eq!((fast.cols_log2, fast.rows_log2, fast.height_sb, fast.count()), (0, 1, 17, 2));
        let big = Tiles::of(7680, 4320, (120, 1)).unwrap();
        assert_eq!((big.cols_log2, big.rows_log2, big.width_sb, big.height_sb, big.count()), (1, 2, 60, 17, 8));
        assert_eq!(Tiles::of(7680, 4320, (60, 1)).map(|t| t.count()), Some(4), "3840 × 2176 at 60 frames per second fits");
        assert_eq!(Tiles::of(16, 16, (u32::MAX, 1)), None, "one superblock coded faster than any tile may be");
        // A tile's size counts whole 4 × 4 blocks: 3826 pixels code as 3832, and two rows of
        // 3832 × 1280 at 120 frames per second pass the rate limit.
        let blocks = Tiles::of(3826, 2554, (120, 1)).unwrap();
        assert_eq!((blocks.cols_log2, blocks.rows_log2, blocks.height_sb, blocks.count()), (0, 2, 10, 4));
        // Eight rows of 7 superblocks leave the bottom row 2 pixels of the frame: two columns of
        // four rows instead.
        let edge = Tiles::of(4096, 3138, (240, 1)).unwrap();
        assert_eq!((edge.cols_log2, edge.rows_log2, edge.width_sb, edge.height_sb, edge.count()), (1, 2, 32, 13, 8));
    }

    #[test]
    fn the_drivers_attributes_choose_the_tools_or_refuse_the_encoder() {
        let none = VA_ATTRIB_NOT_SUPPORTED;
        assert_eq!(tools(none, none, none), Ok(Tools { filter: 0, ..DEFAULT }));
        assert_eq!(tools(0x0555_5555, 0x11, none).map(|t| t.filter), Ok(SWITCHABLE), "every tool optional");
        assert_eq!(tools(0, 0x02, none).map(|t| t.filter), Ok(1));
        assert_eq!(tools(0, 0, none).map(|t| t.filter), Ok(0), "no filter listed: EIGHTTAP");
        for tool in 0..14 {
            assert!(tools(VA_FEATURE_REQUIRED << (2 * tool), none, none).is_err(), "tool {tool} required");
        }
        let ext2 = 3 | (1 << 2) | (2 << 4) | (7 << 7);
        assert_eq!(
            tools(0, 0x10, ext2),
            Ok(Tools { tx_mode: TX_MODE_LARGEST, filter: SWITCHABLE, tile_size_bytes: 4, obu_size_bytes: 2, max_tiles: 8 })
        );
        assert_eq!(tools(0, 0, 4 << 4).map(|t| t.tx_mode), Ok(TX_MODE_SELECT));
        assert_eq!(tools(0, 0, 6 << 4).map(|t| t.tx_mode), Ok(TX_MODE_SELECT), "SELECT before LARGEST");
        assert_eq!(tools(0, 0, 0).map(|t| (t.tx_mode, t.tile_size_bytes, t.max_tiles)), Ok((TX_MODE_SELECT, 1, 1)), "no transform modes listed");
        assert!(tools(0, 0, 1 << 4).is_err(), "4 × 4 transforms only");
    }

    #[test]
    fn av1_parameters_describe_ip_frames_that_refer_to_the_frame_before() {
        let params = p(1920, 1080);
        let seq = av1_sequence(&params, 9, Control::Cqp(23));
        assert_eq!((seq.seq_profile, seq.seq_level_idx, seq.seq_tier, seq.intra_period, seq.ip_period, seq.bits_per_second), (0, 9, 0, 60, 1, 0));
        assert_eq!((seq.seq_fields, seq.order_hint_bits_minus_1), (1 << 8 | 1 << 17 | 1 << 18, 7), "order hints, 4:2:0");
        assert_eq!(av1_sequence(&params, 9, Control::Cbr(6_000_000)).bits_per_second, 6_000_000);

        let tiles = self::tiles(1920, 1080);
        let key_pic = Picture { idr: true, order: 0, idr_id: 0 };
        let header = frame_header(&tiles, &DEFAULT, &key_pic, Control::Cqp(23));
        let key = av1_picture(&params, &tiles, &DEFAULT, &key_pic, Control::Cqp(23), &header, 10, 9, 5);
        assert_eq!((key.frame_width_minus_1, key.frame_height_minus_1, key.reconstructed_frame, key.coded_buf), (1919, 1079, 10, 5));
        assert!(key.reference_frames.iter().all(|&r| r == VA_INVALID_SURFACE));
        assert_eq!((key.picture_flags, key.ref_frame_ctrl_l0, key.refresh_frame_flags, key.primary_ref_frame), (1 << 2 | 1 << 15, 0, 0xff, 7));
        assert_eq!((key.base_qindex, key.filter_level, key.filter_level_u, key.filter_level_v), (115, [19, 19], 19, 19));
        assert_eq!((key.mode_control_flags, key.interpolation_filter, key.tile_cols, key.tile_rows), (2 << 7, SWITCHABLE, 1, 1));
        assert_eq!((key.width_in_sbs_minus_1[0], key.height_in_sbs_minus_1[0], key.superres_scale_denominator), (29, 16, 8));
        assert_eq!(
            (key.bit_offset_qindex, key.byte_offset_frame_hdr_obu_size, key.size_in_bits_frame_hdr_obu),
            (0, 0, 0),
            "no offsets under constant QP"
        );
        assert_eq!((key.wm[0].wmmat[2], key.wm[6].wmmat[5], key.tile_group_obu_hdr_info), (1 << 16, 1 << 16, 0x02));

        let inter_pic = Picture { idr: false, order: 3, idr_id: 0 };
        let header = frame_header(&tiles, &DEFAULT, &inter_pic, Control::Cbr(6_000_000));
        let inter = av1_picture(&params, &tiles, &DEFAULT, &inter_pic, Control::Cbr(6_000_000), &header, 11, 10, 6);
        assert!(inter.reference_frames.iter().all(|&r| r == 10), "every slot holds the frame before");
        assert_eq!((inter.picture_flags, inter.ref_frame_ctrl_l0, inter.order_hint, inter.base_qindex, inter.filter_level), (1, 1, 3, 130, [22, 22]));
        assert_eq!(
            (inter.bit_offset_qindex, inter.bit_offset_loopfilter_params, inter.bit_offset_cdef_params, inter.size_in_bits_cdef_params),
            (header.qindex_bit, header.loop_filter_bit, header.cdef_bit, 0)
        );
        assert_eq!((inter.byte_offset_frame_hdr_obu_size, inter.size_in_bits_frame_hdr_obu), (1, header.bits));

        let t = self::tiles(7680, 4320);
        assert_eq!((av1_tile_group(&t).tg_start, av1_tile_group(&t).tg_end), (0, 3));
        assert_eq!(packed_header(VAEncPackedHeaderPicture, 13), VAEncPackedHeaderParameterBuffer { type_: 2, bit_length: 104, ..Default::default() });
        assert_eq!((base_qindex(Control::Cqp(51)), base_qindex(Control::Cqp(1)), filter_level(255), filter_level(2)), (255, 5, 43, 0));
    }
}
