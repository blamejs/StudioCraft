//! The color signal a stream declares: standard dynamic range BT.709, or high dynamic range with
//! BT.2020 primaries and matrix and the PQ (SMPTE ST 2084) or HLG (ARIB STD-B67) transfer. A PQ
//! stream can also carry HDR10 static metadata: the mastering display color volume (SMPTE ST 2086)
//! and the content light level, which the encoder writes as SEI messages in front of every IDR
//! picture (ITU-T H.265 Annex D.2.28 and D.2.35).
//!
//! The code points are those of ITU-T H.273, and the signal is limited range.

/// The SEI payload type of the mastering display colour volume message (H.265 D.2.28).
pub const SEI_MASTERING_DISPLAY: u32 = 137;
/// The SEI payload type of the content light level information message (H.265 D.2.35).
pub const SEI_CONTENT_LIGHT_LEVEL: u32 = 144;

/// The largest CIE 1931 chromaticity coordinate in units of 0.00002 (a coordinate of 1.0).
const MAX_CHROMATICITY: u16 = 50_000;

/// The color signal of a stream.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Color {
    /// BT.709 primaries, transfer and matrix.
    #[default]
    Bt709,
    /// BT.2020 primaries and non-constant luminance matrix with the PQ transfer, and the static
    /// metadata to write with it. A field left as `None` writes no SEI message for it.
    Pq { mastering: Option<MasteringDisplay>, light: Option<ContentLight> },
    /// BT.2020 primaries and non-constant luminance matrix with the HLG transfer. HLG has no static
    /// metadata.
    Hlg,
}

/// The color volume of the display the content was mastered on (SMPTE ST 2086).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MasteringDisplay {
    /// The green, blue and red primaries, in that order, as CIE 1931 (x, y) in units of 0.00002.
    pub primaries: [(u16, u16); 3],
    /// The white point as CIE 1931 (x, y) in units of 0.00002.
    pub white_point: (u16, u16),
    /// The display's peak luminance in units of 0.0001 cd/m².
    pub max_luminance: u32,
    /// The display's black level in units of 0.0001 cd/m².
    pub min_luminance: u32,
}

impl MasteringDisplay {
    /// A display with the BT.2020 primaries and the D65 white point, and the given peak and black
    /// luminance in units of 0.0001 cd/m².
    pub const fn bt2020_d65(max_luminance: u32, min_luminance: u32) -> MasteringDisplay {
        MasteringDisplay {
            primaries: [(8_500, 39_850), (6_550, 2_300), (35_400, 14_600)],
            white_point: (15_635, 16_450),
            max_luminance,
            min_luminance,
        }
    }

    /// The 24-byte payload of the mastering display colour volume SEI message: each primary's x and
    /// y, the white point's x and y, then the peak and black luminance, all big-endian.
    pub fn sei_payload(&self) -> [u8; 24] {
        let mut out = [0u8; 24];
        let coords = self.primaries.iter().chain(std::iter::once(&self.white_point)).flat_map(|&(x, y)| [x, y]);
        for (dst, v) in out.as_chunks_mut::<2>().0.iter_mut().zip(coords) {
            *dst = v.to_be_bytes();
        }
        out[16..20].copy_from_slice(&self.max_luminance.to_be_bytes());
        out[20..24].copy_from_slice(&self.min_luminance.to_be_bytes());
        out
    }

    fn check(&self) -> Result<(), String> {
        let mut coords = self.primaries.iter().chain(std::iter::once(&self.white_point));
        if let Some(&(x, y)) = coords.find(|&&(x, y)| x > MAX_CHROMATICITY || y > MAX_CHROMATICITY) {
            return Err(format!("the chromaticity ({x}, {y}) is outside 0 to {MAX_CHROMATICITY} (0.00002 units)"));
        }
        if self.min_luminance >= self.max_luminance {
            return Err(format!(
                "the mastering display's black level {} is not below its peak {} (0.0001 cd/m² units)",
                self.min_luminance, self.max_luminance
            ));
        }
        Ok(())
    }
}

/// The content light level of a stream, in cd/m². A value of 0 means unknown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContentLight {
    /// The brightest pixel of the stream (MaxCLL).
    pub max_cll: u16,
    /// The highest frame-average light level of the stream (MaxFALL).
    pub max_fall: u16,
}

impl ContentLight {
    /// The 4-byte payload of the content light level information SEI message: MaxCLL, then
    /// MaxFALL, big-endian.
    pub fn sei_payload(&self) -> [u8; 4] {
        let [a, b] = self.max_cll.to_be_bytes();
        let [c, d] = self.max_fall.to_be_bytes();
        [a, b, c, d]
    }

    fn check(&self) -> Result<(), String> {
        if self.max_cll != 0 && self.max_fall > self.max_cll {
            return Err(format!("MaxFALL {} cd/m² is above MaxCLL {} cd/m²", self.max_fall, self.max_cll));
        }
        Ok(())
    }
}

/// One SEI message: its payload type and payload bytes, without the type and size header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sei {
    pub payload_type: u32,
    pub payload: Vec<u8>,
}

impl Color {
    /// Whether the signal is high dynamic range.
    pub fn is_hdr(self) -> bool {
        !matches!(self, Color::Bt709)
    }

    /// The H.273 code points `(colour_primaries, transfer_characteristics, matrix_coefficients)`.
    pub fn code_points(self) -> (u8, u8, u8) {
        match self {
            Color::Bt709 => (1, 1, 1),
            Color::Pq { .. } => (9, 16, 9),
            Color::Hlg => (9, 18, 9),
        }
    }

    /// Check the static metadata: chromaticities within 0 to 1, a black level below the peak, and
    /// MaxFALL no higher than a known MaxCLL.
    pub fn check(self) -> Result<(), String> {
        if let Color::Pq { mastering, light } = self {
            mastering.as_ref().map_or(Ok(()), MasteringDisplay::check)?;
            light.as_ref().map_or(Ok(()), ContentLight::check)?;
        }
        Ok(())
    }

    /// The SEI messages the signal writes in front of every IDR picture: for PQ the mastering
    /// display colour volume, then the content light level, each when it is set; nothing otherwise.
    pub fn sei(self) -> Vec<Sei> {
        let mut out = Vec::new();
        if let Color::Pq { mastering, light } = self {
            if let Some(m) = mastering {
                out.push(Sei { payload_type: SEI_MASTERING_DISPLAY, payload: m.sei_payload().to_vec() });
            }
            if let Some(l) = light {
                out.push(Sei { payload_type: SEI_CONTENT_LIGHT_LEVEL, payload: l.sei_payload().to_vec() });
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DISPLAY: MasteringDisplay = MasteringDisplay::bt2020_d65(10_000_000, 50);

    #[test]
    fn code_points_follow_h273() {
        assert_eq!(Color::Bt709.code_points(), (1, 1, 1));
        assert_eq!(Color::Pq { mastering: None, light: None }.code_points(), (9, 16, 9));
        assert_eq!(Color::Hlg.code_points(), (9, 18, 9));
        assert!(!Color::default().is_hdr());
        assert!(Color::Hlg.is_hdr());
    }

    #[test]
    fn the_mastering_display_payload_is_big_endian_in_green_blue_red_order() {
        let p = DISPLAY.sei_payload();
        assert_eq!(
            p,
            [
                0x21, 0x34, 0x9b, 0xaa, // green (8500, 39850)
                0x19, 0x96, 0x08, 0xfc, // blue (6550, 2300)
                0x8a, 0x48, 0x39, 0x08, // red (35400, 14600)
                0x3d, 0x13, 0x40, 0x42, // D65 (15635, 16450)
                0x00, 0x98, 0x96, 0x80, // 1,000 cd/m²
                0x00, 0x00, 0x00, 0x32, // 0.005 cd/m²
            ]
        );
    }

    #[test]
    fn pq_writes_the_messages_that_are_set_and_hlg_and_sdr_write_none() {
        let light = ContentLight { max_cll: 1000, max_fall: 400 };
        let both = Color::Pq { mastering: Some(DISPLAY), light: Some(light) }.sei();
        assert_eq!(both.iter().map(|m| m.payload_type).collect::<Vec<_>>(), [137, 144]);
        assert_eq!(both[0].payload, DISPLAY.sei_payload());
        assert_eq!(both[1].payload, [0x03, 0xe8, 0x01, 0x90]);
        let only_light = Color::Pq { mastering: None, light: Some(light) }.sei();
        assert_eq!(only_light.iter().map(|m| m.payload_type).collect::<Vec<_>>(), [144]);
        assert!(Color::Pq { mastering: None, light: None }.sei().is_empty());
        assert!(Color::Hlg.sei().is_empty());
        assert!(Color::Bt709.sei().is_empty());
    }

    #[test]
    fn impossible_metadata_is_refused() {
        let pq = |mastering, light| Color::Pq { mastering, light };
        assert_eq!(pq(Some(DISPLAY), Some(ContentLight { max_cll: 1000, max_fall: 1000 })).check(), Ok(()));
        assert_eq!(pq(None, Some(ContentLight { max_cll: 0, max_fall: 400 })).check(), Ok(()), "MaxCLL unknown");
        assert_eq!(pq(None, Some(ContentLight { max_cll: 0, max_fall: 0 })).check(), Ok(()));
        for bad in [
            pq(Some(MasteringDisplay { white_point: (50_001, 0), ..DISPLAY }), None),
            pq(Some(MasteringDisplay { primaries: [(0, u16::MAX), (0, 0), (0, 0)], ..DISPLAY }), None),
            pq(Some(MasteringDisplay::bt2020_d65(50, 50)), None),
            pq(Some(MasteringDisplay::bt2020_d65(0, 0)), None),
            pq(Some(MasteringDisplay::bt2020_d65(10, u32::MAX)), None),
            pq(None, Some(ContentLight { max_cll: 400, max_fall: 401 })),
        ] {
            assert!(bad.check().is_err(), "{bad:?}");
        }
        assert_eq!(Color::Hlg.check(), Ok(()));
        assert_eq!(Color::Bt709.check(), Ok(()));
    }
}
