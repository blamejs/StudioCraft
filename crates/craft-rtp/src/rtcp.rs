//! RTCP (RFC 3550 section 6) and the transport and payload-specific feedback of RFC 4585.
//!
//! [`Compound`] reads the packets of one compound RTCP datagram one at a time. The `write_*`
//! functions append one packet each, so a compound packet is several calls on the same buffer.
//! [`TimeMap`] turns RTP timestamps into the sender's wall-clock time from its sender reports.

use std::time::Duration;

use crate::{Error, be16, be32, be64};

/// Packet type of a sender report.
pub const SR: u8 = 200;
/// Packet type of a receiver report.
pub const RR: u8 = 201;
/// Packet type of source descriptions.
pub const SDES: u8 = 202;
/// Packet type of BYE.
pub const BYE: u8 = 203;
/// Packet type of application-defined packets.
pub const APP: u8 = 204;
/// Packet type of transport-layer feedback (RFC 4585), which holds the generic NACK.
pub const RTPFB: u8 = 205;
/// Packet type of payload-specific feedback (RFC 4585), which holds the picture loss indication.
pub const PSFB: u8 = 206;
/// The most report blocks or BYE sources one packet holds: the count is a 5-bit field.
pub const MAX_COUNT: usize = 31;
/// Bytes in one report block.
pub const REPORT_BLOCK_LEN: usize = 24;
/// How far a sender report may move the RTP to wall-clock mapping before [`TimeMap`] starts it
/// again.
pub const MAX_JUMP: Duration = Duration::from_secs(1);

/// One report block of a sender or receiver report (RFC 3550 section 6.4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReportBlock {
    /// The source this block reports on.
    pub ssrc: u32,
    /// Packets lost since the last report, as a fraction of 256.
    pub fraction_lost: u8,
    /// Packets lost since the start, a signed 24-bit value (negative when duplicates arrived).
    pub cumulative_lost: i32,
    /// The highest sequence number received, with the count of wraps in the upper 16 bits.
    pub highest_sequence: u32,
    /// The inter-arrival jitter, in timestamp units.
    pub jitter: u32,
    /// The middle 32 bits of the NTP time of the last sender report received, or 0.
    pub last_sr: u32,
    /// The time since that sender report was received, in units of 1/65 536 s, or 0.
    pub delay_since_last_sr: u32,
}

/// Lowest cumulative loss a report block holds.
pub const MIN_CUMULATIVE_LOST: i32 = -0x80_0000;
/// Highest cumulative loss a report block holds.
pub const MAX_CUMULATIVE_LOST: i32 = 0x7F_FFFF;

impl ReportBlock {
    fn parse(b: &[u8]) -> Option<ReportBlock> {
        let lost = be32(b, 4)?;
        // A signed 24-bit field: move it to the top of an i32 and back to sign-extend it.
        let cumulative_lost = ((lost << 8) as i32) >> 8;
        Some(ReportBlock {
            ssrc: be32(b, 0)?,
            fraction_lost: (lost >> 24) as u8,
            cumulative_lost,
            highest_sequence: be32(b, 8)?,
            jitter: be32(b, 12)?,
            last_sr: be32(b, 16)?,
            delay_since_last_sr: be32(b, 20)?,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        let lost = self.cumulative_lost.clamp(MIN_CUMULATIVE_LOST, MAX_CUMULATIVE_LOST) as u32 & 0xFF_FFFF;
        out.extend_from_slice(&self.ssrc.to_be_bytes());
        out.extend_from_slice(&((u32::from(self.fraction_lost) << 24) | lost).to_be_bytes());
        out.extend_from_slice(&self.highest_sequence.to_be_bytes());
        out.extend_from_slice(&self.jitter.to_be_bytes());
        out.extend_from_slice(&self.last_sr.to_be_bytes());
        out.extend_from_slice(&self.delay_since_last_sr.to_be_bytes());
    }
}

/// The report blocks of a sender or receiver report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reports<'a>(&'a [u8]);

impl Reports<'_> {
    /// Blocks it holds.
    pub fn len(&self) -> usize {
        self.0.len() / REPORT_BLOCK_LEN
    }

    /// `true` when it holds none.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Iterator for Reports<'_> {
    type Item = ReportBlock;

    fn next(&mut self) -> Option<ReportBlock> {
        let block = ReportBlock::parse(self.0)?;
        self.0 = self.0.get(REPORT_BLOCK_LEN..).unwrap_or(&[]);
        Some(block)
    }
}

/// The sender information of a sender report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SenderInfo {
    /// The sender's synchronization source identifier.
    pub ssrc: u32,
    /// The sender's wall-clock time when it sent the report, as a 64-bit NTP timestamp.
    pub ntp: u64,
    /// The same instant in the stream's RTP timestamp units.
    pub rtp_timestamp: u32,
    /// RTP data packets sent since the start.
    pub packets: u32,
    /// Payload bytes sent since the start.
    pub octets: u32,
}

/// The sequence numbers a generic NACK asks for: each FCI entry's packet id, and one more for
/// each bit set in its bitmask of following lost packets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lost<'a> {
    fci: &'a [u8],
    bit: u8,
}

impl Iterator for Lost<'_> {
    type Item = u16;

    fn next(&mut self) -> Option<u16> {
        loop {
            let pid = be16(self.fci, 0)?;
            let blp = be16(self.fci, 2)?;
            let bit = self.bit;
            if bit == 0 {
                self.bit = 1;
                return Some(pid);
            }
            if bit > 16 {
                self.fci = self.fci.get(4..).unwrap_or(&[]);
                self.bit = 0;
                continue;
            }
            self.bit += 1;
            if blp & (1 << (bit - 1)) != 0 {
                return Some(pid.wrapping_add(u16::from(bit)));
            }
        }
    }
}

/// One RTCP packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rtcp<'a> {
    /// A sender report and the report blocks it carries.
    SenderReport {
        /// The sender information.
        info: SenderInfo,
        /// The report blocks.
        reports: Reports<'a>,
    },
    /// A receiver report.
    ReceiverReport {
        /// The reporter's synchronization source identifier.
        ssrc: u32,
        /// The report blocks.
        reports: Reports<'a>,
    },
    /// The listed sources are leaving.
    Bye {
        /// The sources, 4 bytes each, big-endian.
        sources: &'a [u8],
        /// The reason the sender gave, as it was sent (RFC 3550 says UTF-8).
        reason: Option<&'a [u8]>,
    },
    /// A generic NACK (RFC 4585 section 6.2.1).
    Nack {
        /// The sender of the NACK.
        sender_ssrc: u32,
        /// The source whose packets are asked for.
        media_ssrc: u32,
        /// The sequence numbers asked for.
        lost: Lost<'a>,
    },
    /// A picture loss indication (RFC 4585 section 6.3.1): the sender is asked for a key frame.
    Pli {
        /// The sender of the indication.
        sender_ssrc: u32,
        /// The source asked for a key frame.
        media_ssrc: u32,
    },
    /// Any other packet, such as source descriptions, application-defined packets and other
    /// feedback messages.
    Other {
        /// The packet type.
        packet_type: u8,
        /// The 5-bit count field (the feedback message type for feedback packets).
        count: u8,
        /// The bytes after the 4-byte header, without padding.
        body: &'a [u8],
    },
}

impl<'a> Rtcp<'a> {
    /// The sources of a [`Rtcp::Bye`], or nothing.
    pub fn bye_sources(&self) -> impl Iterator<Item = u32> + 'a {
        let sources = match self {
            Rtcp::Bye { sources, .. } => *sources,
            _ => &[],
        };
        sources.as_chunks::<4>().0.iter().map(|c| u32::from_be_bytes(*c))
    }
}

/// The packets of a compound RTCP datagram, read one at a time. After an error the iteration
/// ends.
///
/// Each packet must have version 2 and a length that stays inside the datagram. Only the last one
/// may have padding. The datagram need not start with a sender or receiver report, so reduced-size
/// RTCP (RFC 5506), such as a NACK alone, is read too.
#[derive(Debug, Clone)]
pub struct Compound<'a> {
    rest: &'a [u8],
}

impl<'a> Compound<'a> {
    /// The packets of `datagram`.
    pub fn new(datagram: &'a [u8]) -> Compound<'a> {
        Compound { rest: datagram }
    }
}

impl<'a> Iterator for Compound<'a> {
    type Item = Result<Rtcp<'a>, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.rest.is_empty() {
            return None;
        }
        let item = next_packet(self.rest);
        match item {
            Ok((packet, rest)) => {
                self.rest = rest;
                Some(Ok(packet))
            }
            Err(e) => {
                self.rest = &[];
                Some(Err(e))
            }
        }
    }
}

fn next_packet(b: &[u8]) -> Result<(Rtcp<'_>, &[u8]), Error> {
    let (Some(&b0), Some(&packet_type), Some(words)) = (b.first(), b.get(1), be16(b, 2)) else {
        return Err(Error::Truncated("the RTCP header"));
    };
    if b0 >> 6 != 2 {
        return Err(Error::Invalid("the RTCP version is not 2"));
    }
    let total = (usize::from(words) + 1) * 4;
    let whole = b.get(..total).ok_or(Error::Truncated("an RTCP packet"))?;
    let rest = b.get(total..).unwrap_or(&[]);
    let mut body = whole.get(4..).unwrap_or(&[]);
    if b0 & 0x20 != 0 {
        if !rest.is_empty() {
            return Err(Error::Invalid("an RTCP packet before the last one has padding"));
        }
        let n = usize::from(whole.last().copied().unwrap_or(0));
        if n == 0 || n > body.len() {
            return Err(Error::Invalid("the RTCP padding count is 0 or longer than the packet"));
        }
        body = body.get(..body.len() - n).unwrap_or(&[]);
    }
    let count = b0 & 0x1F;
    let reports_of = |at: usize| reports(body, at, count);
    let packet = match packet_type {
        SR => {
            let truncated = Error::Truncated("the sender information");
            let info = SenderInfo {
                ssrc: be32(body, 0).ok_or(truncated)?,
                ntp: be64(body, 4).ok_or(truncated)?,
                rtp_timestamp: be32(body, 12).ok_or(truncated)?,
                packets: be32(body, 16).ok_or(truncated)?,
                octets: be32(body, 20).ok_or(truncated)?,
            };
            Rtcp::SenderReport { info, reports: reports_of(24)? }
        }
        RR => Rtcp::ReceiverReport { ssrc: be32(body, 0).ok_or(Error::Truncated("the receiver report"))?, reports: reports_of(4)? },
        BYE => {
            let end = usize::from(count) * 4;
            let sources = body.get(..end).ok_or(Error::Truncated("the BYE sources"))?;
            let reason = match body.get(end) {
                Some(&n) => Some(body.get(end + 1..end + 1 + usize::from(n)).ok_or(Error::Truncated("the BYE reason"))?),
                None => None,
            };
            Rtcp::Bye { sources, reason }
        }
        RTPFB | PSFB => {
            let truncated = Error::Truncated("the feedback message");
            let sender_ssrc = be32(body, 0).ok_or(truncated)?;
            let media_ssrc = be32(body, 4).ok_or(truncated)?;
            let fci = body.get(8..).unwrap_or(&[]);
            match (packet_type, count) {
                (RTPFB, 1) => {
                    if fci.is_empty() || fci.len() % 4 != 0 {
                        return Err(Error::Invalid("a generic NACK holds one or more 4-byte entries"));
                    }
                    Rtcp::Nack { sender_ssrc, media_ssrc, lost: Lost { fci, bit: 0 } }
                }
                (PSFB, 1) => Rtcp::Pli { sender_ssrc, media_ssrc },
                _ => Rtcp::Other { packet_type, count, body },
            }
        }
        _ => Rtcp::Other { packet_type, count, body },
    };
    Ok((packet, rest))
}

fn reports(body: &[u8], at: usize, count: u8) -> Result<Reports<'_>, Error> {
    let end = at + usize::from(count) * REPORT_BLOCK_LEN;
    body.get(at..end).map(Reports).ok_or(Error::Truncated("the report blocks"))
}

/// Writes a packet header for `count` and `packet_type`, then the body `fill` writes, and sets
/// the length. The body must be a whole number of 32-bit words.
fn write_packet(out: &mut Vec<u8>, count: u8, packet_type: u8, fill: impl FnOnce(&mut Vec<u8>)) -> Result<(), Error> {
    let start = out.len();
    out.extend_from_slice(&[0x80 | (count & 0x1F), packet_type, 0, 0]);
    fill(out);
    let len = out.len() - start;
    let words = len / 4 - 1;
    if !len.is_multiple_of(4) || words > usize::from(u16::MAX) {
        out.truncate(start);
        return Err(Error::TooLarge { size: len, limit: (usize::from(u16::MAX) + 1) * 4 });
    }
    if let Some(w) = out.get_mut(start + 2..start + 4) {
        w.copy_from_slice(&(words as u16).to_be_bytes());
    }
    Ok(())
}

fn check_reports(reports: &[ReportBlock]) -> Result<u8, Error> {
    if reports.len() > MAX_COUNT {
        return Err(Error::BadInput("a report holds at most 31 report blocks"));
    }
    Ok(reports.len() as u8)
}

/// Appends a sender report with `reports`. More than 31 blocks are refused.
pub fn write_sender_report(out: &mut Vec<u8>, info: &SenderInfo, reports: &[ReportBlock]) -> Result<(), Error> {
    let count = check_reports(reports)?;
    write_packet(out, count, SR, |out| {
        out.extend_from_slice(&info.ssrc.to_be_bytes());
        out.extend_from_slice(&info.ntp.to_be_bytes());
        out.extend_from_slice(&info.rtp_timestamp.to_be_bytes());
        out.extend_from_slice(&info.packets.to_be_bytes());
        out.extend_from_slice(&info.octets.to_be_bytes());
        reports.iter().for_each(|r| r.write(out));
    })
}

/// Appends a receiver report from `ssrc` with `reports`. More than 31 blocks are refused. A
/// cumulative loss outside the signed 24-bit range is written as the nearest value inside it.
pub fn write_receiver_report(out: &mut Vec<u8>, ssrc: u32, reports: &[ReportBlock]) -> Result<(), Error> {
    let count = check_reports(reports)?;
    write_packet(out, count, RR, |out| {
        out.extend_from_slice(&ssrc.to_be_bytes());
        reports.iter().for_each(|r| r.write(out));
    })
}

/// Appends a BYE for `sources` with an optional reason. More than 31 sources and a reason over
/// 255 bytes are refused.
pub fn write_bye(out: &mut Vec<u8>, sources: &[u32], reason: Option<&str>) -> Result<(), Error> {
    if sources.len() > MAX_COUNT {
        return Err(Error::BadInput("a BYE lists at most 31 sources"));
    }
    if reason.is_some_and(|r| r.len() > usize::from(u8::MAX)) {
        return Err(Error::BadInput("a BYE reason is at most 255 bytes"));
    }
    write_packet(out, sources.len() as u8, BYE, |out| {
        sources.iter().for_each(|s| out.extend_from_slice(&s.to_be_bytes()));
        if let Some(r) = reason {
            out.push(r.len() as u8);
            out.extend_from_slice(r.as_bytes());
            while out.len() % 4 != 0 {
                out.push(0);
            }
        }
    })
}

/// Appends a generic NACK from `sender_ssrc` asking `media_ssrc` for the `lost` sequence numbers.
/// Each entry covers a packet id and the 16 that follow it, so numbers in increasing order (with
/// wrap-around) take the fewest entries. An empty list is refused.
pub fn write_nack(out: &mut Vec<u8>, sender_ssrc: u32, media_ssrc: u32, lost: &[u16]) -> Result<(), Error> {
    if lost.is_empty() {
        return Err(Error::BadInput("a NACK asks for at least one packet"));
    }
    write_packet(out, 1, RTPFB, |out| {
        out.extend_from_slice(&sender_ssrc.to_be_bytes());
        out.extend_from_slice(&media_ssrc.to_be_bytes());
        let mut entry: Option<(u16, u16)> = None;
        for &seq in lost {
            if let Some((pid, blp)) = entry.as_mut() {
                let d = seq.wrapping_sub(*pid);
                if (1..=16).contains(&d) {
                    *blp |= 1 << (d - 1);
                    continue;
                }
                out.extend_from_slice(&pid.to_be_bytes());
                out.extend_from_slice(&blp.to_be_bytes());
            }
            entry = Some((seq, 0));
        }
        if let Some((pid, blp)) = entry {
            out.extend_from_slice(&pid.to_be_bytes());
            out.extend_from_slice(&blp.to_be_bytes());
        }
    })
}

/// Appends a picture loss indication from `sender_ssrc` to `media_ssrc`.
pub fn write_pli(out: &mut Vec<u8>, sender_ssrc: u32, media_ssrc: u32) -> Result<(), Error> {
    write_packet(out, 1, PSFB, |out| {
        out.extend_from_slice(&sender_ssrc.to_be_bytes());
        out.extend_from_slice(&media_ssrc.to_be_bytes());
    })
}

/// Nanoseconds since the NTP epoch (1900-01-01) of a 64-bit NTP timestamp.
pub fn ntp_nanos(ntp: u64) -> u128 {
    let secs = u128::from(ntp >> 32);
    let frac = u128::from(ntp & 0xFFFF_FFFF);
    secs * 1_000_000_000 + ((frac * 1_000_000_000) >> 32)
}

/// The middle 32 bits of a 64-bit NTP timestamp, which a report block's last-SR field holds.
pub fn ntp_middle(ntp: u64) -> u32 {
    (ntp >> 16) as u32
}

/// What a sender report did to a [`TimeMap`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mapping {
    /// It was the first, and the mapping starts with it.
    First,
    /// It agreed with the mapping within [`MAX_JUMP`], and the mapping now uses it.
    Kept,
    /// It differed by more than [`MAX_JUMP`], for example after the sender's clock was set, and
    /// the mapping starts again from it.
    Reset,
}

/// The sender's wall-clock time of an RTP timestamp, from the stream's sender reports.
///
/// Within one sender, the reports of its audio and video streams share one wall clock, so the
/// times this gives for the two streams line them up. RTP timestamps up to 2^31 units before or
/// after the last report map; that is about 6.6 hours at 90 kHz.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeMap {
    clock_rate: u32,
    anchor: Option<(u128, u32)>,
}

impl TimeMap {
    /// A mapping for a stream whose RTP clock runs at `clock_rate` Hz. A rate of 0 is refused.
    pub fn new(clock_rate: u32) -> Result<TimeMap, Error> {
        if clock_rate == 0 {
            return Err(Error::BadInput("the RTP clock rate is 0"));
        }
        Ok(TimeMap { clock_rate, anchor: None })
    }

    /// Takes the NTP and RTP times of a sender report.
    pub fn sender_report(&mut self, ntp: u64, rtp_timestamp: u32) -> Mapping {
        let now = ntp_nanos(ntp);
        let kind = match self.wall_nanos_of(rtp_timestamp) {
            None if self.anchor.is_none() => Mapping::First,
            Some(predicted) if predicted.abs_diff(now as i128) <= MAX_JUMP.as_nanos() => Mapping::Kept,
            _ => Mapping::Reset,
        };
        self.anchor = Some((now, rtp_timestamp));
        kind
    }

    /// Nanoseconds since the NTP epoch on the sender's wall clock for `rtp_timestamp`, or `None`
    /// before the first sender report or when the time would be before the epoch.
    pub fn nanos(&self, rtp_timestamp: u32) -> Option<u128> {
        u128::try_from(self.wall_nanos_of(rtp_timestamp)?).ok()
    }

    fn wall_nanos_of(&self, rtp_timestamp: u32) -> Option<i128> {
        let (ns, rtp) = self.anchor?;
        let ticks = i128::from(rtp_timestamp.wrapping_sub(rtp) as i32);
        Some(i128::try_from(ns).ok()? + ticks * 1_000_000_000 / i128::from(self.clock_rate))
    }
}
