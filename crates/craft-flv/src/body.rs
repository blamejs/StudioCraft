//! Tag bodies: the bytes after an FLV tag's 11-byte header, which are also the body of the RTMP
//! audio (type 8), video (type 9) and AMF0 data (type 18) messages.

use std::io::Write;

use crate::{Error, MAX_TAG_DATA, TagType};

/// The CodecID of H.264 (AVC) in the video tag header.
pub const CODEC_AVC: u8 = 7;
/// The SoundFormat of AAC in the audio tag header.
pub const SOUND_FORMAT_AAC: u8 = 10;
/// The first byte of every AAC audio tag: SoundFormat 10, SoundRate 3, 16-bit samples, stereo.
/// The FLV specification (E.4.2.1) asks for these values with AAC; decoders read the real rate and
/// channels from the AudioSpecificConfig.
pub const AAC_TAG_HEADER: u8 = SOUND_FORMAT_AAC << 4 | 3 << 2 | 1 << 1 | 1;
/// The smallest composition time offset, in milliseconds (a signed 24-bit field).
pub const MIN_COMPOSITION_TIME: i32 = -0x80_0000;
/// The largest composition time offset, in milliseconds.
pub const MAX_COMPOSITION_TIME: i32 = 0x7F_FFFF;

const FRAME_KEY: u8 = 1;
const FRAME_INTER: u8 = 2;
const AVC_SEQUENCE_HEADER: u8 = 0;
const AVC_NALU: u8 = 1;
const AVC_END_OF_SEQUENCE: u8 = 2;
const AAC_SEQUENCE_HEADER: u8 = 0;
const AAC_RAW: u8 = 1;

/// One tag's body. [`Body::size`] checks it and gives its length; [`Body::write_to`] writes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Body<'a> {
    /// The AVC sequence header: an AVCDecoderConfigurationRecord (`avcC`, ISO/IEC 14496-15
    /// §5.2.4.1) with configurationVersion 1 and 4-byte NAL unit lengths (lengthSizeMinusOne 3).
    AvcSequenceHeader(&'a [u8]),
    /// One H.264 access unit in Annex B form (start codes). It is written with a 4-byte length
    /// in place of each start code; trailing zero bytes before a start code are left out.
    AvcAnnexB {
        /// A keyframe (an IDR picture).
        key: bool,
        /// The presentation time minus the decoding time, in milliseconds.
        composition_time: i32,
        /// The access unit, starting with a start code.
        data: &'a [u8],
    },
    /// One H.264 access unit as NAL units each preceded by a 4-byte big-endian length, as MP4
    /// samples carry them. The lengths must cover the data exactly.
    AvcNalus {
        /// A keyframe (an IDR picture).
        key: bool,
        /// The presentation time minus the decoding time, in milliseconds.
        composition_time: i32,
        /// The length-prefixed NAL units.
        data: &'a [u8],
    },
    /// The end of the AVC sequence.
    AvcEndOfSequence,
    /// The AAC sequence header: an AudioSpecificConfig (ISO/IEC 14496-3 §1.6.2.1).
    AacSequenceHeader(&'a [u8]),
    /// One raw AAC frame, without an ADTS header.
    AacRaw(&'a [u8]),
    /// Script data already encoded in AMF0: a name string followed by its value, such as the
    /// bytes [`crate::Metadata::encode`] writes.
    Script(&'a [u8]),
}

impl Body<'_> {
    /// The tag type, which is also the RTMP message type id.
    pub fn tag_type(&self) -> TagType {
        match self {
            Body::AvcSequenceHeader(_) | Body::AvcAnnexB { .. } | Body::AvcNalus { .. } | Body::AvcEndOfSequence => TagType::Video,
            Body::AacSequenceHeader(_) | Body::AacRaw(_) => TagType::Audio,
            Body::Script(_) => TagType::Script,
        }
    }

    /// Whether this is a video keyframe or a sequence header, where a reader can start.
    pub fn is_key(&self) -> bool {
        match self {
            Body::AvcSequenceHeader(_) | Body::AacSequenceHeader(_) | Body::Script(_) => true,
            Body::AvcAnnexB { key, .. } | Body::AvcNalus { key, .. } => *key,
            Body::AvcEndOfSequence | Body::AacRaw(_) => false,
        }
    }

    /// Checks the body and returns the number of bytes [`Body::write_to`] writes. The check reads
    /// the data once and allocates nothing.
    pub fn size(&self) -> Result<usize, Error> {
        let len = match self {
            Body::AvcSequenceHeader(avcc) => {
                check_avcc(avcc)?;
                5 + avcc.len()
            }
            Body::AvcAnnexB { composition_time, data, .. } => {
                check_composition_time(*composition_time)?;
                5 + annexb_len(data)?
            }
            Body::AvcNalus { composition_time, data, .. } => {
                check_composition_time(*composition_time)?;
                check_nalus(data)?;
                5 + data.len()
            }
            Body::AvcEndOfSequence => 5,
            Body::AacSequenceHeader(asc) => {
                if asc.len() < 2 || asc[0] >> 3 == 0 {
                    return Err(Error::BadInput("the AudioSpecificConfig is shorter than 2 bytes or has audio object type 0"));
                }
                2 + asc.len()
            }
            Body::AacRaw(frame) => {
                if frame.is_empty() {
                    return Err(Error::BadInput("the AAC frame is empty"));
                }
                if frame.len() >= 7 && frame[0] == 0xFF && frame[1] & 0xF0 == 0xF0 {
                    return Err(Error::BadInput("the AAC frame starts with an ADTS header; pass the raw frame"));
                }
                2 + frame.len()
            }
            Body::Script(data) => {
                if data.first() != Some(&crate::amf0::marker::STRING) {
                    return Err(Error::BadInput("script data does not start with an AMF0 string"));
                }
                data.len()
            }
        };
        if len > MAX_TAG_DATA {
            return Err(Error::TooLarge { size: len, limit: MAX_TAG_DATA });
        }
        Ok(len)
    }

    /// Checks the body and writes it to `out`, returning its length.
    pub fn write_to<W: Write + ?Sized>(&self, out: &mut W) -> Result<usize, Error> {
        let len = self.size()?;
        self.emit(out)?;
        Ok(len)
    }

    /// Writes a body that [`Body::size`] accepted.
    pub(crate) fn emit<W: Write + ?Sized>(&self, out: &mut W) -> std::io::Result<()> {
        match self {
            Body::AvcSequenceHeader(avcc) => {
                out.write_all(&[FRAME_KEY << 4 | CODEC_AVC, AVC_SEQUENCE_HEADER, 0, 0, 0])?;
                out.write_all(avcc)
            }
            Body::AvcAnnexB { key, composition_time, data } => {
                out.write_all(&video_header(*key, AVC_NALU, *composition_time))?;
                for nal in AnnexB::new(data) {
                    // Each NAL unit is shorter than MAX_TAG_DATA, which `len` checked.
                    out.write_all(&(nal.len() as u32).to_be_bytes())?;
                    out.write_all(nal)?;
                }
                Ok(())
            }
            Body::AvcNalus { key, composition_time, data } => {
                out.write_all(&video_header(*key, AVC_NALU, *composition_time))?;
                out.write_all(data)
            }
            Body::AvcEndOfSequence => out.write_all(&[FRAME_KEY << 4 | CODEC_AVC, AVC_END_OF_SEQUENCE, 0, 0, 0]),
            Body::AacSequenceHeader(asc) => {
                out.write_all(&[AAC_TAG_HEADER, AAC_SEQUENCE_HEADER])?;
                out.write_all(asc)
            }
            Body::AacRaw(frame) => {
                out.write_all(&[AAC_TAG_HEADER, AAC_RAW])?;
                out.write_all(frame)
            }
            Body::Script(data) => out.write_all(data),
        }
    }
}

fn video_header(key: bool, packet_type: u8, composition_time: i32) -> [u8; 5] {
    let ct = composition_time.to_be_bytes();
    [(if key { FRAME_KEY } else { FRAME_INTER }) << 4 | CODEC_AVC, packet_type, ct[1], ct[2], ct[3]]
}

fn check_composition_time(ct: i32) -> Result<(), Error> {
    if (MIN_COMPOSITION_TIME..=MAX_COMPOSITION_TIME).contains(&ct) { Ok(()) } else { Err(Error::CompositionTime(ct)) }
}

fn check_avcc(avcc: &[u8]) -> Result<(), Error> {
    if avcc.len() < 7 || avcc[0] != 1 {
        return Err(Error::BadInput("the avcC record is shorter than 7 bytes or its configurationVersion is not 1"));
    }
    if avcc[4] & 3 != 3 {
        return Err(Error::BadInput("the avcC record does not use 4-byte NAL unit lengths"));
    }
    Ok(())
}

fn check_nalus(data: &[u8]) -> Result<(), Error> {
    nalus_problem(data).map_or(Ok(()), |p| Err(Error::BadInput(p)))
}

/// What is wrong with a length-prefixed access unit with 4-byte lengths, or `None`: it must hold
/// at least one NAL unit, and its lengths must be above 0 and cover the data exactly.
pub(crate) fn nalus_problem(data: &[u8]) -> Option<&'static str> {
    if data.is_empty() {
        return Some("the access unit is empty");
    }
    let mut rest = data;
    while !rest.is_empty() {
        if rest.len() < 4 {
            return Some("a NAL unit length is cut off");
        }
        let n = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
        if n == 0 || n > rest.len() - 4 {
            return Some("a NAL unit length is 0 or runs past the access unit");
        }
        rest = &rest[4 + n..];
    }
    None
}

/// The length of an Annex B access unit written with 4-byte lengths.
fn annexb_len(data: &[u8]) -> Result<usize, Error> {
    if start_code_at(data, 0).is_none() {
        return Err(Error::BadInput("the Annex B access unit does not start with a start code"));
    }
    let mut len = 0usize;
    let mut count = 0usize;
    for nal in AnnexB::new(data) {
        len += 4 + nal.len();
        count += 1;
    }
    if count == 0 {
        return Err(Error::BadInput("the Annex B access unit holds no NAL unit"));
    }
    Ok(len)
}

/// The length of the start code (3 or 4 bytes) at `i`, if there is one.
fn start_code_at(data: &[u8], i: usize) -> Option<usize> {
    match data.get(i..i + 4) {
        Some([0, 0, 0, 1]) => Some(4),
        _ => match data.get(i..i + 3) {
            Some([0, 0, 1]) => Some(3),
            _ => None,
        },
    }
}

/// The NAL units of an Annex B byte stream, without start codes and trailing zero bytes. Empty
/// NAL units are skipped. Bytes before the first start code are skipped too.
pub struct AnnexB<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> AnnexB<'a> {
    /// Splits `data`.
    pub fn new(data: &'a [u8]) -> Self {
        let pos = find_start(data, 0).map_or(data.len(), |(i, sc)| i + sc);
        AnnexB { data, pos }
    }
}

/// The next start code at or after `from`, as (position, length).
fn find_start(data: &[u8], from: usize) -> Option<(usize, usize)> {
    const ONES: u64 = 0x0101_0101_0101_0101;
    const HIGHS: u64 = 0x8080_8080_8080_8080;
    let mut i = from;
    while i + 3 <= data.len() {
        // A start code starts with a zero byte, so skip 8 bytes that hold none.
        if let Some(word) = data.get(i..i + 8) {
            let mut a = [0u8; 8];
            a.copy_from_slice(word);
            let w = u64::from_le_bytes(a);
            if w.wrapping_sub(ONES) & !w & HIGHS == 0 {
                i += 8;
                continue;
            }
        }
        // A start code has 0x01 at its third byte, so skip ahead on anything above 1.
        if data[i + 2] > 1 {
            i += 3;
            continue;
        }
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            return Some((i, 3));
        }
        i += 1;
    }
    None
}

impl<'a> Iterator for AnnexB<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        while self.pos < self.data.len() {
            let start = self.pos;
            let (end, next) = match find_start(self.data, start) {
                Some((i, sc)) => (i, i + sc),
                None => (self.data.len(), self.data.len()),
            };
            self.pos = next;
            let mut nal = &self.data[start..end];
            while let [rest @ .., 0] = nal {
                nal = rest;
            }
            if !nal.is_empty() {
                return Some(nal);
            }
        }
        None
    }
}

/// The NAL units of a length-prefixed access unit with 4-byte lengths, as [`crate::Video::AvcNalus`]
/// carries them. Iteration stops at the first length that runs past the data.
pub struct Nalus<'a> {
    rest: &'a [u8],
}

impl<'a> Nalus<'a> {
    /// Splits `data`.
    pub fn new(data: &'a [u8]) -> Self {
        Nalus { rest: data }
    }
}

impl<'a> Iterator for Nalus<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        let r = self.rest;
        if r.len() < 4 {
            return None;
        }
        let n = u32::from_be_bytes([r[0], r[1], r[2], r[3]]) as usize;
        if n > r.len() - 4 {
            self.rest = &[];
            return None;
        }
        self.rest = &r[4 + n..];
        Some(&r[4..4 + n])
    }
}
