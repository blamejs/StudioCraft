//! Rebuilding coded frames from RTP payloads: H.264 (RFC 6184), VP8 (RFC 7741), Opus (RFC 7587)
//! and AAC (RFC 3640).

use crate::{Error, be16};

/// The largest frame a [`Depacketizer`] rebuilds by default: the jitter buffer's byte limit.
pub const MAX_FRAME: usize = 8 * 1024 * 1024;

/// The 4-byte start code put before each H.264 NAL unit.
const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// The AU-header fields of an `mpeg4-generic` AAC stream (RFC 3640 section 3.2.1), from its SDP
/// `fmtp` line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AacConfig {
    /// `sizeLength`: bits of the AU-size field, 1 to 16.
    pub size_length: u8,
    /// `indexLength`: bits of the AU-Index field of the first AU-header, 0 to 16.
    pub index_length: u8,
    /// `indexDeltaLength`: bits of the AU-Index-delta field of the others, 0 to 16.
    pub index_delta_length: u8,
    /// Samples per access unit, for the timestamps of several units in one packet: 1024 for AAC
    /// LC.
    pub frame_samples: u32,
}

impl AacConfig {
    /// The AAC-hbr mode: 13, 3 and 3 bits, 1024 samples per unit.
    pub const HBR: AacConfig = AacConfig { size_length: 13, index_length: 3, index_delta_length: 3, frame_samples: 1024 };
    /// The AAC-lbr mode: 6, 2 and 2 bits, 1024 samples per unit.
    pub const LBR: AacConfig = AacConfig { size_length: 6, index_length: 2, index_delta_length: 2, frame_samples: 1024 };
}

/// The payload format of a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// H.264 in packetization mode 0 or 1: single NAL units, STAP-A and FU-A.
    H264,
    /// VP8.
    Vp8,
    /// Opus: one Opus packet per RTP payload.
    Opus,
    /// AAC as `mpeg4-generic` with these AU-header fields.
    Aac(AacConfig),
}

impl Codec {
    fn is_video(self) -> bool {
        matches!(self, Codec::H264 | Codec::Vp8)
    }
}

/// One rebuilt frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame<'a> {
    /// Its RTP timestamp. For the second and later AAC units of one packet, the packet's timestamp
    /// plus [`AacConfig::frame_samples`] for each unit before it.
    pub timestamp: u32,
    /// A frame a decoder can start from: an H.264 access unit with an IDR picture, a VP8 key
    /// frame, and every audio frame.
    pub key: bool,
    /// The frame: an H.264 access unit as an Annex B byte stream with 4-byte start codes, a VP8
    /// frame, an Opus packet or a raw AAC access unit.
    pub data: &'a [u8],
}

/// The counts of a [`Depacketizer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DepacketizerStats {
    /// Frames given out.
    pub frames: u64,
    /// Frames dropped because part of them was lost or did not read, or because video was waiting
    /// for a key frame.
    pub dropped: u64,
    /// Payloads that did not read.
    pub errors: u64,
}

/// Rebuilds the frames of one stream from the payloads of its packets in sequence order, as a
/// [`JitterBuffer`](crate::JitterBuffer) releases them.
///
/// A video frame ends at a packet with the marker bit, or when a packet with another timestamp
/// arrives. A video frame with a lost part is dropped, and video then waits for the next key
/// frame; [`wants_key_frame`](Depacketizer::wants_key_frame) says so, so the caller can send a
/// picture loss indication. Video starts by waiting for a key frame.
#[derive(Debug)]
pub struct Depacketizer {
    codec: Codec,
    max_frame: usize,
    buf: Vec<u8>,
    timestamp: Option<u32>,
    key: bool,
    broken: bool,
    need_key: bool,
    /// H.264: a FU-A fragment run is open. VP8: the frame's first partition began.
    open: bool,
    /// H.264: the NAL unit header of the open FU-A fragment run.
    fu_header: u8,
    /// H.264: whether the frame's first slice has a `first_mb_in_slice` of 0, once a slice came.
    first_slice: Option<bool>,
    /// H.264: the open FU-A fragment run is the frame's first slice and none of its body bytes
    /// has come yet.
    slice_pending: bool,
    /// AAC: the size of the access unit whose fragments are being joined.
    aac_size: Option<usize>,
    stats: DepacketizerStats,
}

impl Depacketizer {
    /// A depacketizer for `codec` with [`MAX_FRAME`] as its frame limit. An AAC configuration
    /// with an AU-size field of 0 or over 16 bits, an index field over 16 bits, or 0 samples per
    /// unit is refused.
    pub fn new(codec: Codec) -> Result<Depacketizer, Error> {
        Depacketizer::with_max_frame(codec, MAX_FRAME)
    }

    /// A depacketizer whose frames are at most `max_frame` bytes; a larger frame is dropped.
    pub fn with_max_frame(codec: Codec, max_frame: usize) -> Result<Depacketizer, Error> {
        if let Codec::Aac(c) = codec {
            if c.size_length == 0 || c.size_length > 16 || c.index_length > 16 || c.index_delta_length > 16 {
                return Err(Error::BadInput("AAC AU-header fields are 1 to 16 bits for the size and 0 to 16 for the index"));
            }
            if c.frame_samples == 0 {
                return Err(Error::BadInput("an AAC access unit holds more than 0 samples"));
            }
        }
        Ok(Depacketizer {
            codec,
            max_frame,
            buf: Vec::new(),
            timestamp: None,
            key: false,
            broken: false,
            need_key: codec.is_video(),
            open: false,
            fu_header: 0,
            first_slice: None,
            slice_pending: false,
            aac_size: None,
            stats: DepacketizerStats::default(),
        })
    }

    /// The stream's payload format.
    pub fn codec(&self) -> Codec {
        self.codec
    }

    /// Takes the next payload in sequence order and gives each frame it completes to `emit`. A
    /// payload that does not read is an error; the frame it belongs to is dropped, and the
    /// depacketizer carries on with the next payload.
    pub fn push(&mut self, timestamp: u32, marker: bool, payload: &[u8], mut emit: impl FnMut(Frame<'_>)) -> Result<(), Error> {
        let result = match self.codec {
            Codec::Opus => {
                if payload.is_empty() {
                    self.stats.dropped += 1;
                    Err(Error::Invalid("an Opus packet is empty"))
                } else if payload.len() > self.max_frame {
                    self.stats.dropped += 1;
                    Err(Error::TooLarge { size: payload.len(), limit: self.max_frame })
                } else {
                    self.stats.frames += 1;
                    emit(Frame { timestamp, key: true, data: payload });
                    Ok(())
                }
            }
            Codec::Aac(config) => self.push_aac(config, timestamp, marker, payload, &mut emit),
            Codec::H264 | Codec::Vp8 => {
                if self.timestamp.is_some_and(|t| t != timestamp) {
                    self.finish(&mut emit);
                }
                self.timestamp = Some(timestamp);
                let r = if self.codec == Codec::H264 { self.push_h264(payload) } else { self.push_vp8(payload) };
                if r.is_err() {
                    self.broken = true;
                }
                if self.broken {
                    // Asked for now, so a caller that checks after each packet can ask for a key
                    // frame before the frame ends.
                    self.need_key = true;
                }
                if marker {
                    self.finish(&mut emit);
                }
                r
            }
        };
        if result.is_err() {
            self.stats.errors += 1;
        }
        result
    }

    /// Takes a gap in the stream: the frame being built is dropped, and video waits for the next
    /// key frame. The frame's timestamp is kept, so its remaining packets are dropped with it and
    /// it is counted once, when it ends.
    pub fn lost(&mut self) {
        if self.codec.is_video() {
            self.need_key = true;
        } else if self.aac_size.is_some() {
            self.stats.dropped += 1;
        }
        let timestamp = self.timestamp;
        self.reset();
        if timestamp.is_some() {
            self.timestamp = timestamp;
            self.broken = true;
        }
    }

    /// `true` while video waits for a key frame.
    pub fn wants_key_frame(&self) -> bool {
        self.need_key
    }

    /// The depacketizer's counts.
    pub fn stats(&self) -> DepacketizerStats {
        self.stats
    }

    fn reset(&mut self) {
        self.buf.clear();
        self.timestamp = None;
        self.key = false;
        self.broken = false;
        self.open = false;
        self.first_slice = None;
        self.slice_pending = false;
        self.aac_size = None;
    }

    /// Ends the video frame being built and gives it out unless it is broken or video waits for a
    /// key frame and this is not one. While video waits, an H.264 key frame whose first slice does
    /// not start at macroblock 0 lost its start and is dropped too.
    fn finish(&mut self, emit: &mut impl FnMut(Frame<'_>)) {
        let Some(timestamp) = self.timestamp else { return };
        // A fragmented H.264 NAL unit that never ended, or a VP8 frame whose start never came.
        if self.codec == Codec::H264 && self.open {
            self.broken = true;
        }
        if self.codec == Codec::Vp8 && !self.open && !self.buf.is_empty() {
            self.broken = true;
        }
        if self.broken {
            self.need_key = true;
        }
        let starts = self.codec != Codec::H264 || self.first_slice == Some(true);
        if self.broken || self.buf.is_empty() || (self.need_key && !(self.key && starts)) {
            self.stats.dropped += 1;
        } else {
            self.need_key = false;
            self.stats.frames += 1;
            emit(Frame { timestamp, key: self.key, data: &self.buf });
        }
        self.reset();
    }

    fn append(&mut self, parts: &[&[u8]]) -> Result<(), Error> {
        let size = self.buf.len() + parts.iter().map(|p| p.len()).sum::<usize>();
        if size > self.max_frame {
            return Err(Error::TooLarge { size, limit: self.max_frame });
        }
        parts.iter().for_each(|p| self.buf.extend_from_slice(p));
        Ok(())
    }

    fn append_nal(&mut self, nal: &[u8]) -> Result<(), Error> {
        let Some(&header) = nal.first() else {
            return Err(Error::Invalid("an H.264 NAL unit is empty"));
        };
        if header & 0x80 != 0 {
            return Err(Error::Invalid("an H.264 NAL unit has the forbidden bit set"));
        }
        self.saw_nal(header, nal.get(1).copied(), false);
        self.append(&[&START_CODE, nal])
    }

    /// Notes a NAL unit with `header` whose next byte is `next`: an IDR slice makes the frame a
    /// key frame, and the first slice says whether the frame starts at its first macroblock. With
    /// `wait`, a first slice without a next byte leaves that open until a later fragment brings one.
    fn saw_nal(&mut self, header: u8, next: Option<u8>, wait: bool) {
        let nal_type = header & 0x1F;
        if nal_type == 5 {
            self.key = true;
        }
        self.slice_pending = false;
        // Coded slices and slice data partition A start with the slice header, whose first field,
        // first_mb_in_slice, is an Exp-Golomb code: 0 is the single bit 1 (H.264 sections 7.3.3
        // and 9.1).
        if matches!(nal_type, 1 | 2 | 5) && self.first_slice.is_none() {
            match next {
                None if wait => self.slice_pending = true,
                _ => self.first_slice = Some(next.is_some_and(|b| b & 0x80 != 0)),
            }
        }
    }

    fn push_h264(&mut self, p: &[u8]) -> Result<(), Error> {
        let Some(&indicator) = p.first() else {
            return Err(Error::Truncated("the H.264 payload header"));
        };
        if indicator & 0x80 != 0 {
            return Err(Error::Invalid("an H.264 payload has the forbidden bit set"));
        }
        match indicator & 0x1F {
            1..=23 => {
                if self.open {
                    self.broken = true;
                    self.open = false;
                }
                self.append_nal(p)
            }
            24 => {
                if self.open {
                    self.broken = true;
                    self.open = false;
                }
                let mut at = 1;
                if at >= p.len() {
                    return Err(Error::Truncated("an STAP-A packet holds no NAL unit"));
                }
                while at < p.len() {
                    let size = usize::from(be16(p, at).ok_or(Error::Truncated("an STAP-A NAL unit size"))?);
                    if size == 0 {
                        return Err(Error::Invalid("an STAP-A NAL unit size is 0"));
                    }
                    let nal = p.get(at + 2..at + 2 + size).ok_or(Error::Truncated("an STAP-A NAL unit"))?;
                    // RFC 6184 section 5.7: an aggregation packet holds no aggregation packet or
                    // fragmentation unit.
                    if !matches!(nal.first().map(|h| h & 0x1F), Some(1..=23)) {
                        return Err(Error::Invalid("an STAP-A holds a NAL unit of a packetization type"));
                    }
                    self.append_nal(nal)?;
                    at += 2 + size;
                }
                Ok(())
            }
            28 => {
                let Some(&fu) = p.get(1) else {
                    return Err(Error::Truncated("the FU-A header"));
                };
                let (start, end) = (fu & 0x80 != 0, fu & 0x40 != 0);
                if start && end {
                    return Err(Error::Invalid("a FU-A fragment has both the start and end bits set"));
                }
                let data = p.get(2..).unwrap_or(&[]);
                // The header of the NAL unit the fragment belongs to.
                let header = (indicator & 0xE0) | (fu & 0x1F);
                if start {
                    if self.open {
                        self.broken = true;
                    }
                    // RFC 6184 section 5.8: an aggregation packet or FU is not fragmented.
                    if !(1..=23).contains(&(header & 0x1F)) {
                        self.open = false;
                        return Err(Error::Invalid("a FU-A fragment carries a NAL unit of a packetization type"));
                    }
                    // RFC 6184 section 5.8 allows a FU payload of no bytes, so the slice header
                    // may come in a later fragment.
                    self.saw_nal(header, data.first().copied(), true);
                    self.append(&[&START_CODE, &[header], data])?;
                    self.fu_header = header;
                    self.open = true;
                } else if !self.open {
                    // The start fragment was lost: the NAL unit cannot be rebuilt.
                    self.broken = true;
                } else if header != self.fu_header {
                    self.open = false;
                    return Err(Error::Invalid("a FU-A fragment does not continue the NAL unit before it"));
                } else {
                    self.append(&[data])?;
                    if self.slice_pending {
                        if let Some(&b) = data.first() {
                            self.first_slice = Some(b & 0x80 != 0);
                            self.slice_pending = false;
                        } else if end {
                            // A slice with no slice header does not start the frame.
                            self.first_slice = Some(false);
                            self.slice_pending = false;
                        }
                    }
                    if end {
                        self.open = false;
                    }
                }
                Ok(())
            }
            25..=27 | 29 => Err(Error::Unsupported("the interleaved H.264 packetization mode")),
            _ => Err(Error::Invalid("an H.264 payload has a reserved NAL unit type")),
        }
    }

    fn push_vp8(&mut self, p: &[u8]) -> Result<(), Error> {
        let Some(&b0) = p.first() else {
            return Err(Error::Truncated("the VP8 payload descriptor"));
        };
        let mut at = 1;
        if b0 & 0x80 != 0 {
            let Some(&x) = p.get(1) else {
                return Err(Error::Truncated("the VP8 payload descriptor"));
            };
            at = 2;
            if x & 0x80 != 0 {
                let Some(&pid) = p.get(at) else {
                    return Err(Error::Truncated("the VP8 PictureID"));
                };
                at += if pid & 0x80 != 0 { 2 } else { 1 };
            }
            if x & 0x40 != 0 {
                at += 1;
            }
            if x & 0x30 != 0 {
                at += 1;
            }
        }
        let data = match p.get(at..) {
            Some(d) if !d.is_empty() => d,
            _ => return Err(Error::Truncated("a VP8 payload holds no frame data")),
        };
        let start = b0 & 0x10 != 0 && b0 & 0x07 == 0;
        if start {
            if self.open || !self.buf.is_empty() {
                self.broken = true;
            }
            // RFC 7741 section 4.3: bit 0 of the first byte of the frame is 0 for a key frame.
            self.key = data.first().is_some_and(|b| b & 0x01 == 0);
            self.open = true;
        }
        self.append(&[data])
    }

    fn push_aac(&mut self, c: AacConfig, timestamp: u32, marker: bool, p: &[u8], emit: &mut impl FnMut(Frame<'_>)) -> Result<(), Error> {
        if self.timestamp.is_some_and(|t| t != timestamp) {
            // A fragmented unit whose last fragment never came, or one a gap cut off.
            if self.aac_size.is_some() {
                self.stats.dropped += 1;
            }
            self.reset();
        }
        if self.broken {
            // The rest of a fragmented unit that a gap cut off; `lost` counted the unit.
            if marker {
                self.reset();
            }
            return Err(Error::Invalid("an AAC fragment belongs to an access unit that was lost"));
        }
        let r = self.read_aac(c, timestamp, marker, p, emit);
        if r.is_err() {
            // The unit being joined, or the first unit the packet holds, is dropped once.
            self.stats.dropped += 1;
            self.reset();
        }
        r
    }

    fn read_aac(&mut self, c: AacConfig, timestamp: u32, marker: bool, p: &[u8], emit: &mut impl FnMut(Frame<'_>)) -> Result<(), Error> {
        let bits = usize::from(be16(p, 0).ok_or(Error::Truncated("the AU-headers-length"))?);
        let header_bytes = bits.div_ceil(8);
        let headers = p.get(2..2 + header_bytes).ok_or(Error::Truncated("the AU-headers"))?;
        let data = p.get(2 + header_bytes..).unwrap_or(&[]);
        let first = usize::from(c.size_length) + usize::from(c.index_length);
        let other = usize::from(c.size_length) + usize::from(c.index_delta_length);
        if bits < first || (bits - first) % other != 0 {
            return Err(Error::Invalid("the AU-headers-length is not a whole number of AU-headers"));
        }
        let units = 1 + (bits - first) / other;
        let mut reader = Bits { data: headers, at: 0 };
        // The fragments of one unit: each carries the size of the whole unit (RFC 3640
        // section 3.2.3).
        if let Some(size) = self.aac_size {
            let au = reader.read(c.size_length).ok_or(Error::Truncated("an AU-header"))? as usize;
            if units != 1 || au != size {
                return Err(Error::Invalid("an AAC fragment does not continue the access unit before it"));
            }
            if self.buf.len() + data.len() > size {
                return Err(Error::Invalid("AAC fragments are longer than their access unit"));
            }
            self.append(&[data])?;
            if self.buf.len() == size {
                // RFC 3640 section 3.1: the final fragment of an access unit has the M bit set.
                if !marker {
                    return Err(Error::Invalid("the last AAC fragment does not have the marker bit"));
                }
                self.stats.frames += 1;
                emit(Frame { timestamp, key: true, data: &self.buf });
                self.reset();
            } else if marker {
                return Err(Error::Invalid("the last AAC fragment ends before its access unit"));
            }
            return Ok(());
        }
        let mut at = 0usize;
        for i in 0..units {
            let size = reader.read(c.size_length).ok_or(Error::Truncated("an AU-header"))? as usize;
            let index_bits = if i == 0 { c.index_length } else { c.index_delta_length };
            let index = reader.read(index_bits).ok_or(Error::Truncated("an AU-header"))?;
            if i > 0 && index != 0 {
                return Err(Error::Unsupported("interleaved AAC access units"));
            }
            if size == 0 {
                return Err(Error::Invalid("an AAC access unit size is 0"));
            }
            if size > self.max_frame {
                return Err(Error::TooLarge { size, limit: self.max_frame });
            }
            let rest = data.len() - at;
            if units == 1 && size > rest {
                if marker {
                    return Err(Error::Invalid("an AAC access unit is longer than its packet"));
                }
                self.buf.clear();
                self.append(&[data])?;
                self.aac_size = Some(size);
                self.timestamp = Some(timestamp);
                return Ok(());
            }
            if size > rest {
                return Err(Error::Truncated("an AAC access unit"));
            }
            at += size;
        }
        if at != data.len() {
            return Err(Error::Invalid("the AAC access units do not fill the packet"));
        }
        let mut reader = Bits { data: headers, at: 0 };
        let mut at = 0usize;
        for i in 0..units {
            let size = reader.read(c.size_length).unwrap_or(0) as usize;
            reader.read(if i == 0 { c.index_length } else { c.index_delta_length });
            let unit = data.get(at..at + size).unwrap_or(&[]);
            at += size;
            self.stats.frames += 1;
            let offset = c.frame_samples.wrapping_mul(i as u32);
            emit(Frame { timestamp: timestamp.wrapping_add(offset), key: true, data: unit });
        }
        Ok(())
    }
}

/// Reads big-endian bit fields.
struct Bits<'a> {
    data: &'a [u8],
    at: usize,
}

impl Bits<'_> {
    /// The next `n` bits (at most 32), or `None` past the end.
    fn read(&mut self, n: u8) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = *self.data.get(self.at / 8)?;
            let bit = (byte >> (7 - self.at % 8)) & 1;
            v = (v << 1) | u32::from(bit);
            self.at += 1;
        }
        Some(v)
    }
}
