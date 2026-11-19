//! Tag bodies: the bytes after an FLV tag's 11-byte header, which are also the body of the RTMP
//! audio (type 8), video (type 9) and AMF0 data (type 18) messages.

use std::io::{self, IoSlice, Write};

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

/// One tag's body. [`Body::check`] checks it once and returns a [`Checked`] body with its length,
/// which can then be written to any number of outputs without being checked again.
///
/// A body sent to several destinations is checked once and the same [`Checked`] value is written
/// to each one. Annex B input costs a start code scan to check and another to write; convert it
/// once with [`annexb_to_nalus`] and send [`Body::AvcNalus`] to every destination instead.
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

impl<'a> Body<'a> {
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
    /// the data once and allocates nothing. To write the body after measuring it, use
    /// [`Body::check`], which keeps the length so the data is not read again.
    pub fn size(&self) -> Result<usize, Error> {
        self.check().map(|c| c.len)
    }

    /// Checks the body and returns it with its length. The check reads the data once and
    /// allocates nothing.
    pub fn check(&self) -> Result<Checked<'a>, Error> {
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
                if is_adts_frame(frame) {
                    return Err(Error::BadInput("the AAC frame is a whole ADTS frame; pass the raw frame without its header"));
                }
                2 + frame.len()
            }
            Body::Script(data) => {
                if data.first() != Some(&crate::amf0::marker::STRING) {
                    return Err(Error::BadInput("script data does not start with an AMF0 string"));
                }
                // The name and one value, under the limits `parse_script` reads them with.
                const BAD: Error = Error::BadInput("script data is not an AMF0 name string followed by one value that reads back");
                let name = crate::amf0::skip(data).map_err(|_| BAD)?;
                if name + crate::amf0::skip(&data[name..]).map_err(|_| BAD)? != data.len() {
                    return Err(BAD);
                }
                data.len()
            }
        };
        if len > MAX_TAG_DATA {
            return Err(Error::TooLarge { size: len, limit: MAX_TAG_DATA });
        }
        Ok(Checked { body: *self, len })
    }

    /// Checks the body and writes it to `out`, returning its length.
    pub fn write_to<W: Write + ?Sized>(&self, out: &mut W) -> Result<usize, Error> {
        Ok(self.check()?.write_to(out)?)
    }

    /// The bytes before the data and the data, for every body but Annex B.
    fn parts(&self) -> Option<([u8; 5], usize, &'a [u8])> {
        Some(match *self {
            Body::AvcSequenceHeader(avcc) => ([FRAME_KEY << 4 | CODEC_AVC, AVC_SEQUENCE_HEADER, 0, 0, 0], 5, avcc),
            Body::AvcAnnexB { .. } => return None,
            Body::AvcNalus { key, composition_time, data } => (video_header(key, AVC_NALU, composition_time), 5, data),
            Body::AvcEndOfSequence => ([FRAME_KEY << 4 | CODEC_AVC, AVC_END_OF_SEQUENCE, 0, 0, 0], 5, &[]),
            Body::AacSequenceHeader(asc) => ([AAC_TAG_HEADER, AAC_SEQUENCE_HEADER, 0, 0, 0], 2, asc),
            Body::AacRaw(frame) => ([AAC_TAG_HEADER, AAC_RAW, 0, 0, 0], 2, frame),
            Body::Script(data) => ([0; 5], 0, data),
        })
    }

    /// Writes `head`, a body that [`Body::check`] accepted, and `tail`. Every body but Annex B goes
    /// out in one vectored write.
    pub(crate) fn emit<W: Write + ?Sized>(&self, out: &mut W, head: &[u8], tail: &[u8]) -> io::Result<()> {
        match (self, self.parts()) {
            (_, Some((prefix, n, data))) => {
                write_all_vectored(out, &mut [IoSlice::new(head), IoSlice::new(&prefix[..n]), IoSlice::new(data), IoSlice::new(tail)])
            }
            (Body::AvcAnnexB { key, composition_time, data }, None) => {
                out.write_all(head)?;
                out.write_all(&video_header(*key, AVC_NALU, *composition_time))?;
                for nal in AnnexB::new(data) {
                    // Each NAL unit is shorter than MAX_TAG_DATA, which the check made sure of.
                    out.write_all(&(nal.len() as u32).to_be_bytes())?;
                    out.write_all(nal)?;
                }
                out.write_all(tail)
            }
            (_, None) => Ok(()),
        }
    }
}

/// A body that [`Body::check`] accepted, with its length. Writing it does not check it again, so
/// one checked body can go to several outputs, such as the chunk streams of several RTMP servers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Checked<'a> {
    body: Body<'a>,
    len: usize,
}

impl<'a> Checked<'a> {
    /// The body.
    pub fn body(&self) -> &Body<'a> {
        &self.body
    }

    /// The number of bytes [`Checked::write_to`] writes: the length of an RTMP message or the
    /// DataSize of an FLV tag.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the body is empty. A checked body never is; every body has at least one byte.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The tag type, which is also the RTMP message type id.
    pub fn tag_type(&self) -> TagType {
        self.body.tag_type()
    }

    /// Writes the body to `out` and returns its length. Every body but Annex B goes out in one
    /// vectored write.
    pub fn write_to<W: Write + ?Sized>(&self, out: &mut W) -> io::Result<usize> {
        self.body.emit(out, &[], &[])?;
        Ok(self.len)
    }

    /// The body as its codec header and its data, which together are the bytes
    /// [`Checked::write_to`] writes, for every body but [`Body::AvcAnnexB`] (`None`). A sender that
    /// splits a message into pieces, such as RTMP chunks, writes the two slices without copying
    /// them into one buffer.
    pub fn parts(&self) -> Option<Parts<'a>> {
        self.body.parts().map(|(header, n, data)| Parts { header, header_len: n as u8, data })
    }
}

/// A checked body's codec header and data, from [`Checked::parts`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Parts<'a> {
    header: [u8; 5],
    header_len: u8,
    data: &'a [u8],
}

impl<'a> Parts<'a> {
    /// The bytes before the data: 5 for video, 2 for audio, none for script data.
    pub fn header(&self) -> &[u8] {
        &self.header[..usize::from(self.header_len)]
    }

    /// The data after the header.
    pub fn data(&self) -> &'a [u8] {
        self.data
    }
}

/// Writes every slice, as `Write::write_all` does for one.
fn write_all_vectored<W: Write + ?Sized>(out: &mut W, mut bufs: &mut [IoSlice<'_>]) -> io::Result<()> {
    IoSlice::advance_slices(&mut bufs, 0);
    while !bufs.is_empty() {
        match out.write_vectored(bufs) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => IoSlice::advance_slices(&mut bufs, n),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Appends the NAL units of an Annex B access unit to `out`, each preceded by its 4-byte length,
/// and returns the bytes appended. The data is scanned once.
///
/// A sender with several destinations converts each access unit once and writes the result as
/// [`Body::AvcNalus`] to every destination, sharing the one buffer. An access unit that does not
/// start with a start code, holds no NAL unit or would not fit in a tag body is refused, and
/// `out` is then left as it was.
pub fn annexb_to_nalus(data: &[u8], out: &mut Vec<u8>) -> Result<usize, Error> {
    if start_code_at(data, 0).is_none() {
        return Err(Error::BadInput("the Annex B access unit does not start with a start code"));
    }
    let start = out.len();
    for nal in AnnexB::new(data) {
        let len = out.len() - start + 4 + nal.len();
        if len > MAX_TAG_DATA - 5 {
            out.truncate(start);
            return Err(Error::TooLarge { size: 5 + len, limit: MAX_TAG_DATA });
        }
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }
    if out.len() == start {
        return Err(Error::BadInput("the Annex B access unit holds no NAL unit"));
    }
    Ok(out.len() - start)
}

fn video_header(key: bool, packet_type: u8, composition_time: i32) -> [u8; 5] {
    let ct = composition_time.to_be_bytes();
    [(if key { FRAME_KEY } else { FRAME_INTER }) << 4 | CODEC_AVC, packet_type, ct[1], ct[2], ct[3]]
}

fn check_composition_time(ct: i32) -> Result<(), Error> {
    if (MIN_COMPOSITION_TIME..=MAX_COMPOSITION_TIME).contains(&ct) { Ok(()) } else { Err(Error::CompositionTime(ct)) }
}

/// Whether `data` is one or more whole ADTS frames back to back (ISO/IEC 14496-3 §1.A.2.2): each
/// with the 12-bit sync word, layer 0, and a frame_length of at least 7 that ends inside the data,
/// the last ending at its end. A raw frame that only starts with the sync word's bits is not
/// refused.
fn is_adts_frame(data: &[u8]) -> bool {
    let mut rest = data;
    while let [0xFF, b1, _, b3, b4, b5, _, ..] = rest {
        if b1 & 0xF6 != 0xF0 {
            return false;
        }
        let len = usize::from(b3 & 3) << 11 | usize::from(*b4) << 3 | usize::from(b5 >> 5);
        if len < 7 || len > rest.len() {
            return false;
        }
        rest = &rest[len..];
        if rest.is_empty() {
            return true;
        }
    }
    false
}

fn check_avcc(avcc: &[u8]) -> Result<(), Error> {
    avcc_problem(avcc).map_or(Ok(()), |p| Err(Error::BadInput(p)))
}

/// What is wrong with an AVCDecoderConfigurationRecord, or `None`: configurationVersion 1, 4-byte
/// NAL unit lengths, and every sequence and picture parameter set its counts declare, each a 16-bit
/// length and that many bytes, inside the record. Bytes after the picture parameter sets (the
/// extension that some profiles add) are allowed.
pub(crate) fn avcc_problem(avcc: &[u8]) -> Option<&'static str> {
    if avcc.len() < 7 || avcc[0] != 1 {
        return Some("the avcC record is shorter than 7 bytes or its configurationVersion is not 1");
    }
    if avcc[4] & 3 != 3 {
        return Some("the avcC record does not use 4-byte NAL unit lengths");
    }
    const CUT: &str = "the avcC record ends inside a parameter set it declares";
    let mut pos = 6;
    let skip_sets = |pos: &mut usize, count: u8| {
        for _ in 0..count {
            let len = match avcc.get(*pos..*pos + 2) {
                Some(&[a, b]) => usize::from(u16::from_be_bytes([a, b])),
                _ => return false,
            };
            *pos += 2 + len;
            if *pos > avcc.len() {
                return false;
            }
        }
        true
    };
    if !skip_sets(&mut pos, avcc[5] & 0x1F) {
        return Some(CUT);
    }
    let Some(&pps) = avcc.get(pos) else { return Some(CUT) };
    pos += 1;
    if !skip_sets(&mut pos, pps) {
        return Some(CUT);
    }
    None
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

/// The length of an Annex B access unit written with 4-byte lengths. The scan stops as soon as
/// the tag body would be longer than [`MAX_TAG_DATA`].
fn annexb_len(data: &[u8]) -> Result<usize, Error> {
    if start_code_at(data, 0).is_none() {
        return Err(Error::BadInput("the Annex B access unit does not start with a start code"));
    }
    let mut len = 0usize;
    let mut count = 0usize;
    for nal in AnnexB::new(data) {
        len += 4 + nal.len();
        count += 1;
        if 5 + len > MAX_TAG_DATA {
            return Err(Error::TooLarge { size: 5 + len, limit: MAX_TAG_DATA });
        }
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
