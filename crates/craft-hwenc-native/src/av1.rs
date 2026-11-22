//! AV1 streams in the low overhead bitstream format, where every OBU carries its size (AV1
//! Bitstream and Decoding Process Specification 5.2), and the `av1C` record (AV1 Codec ISO Media
//! File Format Binding 2.3, `AV1CodecConfigurationRecord`). Matroska stores the record as the
//! track's `CodecPrivate` (`V_AV1`) and MP4 as the `av1C` box. `craft-media`'s [`Av1Config`]
//! writes the record.
//!
//! - [`obus`] splits a temporal unit into its OBUs.
//! - [`SequenceHeader`] reads the fields of a sequence header OBU (AV1 5.5) that the record repeats.
//! - [`Stream`] holds a stream's sequence header and record, and turns each temporal unit an
//!   encoder writes into the sample a container stores: the temporal delimiters, padding and tile
//!   list OBUs are left out, every OBU carries its size, and a sequence header must be the one the
//!   record holds. A sample is a keyframe when its first frame is a key frame that is shown.

use craft_media::bitstream::BitReader;
use craft_media::isobmff::Av1Config;

/// OBU types (AV1 6.2.2).
pub const OBU_SEQUENCE_HEADER: u8 = 1;
pub const OBU_TEMPORAL_DELIMITER: u8 = 2;
pub const OBU_FRAME_HEADER: u8 = 3;
pub const OBU_FRAME: u8 = 6;
pub const OBU_TILE_LIST: u8 = 8;
pub const OBU_PADDING: u8 = 15;

/// The largest `obu_size` a stream may carry: 2^32 - 1 (AV1 4.10.5).
const MAX_OBU_SIZE: u64 = u32::MAX as u64;

/// One OBU of a temporal unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Obu<'a> {
    /// `obu_type`.
    pub kind: u8,
    /// The header byte and the extension byte when there is one, as written, without the size.
    pub header: &'a [u8],
    pub payload: &'a [u8],
}

impl Obu<'_> {
    /// Append the OBU to `out` with `obu_has_size_field` set and its size after the header.
    fn write(&self, out: &mut Vec<u8>) {
        if let Some((&first, extension)) = self.header.split_first() {
            out.push(first | 0x02);
            out.extend_from_slice(extension);
        }
        leb128(self.payload.len() as u64, out);
        out.extend_from_slice(self.payload);
    }
}

/// The `leb128()` value at the start of `d` (AV1 4.10.5) and the bytes it takes: at most 8 bytes
/// and at most 2^32 - 1.
fn read_leb128(d: &[u8]) -> Result<(u64, usize), String> {
    let mut value = 0u64;
    for i in 0..8 {
        let &b = d.get(i).ok_or("an AV1 OBU ends inside its size")?;
        value |= u64::from(b & 0x7f) << (7 * i);
        if b & 0x80 == 0 {
            return if value <= MAX_OBU_SIZE { Ok((value, i + 1)) } else { Err("an AV1 OBU size is above 2^32 - 1".into()) };
        }
    }
    Err("an AV1 OBU size is longer than 8 bytes".into())
}

/// Append `v` to `out` as `leb128()` in the fewest bytes.
fn leb128(mut v: u64, out: &mut Vec<u8>) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

/// The OBUs of `data` (AV1 5.3). An OBU without a size runs to the end of `data`.
pub fn obus(data: &[u8]) -> Result<Vec<Obu<'_>>, String> {
    let mut out = Vec::new();
    let mut rest = data;
    while let Some(&h) = rest.first() {
        if h & 0x80 != 0 {
            return Err("an AV1 OBU has its forbidden bit set".into());
        }
        let head = 1 + usize::from(h & 0x04 != 0);
        let header = rest.get(..head).ok_or("an AV1 OBU ends inside its header")?;
        let (start, size) = if h & 0x02 != 0 {
            let (size, n) = read_leb128(rest.get(head..).unwrap_or_default())?;
            (head + n, usize::try_from(size).map_err(|_| "an AV1 OBU is larger than memory".to_string())?)
        } else {
            (head, rest.len() - head)
        };
        let end = start.checked_add(size).filter(|&e| e <= rest.len()).ok_or("an AV1 OBU runs past the end of its data")?;
        let payload = rest.get(start..end).ok_or("an AV1 OBU runs past the end of its data")?;
        out.push(Obu { kind: (h >> 3) & 0x0f, header, payload });
        rest = rest.get(end..).unwrap_or_default();
    }
    Ok(out)
}

/// The fields of an AV1 sequence header (AV1 5.5) that the record repeats or that reading a frame
/// header needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SequenceHeader {
    /// `seq_profile`: 0 is Main, 1 High, 2 Professional.
    pub profile: u8,
    pub reduced_still_picture_header: bool,
    /// `seq_level_idx` and `seq_tier` of operating point 0.
    pub level_idx_0: u8,
    pub tier_0: bool,
    pub max_width: u32,
    pub max_height: u32,
    /// 8, 10 or 12.
    pub bit_depth: u8,
    pub monochrome: bool,
    pub subsampling_x: bool,
    pub subsampling_y: bool,
    pub chroma_sample_position: u8,
}

impl SequenceHeader {
    /// Parse the payload of a sequence header OBU.
    pub fn parse(payload: &[u8]) -> Result<SequenceHeader, String> {
        let s = Self::read(&mut BitReader::new(payload)).map_err(|_| "the AV1 sequence header is truncated".to_string())?;
        if s.profile > 2 {
            return Err(format!("the AV1 sequence header has the reserved profile {}", s.profile));
        }
        Ok(s)
    }

    fn read(b: &mut BitReader) -> craft_media::bitstream::Result<SequenceHeader> {
        let profile = b.read_bits(3)? as u8;
        let _still_picture = b.read_bit()?;
        let reduced = b.read_bit()?;
        let (mut level_idx_0, mut tier_0) = (0, false);
        if reduced {
            level_idx_0 = b.read_bits(5)? as u8;
        } else {
            let mut decoder_model = false;
            let mut buffer_delay_bits = 0usize;
            if b.read_bit()? {
                // timing_info: num_units_in_display_tick, time_scale, equal_picture_interval
                b.skip(64)?;
                if b.read_bit()? {
                    uvlc(b)?;
                }
                decoder_model = b.read_bit()?;
                if decoder_model {
                    // decoder_model_info: buffer_delay_length_minus_1, num_units_in_decoding_tick,
                    // buffer_removal_time_length_minus_1, frame_presentation_time_length_minus_1
                    buffer_delay_bits = b.read_bits(5)? as usize + 1;
                    b.skip(32 + 5 + 5)?;
                }
            }
            let display_delay = b.read_bit()?;
            let points = b.read_bits(5)? + 1;
            for i in 0..points {
                let _operating_point_idc = b.read_bits(12)?;
                let level = b.read_bits(5)? as u8;
                let tier = level > 7 && b.read_bit()?;
                if i == 0 {
                    (level_idx_0, tier_0) = (level, tier);
                }
                // operating_parameters_info: decoder_buffer_delay, encoder_buffer_delay,
                // low_delay_mode_flag
                if decoder_model && b.read_bit()? {
                    b.skip(2 * buffer_delay_bits + 1)?;
                }
                // initial_display_delay_minus_1
                if display_delay && b.read_bit()? {
                    b.skip(4)?;
                }
            }
        }
        let width_bits = b.read_bits(4)? + 1;
        let height_bits = b.read_bits(4)? + 1;
        let max_width = b.read_bits(width_bits)? + 1;
        let max_height = b.read_bits(height_bits)? + 1;
        // frame_id_numbers_present_flag: delta_frame_id_length_minus_2, additional_frame_id_length_minus_1
        if !reduced && b.read_bit()? {
            b.skip(4 + 3)?;
        }
        // use_128x128_superblock, enable_filter_intra, enable_intra_edge_filter
        b.skip(3)?;
        if !reduced {
            // enable_interintra_compound, enable_masked_compound, enable_warped_motion, enable_dual_filter
            b.skip(4)?;
            let order_hint = b.read_bit()?;
            if order_hint {
                // enable_jnt_comp, enable_ref_frame_mvs
                b.skip(2)?;
            }
            // seq_choose_screen_content_tools, else seq_force_screen_content_tools
            let screen_content_tools = b.read_bit()? || b.read_bit()?;
            // seq_choose_integer_mv, else seq_force_integer_mv
            if screen_content_tools && !b.read_bit()? {
                b.skip(1)?;
            }
            if order_hint {
                // order_hint_bits_minus_1
                b.skip(3)?;
            }
        }
        // enable_superres, enable_cdef, enable_restoration
        b.skip(3)?;
        // color_config (5.5.2)
        let high_bitdepth = b.read_bit()?;
        let bit_depth = match (profile, high_bitdepth) {
            (2, true) => {
                if b.read_bit()? {
                    12
                } else {
                    10
                }
            }
            (_, true) => 10,
            (_, false) => 8,
        };
        let monochrome = profile != 1 && b.read_bit()?;
        let (mut primaries, mut transfer, mut matrix) = (2, 2, 2);
        if b.read_bit()? {
            (primaries, transfer, matrix) = (b.read_bits(8)?, b.read_bits(8)?, b.read_bits(8)?);
        }
        let (mut subsampling_x, mut subsampling_y, mut chroma_sample_position) = (true, true, 0);
        if monochrome {
            // color_range
            b.skip(1)?;
        } else {
            // BT.709 primaries, sRGB transfer and identity matrix: 4:4:4 at full range
            if primaries == 1 && transfer == 13 && matrix == 0 {
                (subsampling_x, subsampling_y) = (false, false);
            } else {
                // color_range
                b.skip(1)?;
                (subsampling_x, subsampling_y) = match profile {
                    0 => (true, true),
                    1 => (false, false),
                    _ if bit_depth == 12 => {
                        let x = b.read_bit()?;
                        (x, x && b.read_bit()?)
                    }
                    _ => (true, false),
                };
                if subsampling_x && subsampling_y {
                    chroma_sample_position = b.read_bits(2)? as u8;
                }
            }
            // separate_uv_delta_q
            b.skip(1)?;
        }
        // film_grain_params_present
        b.skip(1)?;
        Ok(SequenceHeader {
            profile,
            reduced_still_picture_header: reduced,
            level_idx_0,
            tier_0,
            max_width,
            max_height,
            bit_depth,
            monochrome,
            subsampling_x,
            subsampling_y,
            chroma_sample_position,
        })
    }
}

/// `uvlc()` (AV1 4.10.3).
fn uvlc(b: &mut BitReader) -> craft_media::bitstream::Result<u32> {
    let mut zeros = 0u32;
    while !b.read_bit()? {
        zeros = zeros.saturating_add(1);
    }
    if zeros >= 32 {
        return Ok(u32::MAX);
    }
    if zeros == 0 {
        return Ok(0);
    }
    Ok(b.read_bits(zeros)?.saturating_add((1 << zeros) - 1))
}

/// One AV1 stream: its sequence header, the `av1C` record made of it, and the samples of its
/// temporal units.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stream {
    header: SequenceHeader,
    /// The sequence header OBU's payload.
    payload: Vec<u8>,
    record: Vec<u8>,
}

impl Stream {
    /// The stream of the first sequence header in the OBUs `data`, for example the encoder's
    /// sequence parameters or its first temporal unit. The stream must be Main profile 4:2:0 at
    /// `depth` bits, which is what the encoders of this crate are asked for. Every other sequence
    /// header in `data` must be the same.
    pub fn new(data: &[u8], depth: u8) -> Result<Stream, String> {
        let all = obus(data)?;
        let mut headers = all.iter().filter(|o| o.kind == OBU_SEQUENCE_HEADER);
        let seq = headers.next().ok_or("the encoder returned no AV1 sequence header")?;
        if headers.any(|o| o.payload != seq.payload) {
            return Err("the encoder returned two different AV1 sequence headers".into());
        }
        let h = SequenceHeader::parse(seq.payload)?;
        if h.profile != 0 || h.monochrome || !h.subsampling_x || !h.subsampling_y || h.bit_depth != depth {
            let chroma = match (h.monochrome, h.subsampling_x, h.subsampling_y) {
                (true, ..) => "monochrome",
                (false, true, true) => "4:2:0",
                (false, true, false) => "4:2:2",
                (false, false, _) => "4:4:4",
            };
            return Err(format!("the encoder wrote AV1 profile {} {chroma} at {} bits, not Main 4:2:0 at {depth} bits", h.profile, h.bit_depth));
        }
        let mut config_obus = Vec::with_capacity(seq.payload.len() + 6);
        seq.write(&mut config_obus);
        let record = Av1Config {
            seq_profile: h.profile,
            seq_level_idx_0: h.level_idx_0,
            seq_tier_0: h.tier_0,
            high_bitdepth: h.bit_depth > 8,
            twelve_bit: h.bit_depth == 12,
            monochrome: h.monochrome,
            chroma_subsampling_x: h.subsampling_x,
            chroma_subsampling_y: h.subsampling_y,
            chroma_sample_position: h.chroma_sample_position,
            initial_presentation_delay_minus_one: None,
            config_obus,
        }
        .to_bytes();
        Ok(Stream { header: h, payload: seq.payload.to_vec(), record })
    }

    /// The `av1C` record a container stores with the track: its configuration OBUs are the
    /// sequence header.
    pub fn record(&self) -> &[u8] {
        &self.record
    }

    /// The stream's sequence header.
    pub fn sequence_header(&self) -> &SequenceHeader {
        &self.header
    }

    /// Whether `other` has the same sequence header, byte for byte.
    pub fn same_sequence_header(&self, other: &Stream) -> bool {
        self.payload == other.payload
    }

    /// The sample of the temporal unit `data` and whether a decoder can start at it. A sequence
    /// header other than the stream's is an error, and so is a temporal unit without a frame.
    pub fn sample(&self, data: &[u8]) -> Result<(Vec<u8>, bool), String> {
        let mut out = Vec::with_capacity(data.len().saturating_add(8));
        let mut keyframe = None;
        for o in obus(data)? {
            match o.kind {
                OBU_TEMPORAL_DELIMITER | OBU_PADDING | OBU_TILE_LIST => continue,
                OBU_SEQUENCE_HEADER if o.payload != self.payload => return Err("the encoder changed its sequence header".into()),
                OBU_FRAME_HEADER | OBU_FRAME if keyframe.is_none() => keyframe = Some(self.shown_key_frame(o.payload)?),
                _ => {}
            }
            o.write(&mut out);
        }
        let keyframe = keyframe.ok_or("the encoder produced no picture data")?;
        Ok((out, keyframe))
    }

    /// Whether the frame header at the start of `payload` (AV1 5.9.2) is a key frame that is shown.
    /// A frame that shows an earlier frame again is not one.
    fn shown_key_frame(&self, payload: &[u8]) -> Result<bool, String> {
        if self.header.reduced_still_picture_header {
            return Ok(true);
        }
        let mut b = BitReader::new(payload);
        let read = |b: &mut BitReader| -> craft_media::bitstream::Result<bool> {
            if b.read_bit()? {
                return Ok(false);
            }
            let frame_type = b.read_bits(2)?;
            Ok(frame_type == 0 && b.read_bit()?)
        };
        read(&mut b).map_err(|_| "an AV1 frame header is truncated".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use craft_media::bitstream::BitWriter;

    /// An OBU of `kind` with its size, for payloads under 128 bytes.
    fn obu(kind: u8, payload: &[u8]) -> Vec<u8> {
        [&[(kind << 3) | 0x02, payload.len() as u8][..], payload].concat()
    }

    /// A sequence header payload worked out bit by bit from AV1 5.5: profile 0, not a still
    /// picture, one operating point at level 4 (3.0), 256 × 144 in 8-bit frame size fields, no
    /// optional tools, 8-bit 4:2:0 without a color description, then the trailing one bit.
    const SEQ: [u8; 10] = [0x00, 0x00, 0x00, 0x23, 0xbf, 0xfc, 0x78, 0x00, 0x00, 0x10];

    /// A sequence header with more of its optional fields: timing and decoder model information,
    /// two operating points with their parameters and display delays, every tool, a color
    /// description and the chosen chroma sample position.
    fn rich(profile: u32, ten_bit: bool, level: u32) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.write_bits(profile, 3);
        w.write_bits(0b00, 2); // still_picture, reduced_still_picture_header
        w.write_bit(true); // timing_info_present_flag
        w.write_bits(1001, 32);
        w.write_bits(60000, 32);
        w.write_bit(true); // equal_picture_interval
        w.write_bits(0b00100, 5); // uvlc 3
        w.write_bit(true); // decoder_model_info_present_flag
        w.write_bits(9, 5); // buffer_delay_length_minus_1: 10-bit delays
        w.write_bits(90000, 32);
        w.write_bits(0, 10);
        w.write_bit(true); // initial_display_delay_present_flag
        w.write_bits(1, 5); // two operating points
        for (i, lv) in [level, 4].into_iter().enumerate() {
            w.write_bits(if i == 0 { 0x101 } else { 0x103 }, 12);
            w.write_bits(lv, 5);
            if lv > 7 {
                w.write_bit(true); // seq_tier
            }
            w.write_bit(true); // decoder_model_present_for_this_op
            w.write_bits(0x2aa, 10);
            w.write_bits(0x155, 10);
            w.write_bit(false);
            w.write_bit(true); // initial_display_delay_present_for_this_op
            w.write_bits(9, 4);
        }
        w.write_bits(10, 4); // frame_width_bits_minus_1
        w.write_bits(10, 4);
        w.write_bits(1919, 11);
        w.write_bits(1079, 11);
        w.write_bit(true); // frame_id_numbers_present_flag
        w.write_bits(0b111_1111, 7);
        w.write_bits(0b111, 3);
        w.write_bits(0b1111, 4);
        w.write_bit(true); // enable_order_hint
        w.write_bits(0b11, 2);
        w.write_bit(false); // seq_choose_screen_content_tools
        w.write_bit(true); // seq_force_screen_content_tools
        w.write_bit(false); // seq_choose_integer_mv
        w.write_bit(true); // seq_force_integer_mv
        w.write_bits(6, 3); // order_hint_bits_minus_1
        w.write_bits(0b111, 3);
        w.write_bit(ten_bit); // high_bitdepth
        if profile == 2 && ten_bit {
            w.write_bit(false); // twelve_bit
        }
        if profile != 1 {
            w.write_bit(false); // mono_chrome
        }
        w.write_bit(true); // color_description_present_flag
        w.write_bits(9, 8);
        w.write_bits(16, 8);
        w.write_bits(9, 8);
        w.write_bit(false); // color_range
        if profile == 0 {
            w.write_bits(1, 2); // chroma_sample_position: vertical
        }
        w.write_bit(false); // separate_uv_delta_q
        w.write_bit(false); // film_grain_params_present
        w.rbsp_trailing();
        w.finish()
    }

    /// A reduced still picture header: profile 0 at level 5, 64 × 64, 8-bit 4:2:0.
    fn still() -> Vec<u8> {
        let mut w = BitWriter::new();
        w.write_bits(0, 3);
        w.write_bits(0b11, 2);
        w.write_bits(5, 5);
        w.write_bits(5, 4);
        w.write_bits(5, 4);
        w.write_bits(63, 6);
        w.write_bits(63, 6);
        w.write_bits(0, 3 + 3 + 1 + 1 + 1 + 1 + 2 + 1 + 1);
        w.rbsp_trailing();
        w.finish()
    }

    /// Frame header starts: show_existing_frame, frame_type (2 bits), show_frame.
    const KEY: u8 = 0b0001_0000;
    const INTER: u8 = 0b0011_0000;
    const HIDDEN_KEY: u8 = 0b0000_0000;
    const SHOW_EXISTING: u8 = 0b1000_0000;

    #[test]
    fn a_sequence_header_worked_out_by_hand_reads_back() {
        let h = SequenceHeader::parse(&SEQ).unwrap();
        assert_eq!(
            h,
            SequenceHeader {
                profile: 0,
                reduced_still_picture_header: false,
                level_idx_0: 4,
                tier_0: false,
                max_width: 256,
                max_height: 144,
                bit_depth: 8,
                monochrome: false,
                subsampling_x: true,
                subsampling_y: true,
                chroma_sample_position: 0,
            }
        );
    }

    #[test]
    fn the_record_follows_the_av1c_layout_with_the_sequence_header_as_its_configuration() {
        let s = Stream::new(&obu(OBU_SEQUENCE_HEADER, &SEQ), 8).unwrap();
        // marker and version 1; profile 0, level 4; tier, depths, monochrome 0, 4:2:0, position 0;
        // no presentation delay; then the OBU: type 1 with its size
        let expected = [&[0x81, 0x04, 0x0c, 0x00, 0x0a, 0x0a][..], &SEQ].concat();
        assert_eq!(s.record(), expected);
        assert_eq!(Av1Config::parse(s.record()).unwrap().config_obus, obu(OBU_SEQUENCE_HEADER, &SEQ));
    }

    #[test]
    fn optional_sequence_fields_are_skipped_and_operating_point_0_names_the_level() {
        let h = SequenceHeader::parse(&rich(0, true, 13)).unwrap();
        assert_eq!((h.level_idx_0, h.tier_0, h.bit_depth, h.max_width, h.max_height), (13, true, 10, 1920, 1080));
        assert_eq!((h.subsampling_x, h.subsampling_y, h.chroma_sample_position), (true, true, 1));
        let h = SequenceHeader::parse(&rich(0, false, 7)).unwrap();
        assert_eq!((h.level_idx_0, h.tier_0, h.bit_depth), (7, false, 8), "no tier bit at level 7 and below");
        let s = Stream::new(&obu(OBU_SEQUENCE_HEADER, &rich(0, true, 13)), 10).unwrap();
        assert_eq!(&s.record()[..4], &[0x81, 13, 0x80 | 0x40 | 0x0c | 1, 0]);
        let h = SequenceHeader::parse(&still()).unwrap();
        assert!(h.reduced_still_picture_header);
        assert_eq!((h.level_idx_0, h.max_width, h.max_height, h.bit_depth), (5, 64, 64, 8));
    }

    #[test]
    fn streams_other_than_main_4_2_0_at_the_asked_depth_are_refused() {
        let ten = obu(OBU_SEQUENCE_HEADER, &rich(0, true, 8));
        assert!(Stream::new(&ten, 10).is_ok());
        let e = Stream::new(&ten, 8).unwrap_err();
        assert!(e.contains("Main 4:2:0 at 8 bits"), "{e}");
        let high = obu(OBU_SEQUENCE_HEADER, &rich(1, false, 8));
        assert!(Stream::new(&high, 8).unwrap_err().contains("profile 1 4:4:4"));
        let pro = obu(OBU_SEQUENCE_HEADER, &rich(2, true, 8));
        assert!(Stream::new(&pro, 10).unwrap_err().contains("profile 2 4:2:2"));
        let mut reserved = SEQ;
        reserved[0] |= 0xe0;
        assert!(SequenceHeader::parse(&reserved).unwrap_err().contains("reserved profile 7"));
    }

    #[test]
    fn the_sequence_header_is_found_among_other_obus_and_must_be_the_only_one() {
        let tu = [obu(OBU_TEMPORAL_DELIMITER, &[]), obu(OBU_SEQUENCE_HEADER, &SEQ), obu(OBU_FRAME, &[KEY, 1, 2])].concat();
        assert_eq!(Stream::new(&tu, 8).unwrap().record().len(), 4 + 2 + SEQ.len());
        let twice = [obu(OBU_SEQUENCE_HEADER, &SEQ), obu(OBU_SEQUENCE_HEADER, &SEQ)].concat();
        assert!(Stream::new(&twice, 8).is_ok());
        let two = [obu(OBU_SEQUENCE_HEADER, &SEQ), obu(OBU_SEQUENCE_HEADER, &rich(0, false, 8))].concat();
        assert!(Stream::new(&two, 8).unwrap_err().contains("two different"));
        assert!(Stream::new(&obu(OBU_FRAME, &[KEY]), 8).unwrap_err().contains("no AV1 sequence header"));
        assert!(Stream::new(&[], 8).is_err());
    }

    #[test]
    fn samples_leave_out_delimiters_and_padding_and_keep_frames_with_their_sizes() {
        let s = Stream::new(&obu(OBU_SEQUENCE_HEADER, &SEQ), 8).unwrap();
        let tu = [
            obu(OBU_TEMPORAL_DELIMITER, &[]),
            obu(OBU_SEQUENCE_HEADER, &SEQ),
            obu(OBU_FRAME, &[KEY, 7, 7]),
            obu(OBU_PADDING, &[0, 0]),
            obu(OBU_TILE_LIST, &[1]),
        ]
        .concat();
        let (sample, key) = s.sample(&tu).unwrap();
        assert_eq!(sample, [obu(OBU_SEQUENCE_HEADER, &SEQ), obu(OBU_FRAME, &[KEY, 7, 7])].concat());
        assert!(key);
        // A frame header and its tile group, with an extension byte, keep their order.
        let ext = [&[(OBU_FRAME_HEADER << 3) | 0x06, 0x08, 1, INTER][..], &obu(4, &[5, 5])].concat();
        assert_eq!(s.sample(&[obu(OBU_TEMPORAL_DELIMITER, &[]), ext.clone()].concat()).unwrap(), (ext, false));
        // The last OBU may leave out its size; the sample gives it one.
        let no_size = [&obu(OBU_TEMPORAL_DELIMITER, &[])[..], &[OBU_FRAME << 3, INTER, 9, 9, 9]].concat();
        assert_eq!(s.sample(&no_size).unwrap(), (obu(OBU_FRAME, &[INTER, 9, 9, 9]), false));
    }

    #[test]
    fn keyframes_are_shown_key_frames_and_the_first_frame_of_the_sample_decides() {
        let s = Stream::new(&obu(OBU_SEQUENCE_HEADER, &SEQ), 8).unwrap();
        let key = |first: u8| s.sample(&obu(OBU_FRAME, &[first, 0])).map(|(_, k)| k);
        assert_eq!(key(KEY), Ok(true));
        assert_eq!(key(INTER), Ok(false));
        assert_eq!(key(HIDDEN_KEY), Ok(false), "a key frame that is not shown");
        assert_eq!(key(SHOW_EXISTING), Ok(false), "a frame shown again");
        assert_eq!(key(0b0101_0000), Ok(false), "an intra-only frame");
        let hidden_then_shown = [obu(OBU_FRAME, &[HIDDEN_KEY, 1]), obu(OBU_FRAME_HEADER, &[KEY])].concat();
        assert!(!s.sample(&hidden_then_shown).unwrap().1);
        let still = Stream::new(&obu(OBU_SEQUENCE_HEADER, &still()), 8).unwrap();
        assert!(still.sample(&obu(OBU_FRAME, &[INTER])).unwrap().1, "a reduced still picture is always a key frame");
        assert!(s.sample(&obu(OBU_FRAME, &[])).unwrap_err().contains("truncated"));
    }

    #[test]
    fn a_changed_sequence_header_or_a_unit_without_a_frame_is_an_error() {
        let s = Stream::new(&obu(OBU_SEQUENCE_HEADER, &SEQ), 8).unwrap();
        let other = [obu(OBU_SEQUENCE_HEADER, &rich(0, false, 8)), obu(OBU_FRAME, &[KEY])].concat();
        assert!(s.sample(&other).unwrap_err().contains("changed its sequence header"));
        for empty in [&[][..], &obu(OBU_TEMPORAL_DELIMITER, &[]), &obu(OBU_SEQUENCE_HEADER, &SEQ), &obu(OBU_PADDING, &[1])] {
            assert!(s.sample(empty).unwrap_err().contains("no picture data"), "{empty:?}");
        }
        let t = Stream::new(&obu(OBU_SEQUENCE_HEADER, &SEQ), 8).unwrap();
        assert!(s.same_sequence_header(&t));
        assert!(!s.same_sequence_header(&Stream::new(&obu(OBU_SEQUENCE_HEADER, &rich(0, false, 8)), 8).unwrap()));
    }

    #[test]
    fn hostile_obu_streams_are_errors() {
        assert_eq!(obus(&[]).unwrap(), Vec::new());
        assert!(obus(&[0x80 | 0x12, 0]).unwrap_err().contains("forbidden"));
        assert!(obus(&[0x16]).unwrap_err().contains("header"), "an extension flag without its byte");
        assert!(obus(&[0x12]).unwrap_err().contains("size"));
        assert!(obus(&[0x12, 0x80, 0x80]).unwrap_err().contains("size"));
        assert!(obus(&[0x12, 5, 1, 2]).unwrap_err().contains("past the end"));
        assert!(obus(&[0x12, 0xff, 0xff, 0xff, 0xff, 0x7f]).unwrap_err().contains("2^32"));
        assert!(obus(&[0x12, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0]).unwrap_err().contains("8 bytes"));
        // A size written in more bytes than it needs is allowed (AV1 4.10.5).
        assert_eq!(obus(&[0x12, 0x81, 0x80, 0x00, 9]).unwrap()[0].payload, &[9]);
        for n in 0..SEQ.len() {
            assert!(SequenceHeader::parse(&SEQ[..n]).is_err(), "cut at {n}");
        }
        let r = rich(0, true, 13);
        for n in 0..r.len() - 1 {
            assert!(SequenceHeader::parse(&r[..n]).is_err(), "cut at {n}");
        }
    }

    #[test]
    fn sizes_are_written_in_the_fewest_bytes() {
        for (v, bytes) in
            [(0u64, &[0u8][..]), (127, &[0x7f]), (128, &[0x80, 1]), (16_384, &[0x80, 0x80, 1]), (MAX_OBU_SIZE, &[0xff, 0xff, 0xff, 0xff, 0x0f])]
        {
            let mut out = Vec::new();
            leb128(v, &mut out);
            assert_eq!(out, bytes, "{v}");
            assert_eq!(read_leb128(&out), Ok((v, out.len())));
        }
        let payload = vec![0x55; 300];
        let tu = [&[(OBU_FRAME << 3) | 0x02, 0xac, 0x02][..], &[KEY], &payload[1..]].concat();
        let s = Stream::new(&obu(OBU_SEQUENCE_HEADER, &SEQ), 8).unwrap();
        assert_eq!(s.sample(&tu).unwrap().0, tu, "a 300-byte frame keeps its two-byte size");
    }

    #[test]
    fn every_bit_flip_of_a_sequence_header_or_a_unit_is_a_result_or_an_error() {
        let seq = obu(OBU_SEQUENCE_HEADER, &rich(0, false, 9));
        let s = Stream::new(&seq, 8).unwrap();
        let tu = [obu(OBU_TEMPORAL_DELIMITER, &[]), seq.clone(), obu(OBU_FRAME, &[KEY, 3, 4])].concat();
        for data in [&seq, &tu] {
            for byte in 0..data.len() {
                for bit in 0..8 {
                    let mut m = data.clone();
                    m[byte] ^= 1 << bit;
                    let _ = Stream::new(&m, 8);
                    let _ = s.sample(&m);
                }
            }
        }
    }
}
