//! Program specific information: the program association section and the program map section
//! (H.222.0 §2.4.4), and the CRC-32 that ends every section (Annex A).

use crate::{Codec, Error, Stream};

/// Byte-wise table for the CRC-32 of Annex A: polynomial 0x04C11DB7, most significant bit first.
const CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = (i as u32) << 24;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 0x8000_0000 != 0 { (crc << 1) ^ 0x04C1_1DB7 } else { crc << 1 };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
};

/// The section CRC: initial value all ones, no final inversion. Over a whole section, CRC field
/// included, the result is 0.
pub(crate) fn crc32(data: &[u8]) -> u32 {
    data.iter().fold(0xFFFF_FFFF, |crc, &b| (crc << 8) ^ CRC_TABLE[((crc >> 24) as u8 ^ b) as usize])
}

/// Largest `section_length` of a program association or program map section (§2.4.4.3, §2.4.4.8).
pub(crate) const MAX_SECTION_LENGTH: usize = 1021;

/// Writes `section_length` into bytes 1 and 2 and appends the CRC. `s` holds the section from
/// `table_id` up to the CRC.
fn finish(mut s: Vec<u8>) -> Result<Vec<u8>, Error> {
    let length = s.len() + 4 - 3;
    if length > MAX_SECTION_LENGTH {
        return Err(Error::Config(format!("the program map section would be {length} bytes long; the limit is {MAX_SECTION_LENGTH}")));
    }
    s[1] = 0xB0 | (length >> 8) as u8;
    s[2] = length as u8;
    let crc = crc32(&s);
    s.extend_from_slice(&crc.to_be_bytes());
    Ok(s)
}

/// The program association section with one program.
pub(crate) fn pat(transport_stream_id: u16, program_number: u16, pmt_pid: u16) -> Result<Vec<u8>, Error> {
    let mut s = vec![0x00, 0, 0];
    s.extend_from_slice(&transport_stream_id.to_be_bytes());
    // reserved, version_number 0, current_next_indicator 1; section_number 0, last_section_number 0
    s.extend_from_slice(&[0xC1, 0x00, 0x00]);
    s.extend_from_slice(&program_number.to_be_bytes());
    s.extend_from_slice(&(0xE000 | pmt_pid).to_be_bytes());
    finish(s)
}

/// The program map section.
pub(crate) fn pmt(program_number: u16, pcr_pid: u16, streams: &[Stream]) -> Result<Vec<u8>, Error> {
    let mut s = vec![0x02, 0, 0];
    s.extend_from_slice(&program_number.to_be_bytes());
    s.extend_from_slice(&[0xC1, 0x00, 0x00]);
    s.extend_from_slice(&(0xE000 | pcr_pid).to_be_bytes());
    // program_info_length 0
    s.extend_from_slice(&[0xF0, 0x00]);
    for st in streams {
        s.push(st.codec.stream_type());
        s.extend_from_slice(&(0xE000 | st.pid).to_be_bytes());
        let at = s.len();
        s.extend_from_slice(&[0xF0, 0x00]);
        descriptors(st, &mut s);
        let len = s.len() - at - 2;
        s[at] = 0xF0 | (len >> 8) as u8;
        s[at + 1] = len as u8;
    }
    finish(s)
}

/// The elementary stream descriptors of one stream.
fn descriptors(st: &Stream, s: &mut Vec<u8>) {
    match st.codec {
        // registration_descriptor (§2.6.8) with the format identifier "AC-3"
        Codec::Ac3 => s.extend_from_slice(&[0x05, 4, b'A', b'C', b'-', b'3']),
        Codec::Opus { channels } => {
            // registration_descriptor "Opus", then the Opus audio descriptor as an extension
            // descriptor (tag 0x7F) whose channel_config_code is the channel count (1 to 8)
            s.extend_from_slice(&[0x05, 4, b'O', b'p', b'u', b's']);
            s.extend_from_slice(&[0x7F, 2, OPUS_DESCRIPTOR_EXTENSION, channels]);
        }
        _ => {}
    }
    if let Some(lang) = st.language {
        // ISO_639_language_descriptor (§2.6.18), audio_type 0 (undefined)
        s.extend_from_slice(&[0x0A, 4, lang[0], lang[1], lang[2], 0]);
    }
}

/// `descriptor_tag_extension` of the Opus audio descriptor. The Opus transport draft leaves the
/// value open; 0x80 is the value in use for it.
pub(crate) const OPUS_DESCRIPTOR_EXTENSION: u8 = 0x80;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_check_value() {
        // the CRC-32/MPEG-2 check value of the ASCII digits 1 to 9
        assert_eq!(crc32(b"123456789"), 0x0376_E6E7);
    }

    #[test]
    fn sections_end_with_a_crc_over_the_whole_section() {
        let pat = pat(7, 1, 0x1000).unwrap();
        assert_eq!(pat.len(), 16);
        assert_eq!(crc32(&pat), 0);
        assert_eq!(&pat[..3], &[0x00, 0xB0, 13]);
        assert_eq!(&pat[8..12], &[0x00, 0x01, 0xF0, 0x00]);
        let st = [Stream::new(0x100, Codec::H264), Stream::new(0x101, Codec::Opus { channels: 2 }).with_language(*b"eng")];
        let pmt = pmt(1, 0x100, &st).unwrap();
        assert_eq!(crc32(&pmt), 0);
        assert_eq!(pmt.len(), 3 + ((pmt[1] as usize & 0x0F) << 8 | pmt[2] as usize));
        assert_eq!(&pmt[8..10], &[0xE1, 0x00], "PCR_PID");
        assert_eq!(&pmt[12..17], &[0x1B, 0xE1, 0x00, 0xF0, 0x00], "H.264 without descriptors");
        assert_eq!(&pmt[17..22], &[0x06, 0xE1, 0x01, 0xF0, 16]);
    }

    #[test]
    fn a_section_past_the_length_limit_is_refused() {
        let st: Vec<Stream> = (0..200).map(|i| Stream::new(0x100 + i, Codec::Ac3).with_language(*b"eng")).collect();
        assert!(matches!(pmt(1, 0x100, &st), Err(Error::Config(_))));
    }
}
