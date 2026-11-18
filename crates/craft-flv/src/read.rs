//! Reading FLV: the file header, tags one at a time, and the audio, video and script bodies.

use std::io::{self, Read};

use crate::amf0::{self, Value};
use crate::body::{CODEC_AVC, SOUND_FORMAT_AAC, nalus_problem};
use crate::{Error, HEADER_LEN, Header, TAG_HEADER_LEN, TagType};

/// One tag read by [`Reader::next_tag`]. `data` is the tag's body, which is also the body of the
/// RTMP message of the same type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tag<'a> {
    /// Audio, video or script data.
    pub kind: TagType,
    /// The decoding time in milliseconds, read as the signed 32-bit value the specification gives
    /// (E.4.1: TimestampExtended is the upper 8 bits of an SI32).
    pub timestamp: i32,
    /// The body.
    pub data: &'a [u8],
}

/// Reads FLV tags from a byte stream, such as the standard output of a process writing FLV to a
/// pipe.
///
/// Each tag is read into a buffer the caller passes in and reuses, so after the largest tag has
/// been seen no tag allocates. The buffer grows as the bytes arrive, not to the length a tag header
/// claims, and it keeps its capacity until the caller shrinks it. A tag's data is at most
/// [`crate::MAX_TAG_DATA`] bytes.
///
/// The source must block until bytes arrive. A read that fails partway through a tag, such as a
/// `WouldBlock` or `TimedOut` from a socket with a read timeout, loses the bytes already taken, so
/// after any error the reader refuses every later tag with [`Error::Broken`].
pub struct Reader<R> {
    inner: R,
    header: Header,
    tags: u64,
    broken: bool,
}

impl<R: Read> Reader<R> {
    /// Reads the file header and the first PreviousTagSize, which must be 0. A signature other
    /// than `FLV`, a version other than 1 or a DataOffset under 9 is an error; bytes past 9 in a
    /// larger header are skipped.
    pub fn new(mut inner: R) -> Result<Self, Error> {
        let mut h = [0u8; HEADER_LEN];
        if read_full(&mut inner, &mut h)? < HEADER_LEN {
            return Err(Error::Truncated("the FLV header"));
        }
        let header = Header::parse(&h)?;
        let offset = u32::from_be_bytes([h[5], h[6], h[7], h[8]]) as u64;
        let extra = offset - HEADER_LEN as u64;
        if io::copy(&mut (&mut inner).take(extra), &mut io::sink())? < extra {
            return Err(Error::Truncated("the FLV header"));
        }
        let mut p = [0u8; 4];
        if read_full(&mut inner, &mut p)? < 4 {
            return Err(Error::Truncated("the first PreviousTagSize"));
        }
        let found = u32::from_be_bytes(p);
        if found != 0 {
            return Err(Error::PreviousTagSize { expected: 0, found });
        }
        Ok(Reader { inner, header, tags: 0, broken: false })
    }

    /// The file header.
    pub fn header(&self) -> Header {
        self.header
    }

    /// Tags read so far.
    pub fn tags(&self) -> u64 {
        self.tags
    }

    /// The source, for example to close a pipe.
    pub fn into_inner(self) -> R {
        self.inner
    }

    /// Whether an earlier call failed, so the reader refuses every later tag.
    pub fn is_broken(&self) -> bool {
        self.broken
    }

    /// Reads the next tag into `buf` and returns it, or `None` when the stream ends after a
    /// PreviousTagSize. A stream that ends inside a tag, a tag type other than 8, 9 or 18, a set
    /// Filter bit (an encrypted tag), a StreamID other than 0 or a PreviousTagSize that does not
    /// match the tag is an error. After any error, every later call returns [`Error::Broken`].
    pub fn next_tag<'b>(&mut self, buf: &'b mut Vec<u8>) -> Result<Option<Tag<'b>>, Error> {
        if self.broken {
            return Err(Error::Broken);
        }
        match self.read_tag(buf) {
            Ok(Some((kind, timestamp))) => {
                self.tags += 1;
                Ok(Some(Tag { kind, timestamp, data: buf }))
            }
            Ok(None) => Ok(None),
            Err(e) => {
                self.broken = true;
                Err(e)
            }
        }
    }

    fn read_tag(&mut self, buf: &mut Vec<u8>) -> Result<Option<(TagType, i32)>, Error> {
        let mut h = [0u8; TAG_HEADER_LEN];
        match read_full(&mut self.inner, &mut h)? {
            0 => return Ok(None),
            TAG_HEADER_LEN => {}
            _ => return Err(Error::Truncated("a tag header")),
        }
        if h[0] & 0x20 != 0 {
            return Err(Error::Unsupported("encrypted FLV tags"));
        }
        if h[0] & 0xC0 != 0 {
            return Err(Error::Invalid("a tag's reserved bits are set"));
        }
        let kind = TagType::from_u8(h[0] & 0x1F).ok_or(Error::Invalid("a tag type other than audio, video or script data"))?;
        let size = u32::from_be_bytes([0, h[1], h[2], h[3]]) as usize;
        let timestamp = i32::from_be_bytes([h[7], h[4], h[5], h[6]]);
        if h[8..11] != [0, 0, 0] {
            return Err(Error::Invalid("a tag's StreamID is not 0"));
        }
        // Reading through `take` fills the buffer as the bytes arrive, without zeroing it first or
        // reserving a length the header claims before the data is there.
        buf.clear();
        if (&mut self.inner).take(size as u64).read_to_end(buf)? < size {
            return Err(Error::Truncated("a tag's data"));
        }
        let mut p = [0u8; 4];
        if read_full(&mut self.inner, &mut p)? < 4 {
            return Err(Error::Truncated("a PreviousTagSize"));
        }
        let found = u32::from_be_bytes(p);
        let expected = (TAG_HEADER_LEN + size) as u32;
        if found != expected {
            return Err(Error::PreviousTagSize { expected, found });
        }
        Ok(Some((kind, timestamp)))
    }
}

/// Reads until `buf` is full or the source ends; returns the bytes read.
fn read_full<R: Read + ?Sized>(r: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// A video tag's body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Video<'a> {
    /// The AVC sequence header: the AVCDecoderConfigurationRecord.
    AvcSequenceHeader(&'a [u8]),
    /// One H.264 access unit as length-prefixed NAL units ([`crate::Nalus`] splits them).
    AvcNalus {
        /// A keyframe.
        key: bool,
        /// The presentation time minus the decoding time, in milliseconds.
        composition_time: i32,
        /// The NAL units.
        data: &'a [u8],
    },
    /// The end of the AVC sequence.
    AvcEndOfSequence,
    /// A frame of another codec, or a video info or command frame.
    Other {
        /// The FrameType field.
        frame_type: u8,
        /// The CodecID field.
        codec: u8,
    },
}

/// Reads a video tag's body. An AVC access unit must be NAL units with 4-byte lengths that cover
/// it exactly, as [`crate::Writer`] writes them; other lengths are [`Error::Invalid`]. Only
/// FrameType 1 to 4 with CodecID 7 is read as AVC. FrameType 5 (video info or command frames),
/// the reserved values 0, 6 and 7, and a first byte with the high bit set (the extended video
/// header of later revisions, whose low four bits are a packet type) are returned as
/// [`Video::Other`].
pub fn parse_video(data: &[u8]) -> Result<Video<'_>, Error> {
    let first = *data.first().ok_or(Error::Truncated("a video tag"))?;
    let frame_type = first >> 4;
    let codec = first & 0x0F;
    if codec != CODEC_AVC || !(1..=4).contains(&frame_type) {
        return Ok(Video::Other { frame_type, codec });
    }
    if data.len() < 5 {
        return Err(Error::Truncated("an AVC video tag header"));
    }
    let ct = i32::from_be_bytes([0, data[2], data[3], data[4]]) << 8 >> 8;
    let rest = &data[5..];
    match data[1] {
        0 if rest.is_empty() => Err(Error::Truncated("an AVC sequence header")),
        0 => Ok(Video::AvcSequenceHeader(rest)),
        1 => match nalus_problem(rest) {
            Some(p) => Err(Error::Invalid(p)),
            None => Ok(Video::AvcNalus { key: frame_type == 1, composition_time: ct, data: rest }),
        },
        2 => Ok(Video::AvcEndOfSequence),
        _ => Err(Error::Invalid("an AVCPacketType other than 0, 1 or 2")),
    }
}

/// An audio tag's body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Audio<'a> {
    /// The AAC sequence header: the AudioSpecificConfig.
    AacSequenceHeader(&'a [u8]),
    /// One raw AAC frame.
    AacRaw(&'a [u8]),
    /// Audio in another format.
    Other {
        /// The SoundFormat field.
        format: u8,
    },
}

/// Reads an audio tag's body. An empty raw AAC frame is [`Error::Invalid`], as
/// [`crate::Body::AacRaw`] refuses one.
pub fn parse_audio(data: &[u8]) -> Result<Audio<'_>, Error> {
    let first = *data.first().ok_or(Error::Truncated("an audio tag"))?;
    if first >> 4 != SOUND_FORMAT_AAC {
        return Ok(Audio::Other { format: first >> 4 });
    }
    let packet_type = *data.get(1).ok_or(Error::Truncated("an AAC audio tag header"))?;
    let rest = &data[2..];
    match packet_type {
        0 if rest.is_empty() => Err(Error::Truncated("an AAC sequence header")),
        0 => Ok(Audio::AacSequenceHeader(rest)),
        1 if rest.is_empty() => Err(Error::Invalid("an AAC frame is empty")),
        1 => Ok(Audio::AacRaw(rest)),
        _ => Err(Error::Invalid("an AACPacketType other than 0 or 1")),
    }
}

/// Reads a script data tag's body: the method or object name and its value. Bytes after the value
/// are an error.
pub fn parse_script(data: &[u8]) -> Result<(String, Value), Error> {
    let (name, n) = amf0::read(data)?;
    let Value::String(name) = name else {
        return Err(Error::Invalid("script data does not start with a string"));
    };
    let (value, m) = amf0::read(&data[n..])?;
    if n + m != data.len() {
        return Err(Error::Invalid("script data has bytes after its value"));
    }
    Ok((name, value))
}
