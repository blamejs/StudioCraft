//! RTP data packets (RFC 3550 section 5.1).

use crate::{Error, be16, be32};

/// Bytes in the fixed RTP header.
pub const HEADER_LEN: usize = 12;
/// The most contributing sources one header lists: the CSRC count is a 4-bit field.
pub const MAX_CSRC: usize = 15;

/// A header extension (RFC 3550 section 5.3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extension<'a> {
    /// The 16 bits the profile defines, such as `0xBEDE` for one-byte header extensions
    /// (RFC 8285).
    pub profile: u16,
    /// The extension's data, a whole number of 32-bit words and at most 65 535 of them.
    pub data: &'a [u8],
}

/// One RTP data packet, borrowed from the bytes it was read from or that it will be written from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Packet<'a> {
    /// The marker bit. Video profiles set it on the last packet of a frame.
    pub marker: bool,
    /// The payload type, 0 to 127.
    pub payload_type: u8,
    /// The sequence number, which counts up by one per packet and wraps at 65 536.
    pub sequence: u16,
    /// The media time of the payload, in the payload's clock rate.
    pub timestamp: u32,
    /// The synchronization source identifier.
    pub ssrc: u32,
    /// The contributing source identifiers, 4 bytes each, big-endian, at most [`MAX_CSRC`].
    pub csrc: &'a [u8],
    /// The header extension.
    pub extension: Option<Extension<'a>>,
    /// The payload, without the padding.
    pub payload: &'a [u8],
    /// Bytes of padding after the payload, counting the last byte, which holds the count. 0 for
    /// none.
    pub padding: u8,
}

impl<'a> Packet<'a> {
    /// A packet with no contributing sources, no extension and no padding.
    pub fn new(payload_type: u8, sequence: u16, timestamp: u32, ssrc: u32, marker: bool, payload: &'a [u8]) -> Packet<'a> {
        Packet { marker, payload_type, sequence, timestamp, ssrc, csrc: &[], extension: None, payload, padding: 0 }
    }

    /// Reads a packet. A version other than 2, a header, contributing source list or extension
    /// that runs past the end, a padding count of 0, and padding longer than the bytes after the
    /// header are errors.
    pub fn parse(b: &'a [u8]) -> Result<Packet<'a>, Error> {
        let (Some(&b0), Some(&b1)) = (b.first(), b.get(1)) else {
            return Err(Error::Truncated("the RTP header"));
        };
        if b.len() < HEADER_LEN {
            return Err(Error::Truncated("the RTP header"));
        }
        if b0 >> 6 != 2 {
            return Err(Error::Invalid("the RTP version is not 2"));
        }
        let sequence = be16(b, 2).ok_or(Error::Truncated("the RTP header"))?;
        let timestamp = be32(b, 4).ok_or(Error::Truncated("the RTP header"))?;
        let ssrc = be32(b, 8).ok_or(Error::Truncated("the RTP header"))?;
        let csrc_end = HEADER_LEN + usize::from(b0 & 0x0F) * 4;
        let csrc = b.get(HEADER_LEN..csrc_end).ok_or(Error::Truncated("the contributing source list"))?;
        let mut at = csrc_end;
        let extension = if b0 & 0x10 != 0 {
            let profile = be16(b, at).ok_or(Error::Truncated("the header extension"))?;
            let words = be16(b, at + 2).ok_or(Error::Truncated("the header extension"))?;
            let start = at + 4;
            let end = start + usize::from(words) * 4;
            let data = b.get(start..end).ok_or(Error::Truncated("the header extension"))?;
            at = end;
            Some(Extension { profile, data })
        } else {
            None
        };
        let mut end = b.len();
        let padding = if b0 & 0x20 != 0 {
            let n = b.last().copied().unwrap_or(0);
            if n == 0 {
                return Err(Error::Invalid("the padding count is 0"));
            }
            if usize::from(n) > end - at {
                return Err(Error::Invalid("the padding is longer than the bytes after the header"));
            }
            end -= usize::from(n);
            n
        } else {
            0
        };
        let payload = b.get(at..end).ok_or(Error::Truncated("the payload"))?;
        Ok(Packet { marker: b1 & 0x80 != 0, payload_type: b1 & 0x7F, sequence, timestamp, ssrc, csrc, extension, payload, padding })
    }

    /// The contributing source identifiers.
    pub fn csrcs(&self) -> impl Iterator<Item = u32> + 'a {
        self.csrc.as_chunks::<4>().0.iter().map(|c| u32::from_be_bytes(*c))
    }

    /// Bytes [`write`](Packet::write) appends.
    pub fn len(&self) -> usize {
        HEADER_LEN + self.csrc.len() + self.extension.map_or(0, |e| 4 + e.data.len()) + self.payload.len() + usize::from(self.padding)
    }

    /// `true` when the packet has no payload and no padding.
    pub fn is_empty(&self) -> bool {
        self.payload.is_empty() && self.padding == 0
    }

    /// Appends the packet to `out`. A payload type over 127, a contributing source list that is
    /// not a whole number of 4-byte identifiers or holds more than [`MAX_CSRC`], and extension
    /// data that is not a whole number of 32-bit words or is longer than 65 535 words are refused,
    /// and `out` is left as it was. Padding bytes are written as zeros, the last one holding the
    /// count.
    pub fn write(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        if self.payload_type > 0x7F {
            return Err(Error::BadInput("an RTP payload type is 0 to 127"));
        }
        if !self.csrc.len().is_multiple_of(4) || self.csrc.len() / 4 > MAX_CSRC {
            return Err(Error::BadInput("the contributing source list holds 0 to 15 identifiers of 4 bytes"));
        }
        let words = match self.extension {
            Some(e) if e.data.len() % 4 != 0 || e.data.len() / 4 > usize::from(u16::MAX) => {
                return Err(Error::BadInput("header extension data is 0 to 65 535 words of 4 bytes"));
            }
            Some(e) => Some((e.profile, (e.data.len() / 4) as u16, e.data)),
            None => None,
        };
        out.reserve(self.len());
        let b0 = 0x80 | (u8::from(self.padding > 0) << 5) | (u8::from(words.is_some()) << 4) | (self.csrc.len() / 4) as u8;
        out.push(b0);
        out.push((u8::from(self.marker) << 7) | self.payload_type);
        out.extend_from_slice(&self.sequence.to_be_bytes());
        out.extend_from_slice(&self.timestamp.to_be_bytes());
        out.extend_from_slice(&self.ssrc.to_be_bytes());
        out.extend_from_slice(self.csrc);
        if let Some((profile, n, data)) = words {
            out.extend_from_slice(&profile.to_be_bytes());
            out.extend_from_slice(&n.to_be_bytes());
            out.extend_from_slice(data);
        }
        out.extend_from_slice(self.payload);
        if self.padding > 0 {
            out.resize(out.len() + usize::from(self.padding) - 1, 0);
            out.push(self.padding);
        }
        Ok(())
    }
}

/// `true` when a packet on a port that carries both RTP and RTCP is RTCP: its second byte is 192
/// to 223, the RTCP packet types that RFC 5761 section 4 keeps apart from RTP payload types.
pub fn is_rtcp(b: &[u8]) -> bool {
    matches!(b.get(1), Some(192..=223))
}
