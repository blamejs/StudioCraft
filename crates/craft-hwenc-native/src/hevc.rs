//! The `hvcC` record (ISO/IEC 14496-15 8.3.2.1, `HEVCDecoderConfigurationRecord`) of an HEVC
//! stream, built from the VPS, SPS and PPS the encoder wrote. Matroska stores it as the track's
//! `CodecPrivate` (`V_MPEGH/ISO/HEVC`) and MP4 as the `hvcC` box.
//!
//! The profile, tier, compatibility flags, constraint flags, level, chroma format, bit depths and
//! temporal layers come from the SPS (ITU-T H.265 7.3.2.2.1 and 7.3.3). The arrays are complete:
//! the parameter sets are in the record only, never in the samples. `craft-media`'s
//! [`HevcConfig`] writes the record.

use craft_media::bitstream::{BitReader, unescape_rbsp};
use craft_media::isobmff::{HevcConfig, HevcNalArray};

use crate::Codec;
use crate::annexb::nal_type;

/// The fields of an HEVC SPS the record repeats, and the picture size after cropping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sps {
    pub max_sub_layers_minus1: u8,
    pub temporal_id_nesting: bool,
    pub profile_space: u8,
    pub tier: bool,
    pub profile_idc: u8,
    pub compatibility: u32,
    /// The 48 bits after the compatibility flags (progressive_source_flag first).
    pub constraints: u64,
    pub level_idc: u8,
    pub chroma_format_idc: u32,
    pub bit_depth_luma: u32,
    pub bit_depth_chroma: u32,
    /// Width and height after the conformance window.
    pub size: (u32, u32),
}

impl Sps {
    /// Parse the SPS NAL unit `nal` (with its two-byte header, without a start code).
    pub fn parse(nal: &[u8]) -> Result<Sps, String> {
        if nal_type(Codec::Hevc, nal) != Some(33) {
            return Err("not an HEVC SPS NAL unit".into());
        }
        let rbsp = unescape_rbsp(nal.get(2..).unwrap_or_default());
        Self::read(&mut BitReader::new(&rbsp)).map_err(|_| "the HEVC SPS is truncated".to_string())?
    }

    fn read(b: &mut BitReader) -> craft_media::bitstream::Result<Result<Sps, String>> {
        let _vps_id = b.read_bits(4)?;
        let max_sub_layers_minus1 = b.read_bits(3)? as u8;
        let temporal_id_nesting = b.read_bit()?;
        let profile_space = b.read_bits(2)? as u8;
        let tier = b.read_bit()?;
        let profile_idc = b.read_bits(5)? as u8;
        let compatibility = b.read_bits(32)?;
        let constraints = b.read_bits_u64(48)?;
        let level_idc = b.read_bits(8)? as u8;
        let subs = usize::from(max_sub_layers_minus1);
        let mut present = Vec::with_capacity(subs);
        for _ in 0..subs {
            present.push((b.read_bit()?, b.read_bit()?));
        }
        if subs > 0 {
            b.skip(2 * (8 - subs))?;
        }
        for (profile, level) in present {
            if profile {
                b.skip(88)?;
            }
            if level {
                b.skip(8)?;
            }
        }
        let _sps_id = b.read_ue()?;
        let chroma_format_idc = b.read_ue()?;
        let separate_planes = chroma_format_idc == 3 && b.read_bit()?;
        let width = b.read_ue()?;
        let height = b.read_ue()?;
        let (mut left, mut right, mut top, mut bottom) = (0, 0, 0, 0);
        if b.read_bit()? {
            (left, right, top, bottom) = (b.read_ue()?, b.read_ue()?, b.read_ue()?, b.read_ue()?);
        }
        let bit_depth_luma = b.read_ue()?.saturating_add(8);
        let bit_depth_chroma = b.read_ue()?.saturating_add(8);
        // H.265 Table 6-1: the conformance window counts chroma samples
        let (sub_w, sub_h) = match (chroma_format_idc, separate_planes) {
            (1, _) => (2, 2),
            (2, _) => (2, 1),
            _ => (1, 1),
        };
        let crop_w = left.saturating_add(right).saturating_mul(sub_w);
        let crop_h = top.saturating_add(bottom).saturating_mul(sub_h);
        if crop_w >= width || crop_h >= height {
            return Ok(Err(format!("the HEVC SPS crops its {width}x{height} pictures to nothing")));
        }
        Ok(Ok(Sps {
            max_sub_layers_minus1,
            temporal_id_nesting,
            profile_space,
            tier,
            profile_idc,
            compatibility,
            constraints,
            level_idc,
            chroma_format_idc,
            bit_depth_luma,
            bit_depth_chroma,
            size: (width - crop_w, height - crop_h),
        }))
    }
}

/// The `hvcC` record of a stream of `size` pictures whose parameter sets are `vps`, `sps` and
/// `pps` (NAL units with their headers, without start codes), lengths of 4 bytes before each NAL
/// unit in the samples. `bit_depth` 8 asks for Main (profile 1) and 10 for Main 10 (profile 2),
/// both 4:2:0; a stream that is not the one asked for is an error.
pub fn hvcc(vps: &[u8], sps: &[u8], pps: &[u8], size: (u32, u32), bit_depth: u32) -> Result<Vec<u8>, String> {
    let (profile, name) = match bit_depth {
        8 => (1u8, "Main"),
        10 => (2, "Main 10"),
        other => return Err(format!("{other}-bit HEVC is not written by this crate")),
    };
    for (nal, kind, label) in [(vps, 32u8, "VPS"), (sps, 33, "SPS"), (pps, 34, "PPS")] {
        if nal_type(Codec::Hevc, nal) != Some(kind) || nal.len() <= 2 {
            return Err(format!("the encoder's {label} is not a {label} NAL unit"));
        }
        if u16::try_from(nal.len()).is_err() {
            return Err(format!("the {label} is too long for an hvcC record"));
        }
    }
    let s = Sps::parse(sps)?;
    if s.profile_space != 0 || s.profile_idc != profile {
        return Err(format!("the encoder wrote HEVC profile {} (space {}), not {name}", s.profile_idc, s.profile_space));
    }
    if s.chroma_format_idc != 1 || s.bit_depth_luma != bit_depth || s.bit_depth_chroma != bit_depth {
        return Err(format!(
            "the encoder wrote chroma format {} at {}/{} bits, not {bit_depth}-bit 4:2:0",
            s.chroma_format_idc, s.bit_depth_luma, s.bit_depth_chroma
        ));
    }
    if s.size != size {
        return Err(format!("the encoder wrote {}x{} pictures for {}x{} frames", s.size.0, s.size.1, size.0, size.1));
    }
    Ok(HevcConfig {
        general_profile_space: s.profile_space,
        general_tier_flag: s.tier,
        general_profile_idc: s.profile_idc,
        general_profile_compatibility_flags: s.compatibility,
        general_constraint_indicator_flags: s.constraints,
        general_level_idc: s.level_idc,
        // min_spatial_segmentation_idc 0 and parallelismType 0: not specified
        min_spatial_segmentation_idc: 0,
        parallelism_type: 0,
        chroma_format_idc: 1,
        bit_depth_luma: bit_depth as u8,
        bit_depth_chroma: bit_depth as u8,
        // avgFrameRate 0 (unspecified) and constantFrameRate 0
        avg_frame_rate: 0,
        constant_frame_rate: 0,
        num_temporal_layers: s.max_sub_layers_minus1.saturating_add(1),
        temporal_id_nested: s.temporal_id_nesting,
        length_size: 4,
        arrays: [(vps, 32u8), (sps, 33), (pps, 34)]
            .into_iter()
            .map(|(nal, nal_type)| HevcNalArray { completeness: true, nal_type, nalus: vec![nal.to_vec()] })
            .collect(),
    }
    .to_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Parameter sets an NVENC encoder wrote for 1280x720 HEVC Main and, with the PQ signal, Main 10
    // (coded at 1280x736 and cropped). They come from FilmCraft's tests (see NOTICE).
    const VPS: &str = "40010c01ffff01600000030090000003000003005d9940c0000003004000000614";
    const SPS: &str = "42010101600000030090000003000003005da00280802e1f1396654a421191bff0c05a8080808a0000030002000003003010";
    const PPS: &str = "4401c1937c0cc9";
    const VPS10: &str = "40010c01ffff2220000003009000000300000300789940c0000003004000000614";
    const SPS10: &str = "420101222000000300900000030000030078a00280802e1f12d96654a421191bff0c05a8488048a000000300200000030301";
    const PPS10: &str = "4401c1933c0cc9";

    fn unhex(h: &str) -> Vec<u8> {
        (0..h.len()).step_by(2).filter_map(|i| u8::from_str_radix(h.get(i..i + 2)?, 16).ok()).collect()
    }

    #[test]
    fn the_sps_reads_as_the_stream_it_describes() {
        let s = Sps::parse(&unhex(SPS)).unwrap();
        assert_eq!((s.profile_space, s.tier, s.profile_idc), (0, false, 1));
        assert_eq!(s.compatibility, 0x6000_0000, "Main and Main 10");
        assert_eq!(s.constraints, 0x9000_0000_0000, "progressive_source_flag and frame_only_constraint_flag");
        assert_eq!(s.level_idc, 93, "level 3.1");
        assert_eq!((s.chroma_format_idc, s.bit_depth_luma, s.bit_depth_chroma), (1, 8, 8));
        assert_eq!((s.max_sub_layers_minus1, s.temporal_id_nesting), (0, true));
        assert_eq!(s.size, (1280, 720));
        let t = Sps::parse(&unhex(SPS10)).unwrap();
        assert_eq!((t.tier, t.profile_idc, t.compatibility, t.level_idc), (true, 2, 0x2000_0000, 120));
        assert_eq!((t.bit_depth_luma, t.bit_depth_chroma, t.size), (10, 10, (1280, 720)));
    }

    #[test]
    fn the_record_repeats_the_sps_and_holds_the_three_parameter_sets() {
        let (v, s, p) = (unhex(VPS), unhex(SPS), unhex(PPS));
        let r = hvcc(&v, &s, &p, (1280, 720), 8).unwrap();
        // the profile, tier, level record is the SPS's own 12 bytes after its first RBSP byte
        let rbsp = unescape_rbsp(&s[2..]);
        assert_eq!(&r[1..13], &rbsp[1..13]);
        assert_eq!(&r[13..23], &[0xf0, 0, 0xfc, 0xfd, 0xf8, 0xf8, 0, 0, 0x0f, 3]);
        let mut at = 23;
        for (nal, kind) in [(&v, 32u8), (&s, 33), (&p, 34)] {
            assert_eq!(r[at], 0x80 | kind);
            assert_eq!(&r[at + 1..at + 3], &[0, 1]);
            assert_eq!(usize::from(u16::from_be_bytes([r[at + 3], r[at + 4]])), nal.len());
            assert_eq!(&r[at + 5..at + 5 + nal.len()], &nal[..]);
            at += 5 + nal.len();
        }
        assert_eq!(at, r.len());
        let r10 = hvcc(&unhex(VPS10), &unhex(SPS10), &unhex(PPS10), (1280, 720), 10).unwrap();
        assert_eq!(&r10[16..19], &[0xfd, 0xfa, 0xfa], "4:2:0 at 10 bits");
    }

    #[test]
    fn a_stream_that_was_not_asked_for_is_refused() {
        let (v, s, p) = (unhex(VPS), unhex(SPS), unhex(PPS));
        for wrong in [(1280, 736), (1920, 1080), (0, 0)] {
            assert!(hvcc(&v, &s, &p, wrong, 8).unwrap_err().contains("1280x720"));
        }
        assert!(hvcc(&v, &s, &p, (1280, 720), 10).unwrap_err().contains("not Main 10"));
        let (v10, s10, p10) = (unhex(VPS10), unhex(SPS10), unhex(PPS10));
        assert!(hvcc(&v10, &s10, &p10, (1280, 720), 8).unwrap_err().contains("not Main"));
        for depth in [0, 7, 9, 12] {
            assert!(hvcc(&v, &s, &p, (1280, 720), depth).is_err(), "{depth}");
        }
        assert!(hvcc(&s, &v, &p, (1280, 720), 8).is_err(), "swapped");
    }

    #[test]
    fn truncated_and_flipped_parameter_sets_are_errors_or_records() {
        let (v, s, p) = (unhex(VPS), unhex(SPS), unhex(PPS));
        // the fields the record needs end in the SPS's 24th byte
        for n in 0..24 {
            assert!(hvcc(&v, &s[..n], &p, (1280, 720), 8).is_err(), "SPS cut at {n}");
        }
        for n in 24..s.len() {
            let _ = hvcc(&v, &s[..n], &p, (1280, 720), 8);
        }
        for byte in 0..s.len() {
            for bit in 0..8 {
                let mut m = s.clone();
                m[byte] ^= 1 << bit;
                let _ = hvcc(&v, &m, &p, (1280, 720), 8);
            }
        }
        let mut huge = s.clone();
        huge.resize(70_000, 0x55);
        assert!(hvcc(&v, &huge, &p, (1280, 720), 8).is_err());
    }
}
