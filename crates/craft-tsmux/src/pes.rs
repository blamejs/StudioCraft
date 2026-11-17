//! PES packet headers (H.222.0 §2.4.3.6), the adaptation field's PCR (§2.4.3.4), and the bytes
//! written ahead of a frame: access unit delimiters, ADTS headers and Opus control headers.

use crate::{Codec, Error};

/// 2^33: PTS, DTS and the PCR base are 33-bit counters.
pub(crate) const WRAP_90K: i64 = 1 << 33;

/// Largest PES header this writer produces: 9 bytes, PTS and DTS.
pub(crate) const HEADER_MAX: usize = 19;

/// Largest prefix: an Opus control header for a 65 535-byte packet (2 bytes, 257 size bytes).
pub(crate) const PREFIX_MAX: usize = 2 + 0xFFFF / 255 + 1;

/// A 33-bit timestamp with its 4-bit prefix and marker bits (§2.4.3.7).
fn timestamp(prefix: u8, v: i64) -> [u8; 5] {
    let v = v.rem_euclid(WRAP_90K);
    [prefix << 4 | ((v >> 29) & 0x0E) as u8 | 1, (v >> 22) as u8, ((v >> 14) & 0xFE) as u8 | 1, (v >> 7) as u8, ((v << 1) & 0xFE) as u8 | 1]
}

/// Writes a PES header into `out` and returns its length. `length` is the PES_packet_length field
/// (0 for an unbounded video packet).
pub(crate) fn header(out: &mut [u8; HEADER_MAX], stream_id: u8, length: u16, pts: i64, dts: Option<i64>) -> usize {
    let [hi, lo] = length.to_be_bytes();
    // '10', not scrambled, no priority, data_alignment_indicator set, not copyrighted, a copy
    out[..7].copy_from_slice(&[0, 0, 1, stream_id, hi, lo, 0x84]);
    match dts {
        Some(dts) => {
            out[7] = 0xC0;
            out[8] = 10;
            out[9..14].copy_from_slice(&timestamp(3, pts));
            out[14..19].copy_from_slice(&timestamp(1, dts));
            19
        }
        None => {
            out[7] = 0x80;
            out[8] = 5;
            out[9..14].copy_from_slice(&timestamp(2, pts));
            14
        }
    }
}

/// The six program_clock_reference bytes for a 27 MHz clock value: the 33-bit base (clock / 300),
/// six reserved bits, and the 9-bit extension (clock % 300).
pub(crate) fn pcr(clock: i64) -> [u8; 6] {
    let v = clock.rem_euclid(WRAP_90K * 300);
    let (base, ext) = (v / 300, v % 300);
    [(base >> 25) as u8, (base >> 17) as u8, (base >> 9) as u8, (base >> 1) as u8, ((base & 1) << 7) as u8 | 0x7E | (ext >> 8) as u8, ext as u8]
}

/// The first NAL unit's first header byte, when `data` starts with an Annex B start code.
fn first_nal(data: &[u8]) -> Option<u8> {
    let at = if data.starts_with(&[0, 0, 1]) {
        3
    } else if data.starts_with(&[0, 0, 0, 1]) {
        4
    } else {
        return None;
    };
    data.get(at).copied()
}

/// Checks that `data` is a frame of `codec` and writes what goes in front of it into `out`.
/// Returns the prefix length.
pub(crate) fn prefix(codec: Codec, data: &[u8], out: &mut [u8; PREFIX_MAX]) -> Result<usize, Error> {
    match codec {
        Codec::H264 => {
            let nal = first_nal(data).ok_or(Error::BadFrame("an H.264 frame must start with an Annex B start code"))?;
            if nal & 0x1F == 9 {
                return Ok(0);
            }
            // access unit delimiter, primary_pic_type 7 (any slice type) (H.222.0 §2.14)
            out[..6].copy_from_slice(&[0, 0, 0, 1, 0x09, 0xF0]);
            Ok(6)
        }
        Codec::Hevc => {
            let nal = first_nal(data).ok_or(Error::BadFrame("an HEVC frame must start with an Annex B start code"))?;
            if (nal >> 1) & 0x3F == 35 {
                return Ok(0);
            }
            // access unit delimiter (nal_unit_type 35), pic_type 2 (any slice type) (H.222.0 §2.17)
            out[..7].copy_from_slice(&[0, 0, 0, 1, 0x46, 0x01, 0x50]);
            Ok(7)
        }
        Codec::Mpeg2Video => {
            first_nal(data).ok_or(Error::BadFrame("an MPEG-2 video frame must start with a start code"))?;
            Ok(0)
        }
        Codec::AacAdts => match data {
            [0xFF, b, _, _, _, _, _, ..] if b & 0xF6 == 0xF0 => Ok(0),
            _ => Err(Error::BadFrame("an AAC frame must start with an ADTS header")),
        },
        Codec::Aac { object_type, sample_rate_index, channels } => {
            let len = data.len() + 7;
            if len > 0x1FFF {
                return Err(Error::FrameTooLarge { size: data.len(), limit: 0x1FFF - 7 });
            }
            // ADTS fixed and variable header (ISO/IEC 13818-7 §6.2): MPEG-4, no CRC, buffer
            // fullness 0x7FF (variable rate), one raw data block
            out[..7].copy_from_slice(&[
                0xFF,
                0xF1,
                (object_type - 1) << 6 | sample_rate_index << 2 | channels >> 2,
                (channels & 3) << 6 | (len >> 11) as u8,
                (len >> 3) as u8,
                ((len & 7) as u8) << 5 | 0x1F,
                0xFC,
            ]);
            Ok(7)
        }
        Codec::Opus { .. } => {
            // opus_control_header: the 11-bit prefix 0x3FF, no trim or extension, then the
            // payload size as 0xFF bytes and a last byte
            let n = data.len() / 255;
            if 2 + n + 1 > PREFIX_MAX {
                return Err(Error::FrameTooLarge { size: data.len(), limit: 0xFFFF - PREFIX_MAX });
            }
            out[0] = 0x7F;
            out[1] = 0xE0;
            out[2..2 + n].fill(0xFF);
            out[2 + n] = (data.len() % 255) as u8;
            Ok(3 + n)
        }
        Codec::Ac3 => match data {
            [0x0B, 0x77, ..] => Ok(0),
            _ => Err(Error::BadFrame("an AC-3 frame must start with its sync word")),
        },
        Codec::MpegAudio { .. } => match data {
            [0xFF, b, ..] if b & 0xE0 == 0xE0 => Ok(0),
            _ => Err(Error::BadFrame("an MPEG audio frame must start with its sync word")),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads a 33-bit timestamp back from its five bytes.
    fn read(b: &[u8]) -> i64 {
        (((b[0] >> 1) & 7) as i64) << 30 | (b[1] as i64) << 22 | ((b[2] >> 1) as i64) << 15 | (b[3] as i64) << 7 | (b[4] >> 1) as i64
    }

    #[test]
    fn timestamps_carry_their_prefix_and_markers() {
        for v in [0, 1, 90_000, (1 << 32) + 12_345, WRAP_90K - 1] {
            let b = timestamp(2, v);
            assert_eq!(b[0] >> 4, 2);
            assert_eq!([b[0] & 1, b[2] & 1, b[4] & 1], [1, 1, 1]);
            assert_eq!(read(&b), v);
        }
        assert_eq!(read(&timestamp(2, WRAP_90K + 5)), 5, "wraps at 2^33");
        assert_eq!(read(&timestamp(2, -1)), WRAP_90K - 1);
    }

    #[test]
    fn headers_hold_pts_and_dts() {
        let mut h = [0u8; HEADER_MAX];
        assert_eq!(header(&mut h, 0xE0, 0, 3000, Some(0)), 19);
        assert_eq!(&h[..9], &[0, 0, 1, 0xE0, 0, 0, 0x84, 0xC0, 10]);
        assert_eq!((h[9] >> 4, read(&h[9..14]), h[14] >> 4, read(&h[14..19])), (3, 3000, 1, 0));
        assert_eq!(header(&mut h, 0xC0, 300, 7, None), 14);
        assert_eq!(&h[4..9], &[1, 44, 0x84, 0x80, 5]);
    }

    #[test]
    fn pcr_splits_base_and_extension() {
        let clock = 123_456_789i64 * 300 + 299;
        let b = pcr(clock);
        let base = (b[0] as i64) << 25 | (b[1] as i64) << 17 | (b[2] as i64) << 9 | (b[3] as i64) << 1 | (b[4] >> 7) as i64;
        let ext = ((b[4] & 1) as i64) << 8 | b[5] as i64;
        assert_eq!((base, ext, b[4] & 0x7E), (123_456_789, 299, 0x7E));
    }

    #[test]
    fn access_unit_delimiters_are_added_once() {
        let mut p = [0u8; PREFIX_MAX];
        assert_eq!(prefix(Codec::H264, &[0, 0, 0, 1, 0x65, 0x88], &mut p).unwrap(), 6);
        assert_eq!(prefix(Codec::H264, &[0, 0, 1, 0x09, 0xF0, 0, 0, 1, 0x65], &mut p).unwrap(), 0);
        assert_eq!(prefix(Codec::Hevc, &[0, 0, 0, 1, 0x26, 0x01], &mut p).unwrap(), 7);
        assert_eq!(prefix(Codec::Hevc, &[0, 0, 1, 0x46, 0x01, 0x50], &mut p).unwrap(), 0);
        assert!(matches!(prefix(Codec::H264, &[0x65, 0x88], &mut p), Err(Error::BadFrame(_))));
        assert!(matches!(prefix(Codec::H264, &[0, 0, 1], &mut p), Err(Error::BadFrame(_))));
    }

    #[test]
    fn opus_control_header_codes_the_size() {
        let mut p = [0u8; PREFIX_MAX];
        assert_eq!(prefix(Codec::Opus { channels: 2 }, &[0xFC; 254], &mut p).unwrap(), 3);
        assert_eq!(&p[..3], &[0x7F, 0xE0, 254]);
        assert_eq!(prefix(Codec::Opus { channels: 2 }, &[0xFC; 510], &mut p).unwrap(), 5);
        assert_eq!(&p[..5], &[0x7F, 0xE0, 0xFF, 0xFF, 0]);
    }

    #[test]
    fn adts_header_for_raw_aac() {
        let mut p = [0u8; PREFIX_MAX];
        let codec = Codec::Aac { object_type: 2, sample_rate_index: 3, channels: 2 };
        assert_eq!(prefix(codec, &[0; 100], &mut p).unwrap(), 7);
        // AAC LC, 48 kHz, 2 channels, 107 bytes
        assert_eq!(&p[..7], &[0xFF, 0xF1, 0x4C, 0x80, 0x0D, 0x7F, 0xFC]);
        assert!(matches!(prefix(codec, &[0; 8185], &mut p), Err(Error::FrameTooLarge { .. })));
    }
}
