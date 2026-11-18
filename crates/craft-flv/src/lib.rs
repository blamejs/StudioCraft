//! FLV for H.264 and AAC (Video File Format Specification version 10.1, Annex E).
//!
//! - [`Writer`] writes an FLV stream to any [`Write`]: the file header, then tags, each followed
//!   by its PreviousTagSize.
//! - [`Body`] is one tag's body: the AVC and AAC sequence headers, H.264 access units (Annex B or
//!   length-prefixed) with their composition time, raw AAC frames, the end of the AVC sequence,
//!   and script data. The same bytes are the body of an RTMP audio, video or data message, so an
//!   RTMP sender checks a body once with [`Body::check`] and writes the [`Checked`] body into the
//!   chunks of every destination without the FLV tag header.
//! - [`annexb_to_nalus`] converts an Annex B access unit to length-prefixed NAL units once, so
//!   several destinations share one buffer instead of each scanning for start codes.
//! - [`Metadata`] encodes the `onMetaData` script data.
//! - [`Reader`] reads tags one at a time from a byte stream, such as a process writing FLV to a
//!   pipe, and [`parse_video`], [`parse_audio`] and [`parse_script`] read their bodies.
//! - [`amf0`] writes and reads the AMF0 values of script data and RTMP commands.
//!
//! # Cost
//!
//! [`Writer::write`] builds the 11-byte tag header and the 4-byte PreviousTagSize on the stack and
//! writes the body straight from the caller's slice: it allocates nothing and copies the frame only
//! into the output. Every body but Annex B goes out in one vectored write with its tag header and
//! PreviousTagSize. An Annex B access unit is read twice, once to measure it and once to write it
//! with 4-byte lengths in place of the start codes, and it takes two writes per NAL unit. With
//! several destinations, convert it once with [`annexb_to_nalus`]. [`Reader::next_tag`] reads into
//! a buffer the caller reuses, as the bytes arrive.
//!
//! No dependencies beyond `std`, no `unsafe`, builds for `wasm32-unknown-unknown`.

#![warn(missing_docs)]

pub mod amf0;
mod body;
mod read;

use std::fmt;
use std::io::{self, Write};

pub use body::{
    AAC_TAG_HEADER, AnnexB, Body, CODEC_AVC, Checked, MAX_COMPOSITION_TIME, MIN_COMPOSITION_TIME, Nalus, SOUND_FORMAT_AAC, annexb_to_nalus,
};
pub use read::{Audio, Reader, Tag, Video, parse_audio, parse_script, parse_video};

use amf0::Value;

/// Bytes in the FLV version 1 file header.
pub const HEADER_LEN: usize = 9;
/// Bytes in a tag header.
pub const TAG_HEADER_LEN: usize = 11;
/// The largest tag body: DataSize is a 24-bit field.
pub const MAX_TAG_DATA: usize = 0xFF_FFFF;
/// The largest file header [`Reader`] skips. Version 1 headers are 9 bytes; the specification
/// leaves room for larger ones in later versions.
pub const MAX_DATA_OFFSET: u32 = 0x1_0000;

/// An error from writing or reading FLV.
#[derive(Debug)]
pub enum Error {
    /// Writing the output or reading the input failed.
    Io(io::Error),
    /// The input ends inside this part.
    Truncated(&'static str),
    /// The input breaks the format here.
    Invalid(&'static str),
    /// The input uses a part of the format this crate does not read.
    Unsupported(&'static str),
    /// A value passed in to be written is not valid.
    BadInput(&'static str),
    /// A tag body would be longer than [`MAX_TAG_DATA`].
    TooLarge {
        /// Bytes in the body. For Annex B input, the check stops at the first NAL unit that takes
        /// the body past the limit, and this is the length up to that NAL unit.
        size: usize,
        /// Most bytes a body may have.
        limit: usize,
    },
    /// A composition time outside [`MIN_COMPOSITION_TIME`] to [`MAX_COMPOSITION_TIME`].
    CompositionTime(i32),
    /// A timestamp that is negative or earlier than the last tag of the same type.
    Timestamp {
        /// The timestamp passed in, in milliseconds.
        timestamp: i32,
        /// The smallest timestamp allowed.
        min: i32,
    },
    /// A PreviousTagSize that does not match the tag before it.
    PreviousTagSize {
        /// The size of the tag before it.
        expected: u32,
        /// The value read.
        found: u32,
    },
    /// An earlier read or write failed partway through a tag, so the stream is out of step. A
    /// [`Writer`] or [`Reader`] refuses every later tag; start again with a new one.
    Broken,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O: {e}"),
            Error::Truncated(s) => write!(f, "the FLV data ends inside {s}"),
            Error::Invalid(s) => write!(f, "FLV data not valid: {s}"),
            Error::Unsupported(s) => write!(f, "FLV data uses {s}, which is not read"),
            Error::BadInput(s) => write!(f, "{s}"),
            Error::TooLarge { size, limit } => write!(f, "the tag body is {size} bytes; the limit is {limit}"),
            Error::CompositionTime(ct) => {
                write!(f, "a composition time of {ct} ms is outside {MIN_COMPOSITION_TIME} to {MAX_COMPOSITION_TIME}")
            }
            Error::Timestamp { timestamp, min } => write!(f, "a timestamp of {timestamp} ms is before {min} ms"),
            Error::PreviousTagSize { expected, found } => write!(f, "a PreviousTagSize of {found} follows a tag of {expected} bytes"),
            Error::Broken => write!(f, "an earlier read or write failed partway through a tag"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

/// The tag types, which are also the RTMP message type ids of audio, video and AMF0 data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TagType {
    /// Audio (8).
    Audio = 8,
    /// Video (9).
    Video = 9,
    /// Script data in AMF0 (18).
    Script = 18,
}

impl TagType {
    /// The type for a TagType field value, or `None`.
    pub fn from_u8(v: u8) -> Option<TagType> {
        match v {
            8 => Some(TagType::Audio),
            9 => Some(TagType::Video),
            18 => Some(TagType::Script),
            _ => None,
        }
    }

    fn index(self) -> usize {
        match self {
            TagType::Audio => 0,
            TagType::Video => 1,
            TagType::Script => 2,
        }
    }
}

/// The FLV file header's flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Header {
    /// Audio tags are present.
    pub audio: bool,
    /// Video tags are present.
    pub video: bool,
}

impl Header {
    /// The 9-byte version 1 header followed by the first PreviousTagSize (0).
    pub fn bytes(&self) -> [u8; HEADER_LEN + 4] {
        let flags = u8::from(self.audio) << 2 | u8::from(self.video);
        [b'F', b'L', b'V', 1, flags, 0, 0, 0, HEADER_LEN as u8, 0, 0, 0, 0]
    }

    /// Reads a 9-byte header. A signature other than `FLV`, a version other than 1, set reserved
    /// bits, or a DataOffset under 9 or over [`MAX_DATA_OFFSET`] is an error.
    pub fn parse(h: &[u8; HEADER_LEN]) -> Result<Header, Error> {
        if &h[..3] != b"FLV" {
            return Err(Error::Invalid("the signature is not FLV"));
        }
        if h[3] != 1 {
            return Err(Error::Unsupported("an FLV version other than 1"));
        }
        if h[4] & !0x05 != 0 {
            return Err(Error::Invalid("the header's reserved flag bits are set"));
        }
        let offset = u32::from_be_bytes([h[5], h[6], h[7], h[8]]);
        if offset < HEADER_LEN as u32 {
            return Err(Error::Invalid("the header's DataOffset is under 9"));
        }
        if offset > MAX_DATA_OFFSET {
            return Err(Error::Unsupported("a DataOffset over 65 536 bytes"));
        }
        Ok(Header { audio: h[4] & 4 != 0, video: h[4] & 1 != 0 })
    }
}

/// An 11-byte tag header: TagType, DataSize, Timestamp with TimestampExtended, and StreamID 0.
/// A `data_size` over [`MAX_TAG_DATA`] is [`Error::TooLarge`], and a negative timestamp is
/// [`Error::Timestamp`], as [`Writer::write`] refuses them.
///
/// The timestamp is the signed 32-bit value the specification gives (E.4.1: TimestampExtended is
/// the upper 8 bits of an SI32), so the last time a tag can carry is `i32::MAX` milliseconds,
/// about 24.8 days. An RTMP message header carries its own 32-bit timestamp, which wraps; a sender
/// writes [`Body`] bytes under it and is not bound by this limit.
pub fn tag_header(kind: TagType, data_size: usize, timestamp: i32) -> Result<[u8; TAG_HEADER_LEN], Error> {
    if data_size > MAX_TAG_DATA {
        return Err(Error::TooLarge { size: data_size, limit: MAX_TAG_DATA });
    }
    if timestamp < 0 {
        return Err(Error::Timestamp { timestamp, min: 0 });
    }
    let s = (data_size as u32).to_be_bytes();
    let t = timestamp.to_be_bytes();
    Ok([kind as u8, s[1], s[2], s[3], t[1], t[2], t[3], t[0], 0, 0, 0])
}

/// Writes an FLV stream tag by tag.
///
/// Timestamps are decoding times in milliseconds. They must not be negative, and within each tag
/// type they must not go back ([`Error::Timestamp`]). The largest is `i32::MAX`, about 24.8 days
/// of stream; a stream that runs longer calls [`Writer::reset_timestamps`] and starts again from 0.
///
/// A tag is checked before any of it is written, and a refused tag leaves the output as it was.
/// After an [`Error::Io`] the output may hold part of a tag, so every later tag is refused with
/// [`Error::Broken`]: a caller that retries after a timeout must start a new stream with a new
/// `Writer`, the sequence headers and the metadata.
///
/// Every body but Annex B goes out in one vectored write with its tag header and PreviousTagSize,
/// so an unbuffered socket or pipe takes one system call per tag.
pub struct Writer<W: Write> {
    out: W,
    last: [i32; 3],
    tags: u64,
    bytes: u64,
    broken: bool,
}

impl<W: Write> Writer<W> {
    /// Writes the file header and the first PreviousTagSize to `out`.
    pub fn new(mut out: W, header: Header) -> Result<Self, Error> {
        let h = header.bytes();
        out.write_all(&h)?;
        Ok(Writer { out, last: [0; 3], tags: 0, bytes: h.len() as u64, broken: false })
    }

    /// Writes one tag at `timestamp` and returns the bytes written, PreviousTagSize included.
    pub fn write(&mut self, timestamp: i32, body: &Body<'_>) -> Result<usize, Error> {
        if self.broken {
            return Err(Error::Broken);
        }
        self.write_checked(timestamp, &body.check()?)
    }

    /// Writes one tag whose body [`Body::check`] accepted, without checking the body again, and
    /// returns the bytes written. Several writers can share one checked body.
    pub fn write_checked(&mut self, timestamp: i32, body: &Checked<'_>) -> Result<usize, Error> {
        if self.broken {
            return Err(Error::Broken);
        }
        let kind = body.tag_type();
        let min = self.last[kind.index()];
        if timestamp < min {
            return Err(Error::Timestamp { timestamp, min });
        }
        let len = body.len();
        let head = tag_header(kind, len, timestamp)?;
        let tail = ((TAG_HEADER_LEN + len) as u32).to_be_bytes();
        if let Err(e) = body.body().emit(&mut self.out, &head, &tail) {
            self.broken = true;
            return Err(Error::Io(e));
        }
        self.last[kind.index()] = timestamp;
        let n = TAG_HEADER_LEN + len + 4;
        self.tags += 1;
        self.bytes += n as u64;
        Ok(n)
    }

    /// Lets the next tag of each type start again from timestamp 0, for a stream that reaches the
    /// largest timestamp. Players see the time go back, so send the sequence headers and the
    /// metadata again first.
    pub fn reset_timestamps(&mut self) {
        self.last = [0; 3];
    }

    /// Whether a write failed partway through a tag, so the writer refuses every later tag.
    pub fn is_broken(&self) -> bool {
        self.broken
    }

    /// Encodes `metadata` and writes it as an `onMetaData` script tag at `timestamp`.
    pub fn write_metadata(&mut self, timestamp: i32, metadata: &Metadata) -> Result<usize, Error> {
        let mut data = Vec::with_capacity(256);
        metadata.encode(&mut data)?;
        self.write(timestamp, &Body::Script(&data))
    }

    /// Flushes the output. A failed flush may leave part of a tag at the destination, so it marks
    /// the writer broken, as a failed write does.
    pub fn flush(&mut self) -> Result<(), Error> {
        if self.broken {
            return Err(Error::Broken);
        }
        self.out.flush().map_err(|e| {
            self.broken = true;
            Error::Io(e)
        })
    }

    /// Tags written.
    pub fn tags(&self) -> u64 {
        self.tags
    }

    /// Bytes written, the header included.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// The output.
    pub fn get_ref(&self) -> &W {
        &self.out
    }

    /// The output, for example to clear a buffer between tags.
    pub fn get_mut(&mut self) -> &mut W {
        &mut self.out
    }

    /// Returns the output.
    pub fn into_inner(self) -> W {
        self.out
    }
}

/// The `onMetaData` properties (FLV specification E.5). Properties left at `None` are not written.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Metadata {
    /// Total duration in seconds; 0 for a live stream.
    pub duration: Option<f64>,
    /// Picture width in pixels.
    pub width: Option<f64>,
    /// Picture height in pixels.
    pub height: Option<f64>,
    /// Video bit rate in kilobits per second.
    pub videodatarate: Option<f64>,
    /// Frames per second.
    pub framerate: Option<f64>,
    /// Video CodecID; 7 for H.264.
    pub videocodecid: Option<f64>,
    /// Audio bit rate in kilobits per second.
    pub audiodatarate: Option<f64>,
    /// Audio sample rate in hertz.
    pub audiosamplerate: Option<f64>,
    /// Bits per audio sample.
    pub audiosamplesize: Option<f64>,
    /// Stereo audio.
    pub stereo: Option<bool>,
    /// Audio SoundFormat; 10 for AAC.
    pub audiocodecid: Option<f64>,
    /// Total size of the file in bytes.
    pub filesize: Option<f64>,
}

impl Metadata {
    /// Metadata for a live H.264 and AAC stream: duration 0, the picture size and frame rate,
    /// both bit rates in kilobits per second, the audio sample rate, 16-bit samples, and the
    /// channel layout.
    pub fn h264_aac(width: u32, height: u32, framerate: f64, video_kbps: u32, audio_kbps: u32, sample_rate: u32, stereo: bool) -> Metadata {
        Metadata {
            duration: Some(0.0),
            width: Some(f64::from(width)),
            height: Some(f64::from(height)),
            videodatarate: Some(f64::from(video_kbps)),
            framerate: Some(framerate),
            videocodecid: Some(f64::from(CODEC_AVC)),
            audiodatarate: Some(f64::from(audio_kbps)),
            audiosamplerate: Some(f64::from(sample_rate)),
            audiosamplesize: Some(16.0),
            stereo: Some(stereo),
            audiocodecid: Some(f64::from(SOUND_FORMAT_AAC)),
            filesize: None,
        }
    }

    fn numbers(&self) -> [(&'static str, Option<f64>); 11] {
        [
            ("duration", self.duration),
            ("width", self.width),
            ("height", self.height),
            ("videodatarate", self.videodatarate),
            ("framerate", self.framerate),
            ("videocodecid", self.videocodecid),
            ("audiodatarate", self.audiodatarate),
            ("audiosamplerate", self.audiosamplerate),
            ("audiosamplesize", self.audiosamplesize),
            ("audiocodecid", self.audiocodecid),
            ("filesize", self.filesize),
        ]
    }

    /// Appends the script data body: the string `onMetaData` and an ECMA array of the properties
    /// that are set.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        let numbers = self.numbers();
        let count = numbers.iter().filter(|(_, v)| v.is_some()).count() + usize::from(self.stereo.is_some());
        amf0::write_string(out, "onMetaData")?;
        amf0::write_ecma_array_start(out, count as u32);
        // The stereo flag goes before audiocodecid, the order of the specification's table.
        for (name, v) in numbers {
            if name == "audiocodecid"
                && let Some(s) = self.stereo
            {
                amf0::write_property_name(out, "stereo")?;
                amf0::write_bool(out, s);
            }
            if let Some(v) = v {
                amf0::write_property_name(out, name)?;
                amf0::write_number(out, v);
            }
        }
        amf0::write_object_end(out);
        Ok(())
    }

    /// The properties this type holds, read from an `onMetaData` value (an ECMA array or an
    /// object). Other properties and properties of another type are left out.
    pub fn from_value(v: &Value) -> Metadata {
        let n = |k: &str| v.get(k).and_then(Value::as_number);
        Metadata {
            duration: n("duration"),
            width: n("width"),
            height: n("height"),
            videodatarate: n("videodatarate"),
            framerate: n("framerate"),
            videocodecid: n("videocodecid"),
            audiodatarate: n("audiodatarate"),
            audiosamplerate: n("audiosamplerate"),
            audiosamplesize: n("audiosamplesize"),
            stereo: v.get("stereo").and_then(Value::as_bool),
            audiocodecid: n("audiocodecid"),
            filesize: n("filesize"),
        }
    }
}

#[cfg(test)]
mod tests;
