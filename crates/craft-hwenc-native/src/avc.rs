//! The `avcC` record (ISO/IEC 14496-15 5.3.2.1, `AVCDecoderConfigurationRecord`) of an H.264
//! stream, built from the SPS and PPS the encoder wrote. Matroska stores it as the track's
//! `CodecPrivate` (`V_MPEG4/ISO/AVC`) and MP4 as the `avcC` box. `craft-media`'s [`AvcConfig`]
//! writes the record.

use craft_media::bitstream::{BitReader, unescape_rbsp};
use craft_media::isobmff::AvcConfig;

/// The fields of an H.264 SPS (ITU-T H.264 7.3.2.1.1) the record repeats.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sps {
    pub profile_idc: u8,
    /// The `constraint_set` flags and reserved bits: the record's `profile_compatibility`.
    pub constraints: u8,
    pub level_idc: u8,
    /// 1 for 4:2:0. Profiles without the field are 4:2:0.
    pub chroma_format_idc: u32,
    pub bit_depth_luma: u32,
    pub bit_depth_chroma: u32,
}

/// Whether `profile_idc` is one whose SPS carries the chroma format and bit depths (H.264 7.3.2.1.1).
fn has_chroma_info(profile_idc: u8) -> bool {
    matches!(profile_idc, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135)
}

impl Sps {
    /// Parse the SPS NAL unit `nal` (with its one-byte header, without a start code).
    pub fn parse(nal: &[u8]) -> Result<Sps, String> {
        if nal.first().map(|b| b & 0x1f) != Some(7) {
            return Err("not an H.264 SPS NAL unit".into());
        }
        let rbsp = unescape_rbsp(nal.get(1..).unwrap_or_default());
        Self::read(&mut BitReader::new(&rbsp)).map_err(|_| "the H.264 SPS is truncated".to_string())
    }

    fn read(b: &mut BitReader) -> craft_media::bitstream::Result<Sps> {
        let profile_idc = b.read_bits(8)? as u8;
        let constraints = b.read_bits(8)? as u8;
        let level_idc = b.read_bits(8)? as u8;
        let _seq_parameter_set_id = b.read_ue()?;
        let (mut chroma_format_idc, mut bit_depth_luma, mut bit_depth_chroma) = (1, 8, 8);
        if has_chroma_info(profile_idc) {
            chroma_format_idc = b.read_ue()?;
            if chroma_format_idc == 3 {
                let _separate_colour_plane_flag = b.read_bit()?;
            }
            bit_depth_luma = b.read_ue()?.saturating_add(8);
            bit_depth_chroma = b.read_ue()?.saturating_add(8);
        }
        Ok(Sps { profile_idc, constraints, level_idc, chroma_format_idc, bit_depth_luma, bit_depth_chroma })
    }
}

/// The `avcC` record of a stream with one SPS and one PPS (NAL units with their headers, without
/// start codes), lengths of 4 bytes before each NAL unit in the samples. The stream must be 8-bit
/// 4:2:0, which is what the encoders of this crate are asked for.
pub fn avcc(sps: &[u8], pps: &[u8]) -> Result<Vec<u8>, String> {
    let s = Sps::parse(sps)?;
    if pps.first().map(|b| b & 0x1f) != Some(8) || pps.len() < 2 {
        return Err("not an H.264 PPS NAL unit".into());
    }
    if s.chroma_format_idc != 1 || s.bit_depth_luma != 8 || s.bit_depth_chroma != 8 {
        return Err(format!(
            "the encoder wrote chroma format {} at {}/{} bits, not 8-bit 4:2:0",
            s.chroma_format_idc, s.bit_depth_luma, s.bit_depth_chroma
        ));
    }
    // `AvcConfig::to_bytes` writes each length as a `u16` without checking it
    if u16::try_from(sps.len()).is_err() {
        return Err("the SPS is too long for an avcC record".into());
    }
    if u16::try_from(pps.len()).is_err() {
        return Err("the PPS is too long for an avcC record".into());
    }
    // 14496-15 adds the chroma format and bit depths for the High profiles 100, 110, 122 and 144
    let ext = if matches!(s.profile_idc, 100 | 110 | 122 | 144) {
        vec![
            0xfc | (s.chroma_format_idc as u8 & 3),
            0xf8 | (s.bit_depth_luma.saturating_sub(8) as u8 & 7),
            0xf8 | (s.bit_depth_chroma.saturating_sub(8) as u8 & 7),
            0,
        ]
    } else {
        Vec::new()
    };
    Ok(AvcConfig {
        profile: s.profile_idc,
        compatibility: s.constraints,
        level: s.level_idc,
        length_size: 4,
        sps: vec![sps.to_vec()],
        pps: vec![pps.to_vec()],
        ext,
    }
    .to_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A High profile SPS: profile 100, no constraint flags, level 4.0, then `ue` fields
    /// seq_parameter_set_id 0 (`1`), chroma_format_idc 1 (`010`), both bit depths 8 (`1`, `1`).
    const HIGH: [u8; 5] = [0x67, 100, 0x00, 40, 0b1010_1100];
    const PPS: [u8; 3] = [0x68, 0xee, 0x3c];

    #[test]
    fn a_high_profile_record_carries_its_chroma_format_and_depths() {
        let r = avcc(&HIGH, &PPS).unwrap();
        let mut expected = vec![1, 100, 0, 40, 0xff, 0xe1, 0, 5];
        expected.extend_from_slice(&HIGH);
        expected.extend_from_slice(&[1, 0, 3]);
        expected.extend_from_slice(&PPS);
        expected.extend_from_slice(&[0xfd, 0xf8, 0xf8, 0]);
        assert_eq!(r, expected);
    }

    #[test]
    fn a_main_profile_record_has_no_extension() {
        // profile 77, constraint_set1, level 3.1, seq_parameter_set_id 0
        let sps = [0x67, 77, 0x40, 31, 0x80];
        let r = avcc(&sps, &PPS).unwrap();
        assert_eq!(r.len(), 6 + 2 + sps.len() + 1 + 2 + PPS.len());
        assert_eq!(&r[..4], &[1, 77, 0x40, 31]);
    }

    #[test]
    fn other_formats_and_broken_units_are_refused() {
        // High 4:4:4 (244) at 4:4:4: chroma 3 (`00100`), separate_colour_plane 0, depths 8
        let yuv444 = [0x67, 244, 0, 40, 0b1001_0001, 0b1000_0000];
        assert!(avcc(&yuv444, &PPS).unwrap_err().contains("4:2:0"));
        // 10-bit luma: bit_depth_luma_minus8 2 (`011`)
        let ten = [0x67, 110, 0, 40, 0b1010_0111, 0b1000_0000];
        assert!(avcc(&ten, &PPS).is_err());
        for n in 0..HIGH.len() {
            assert!(avcc(&HIGH[..n], &PPS).is_err(), "SPS cut at {n}");
        }
        assert!(avcc(&PPS, &HIGH).is_err(), "swapped");
        assert!(avcc(&HIGH, &[0x68]).is_err(), "an empty PPS");
        let mut long = PPS.to_vec();
        long.resize(70_000, 0x55);
        assert!(avcc(&HIGH, &long).is_err());
    }

    #[test]
    fn every_bit_flip_of_the_sps_is_a_record_or_an_error() {
        for byte in 0..HIGH.len() {
            for bit in 0..8 {
                let mut m = HIGH;
                m[byte] ^= 1 << bit;
                let _ = avcc(&m, &PPS);
            }
        }
    }
}
