//! CEA-608 pairs in an H.264 or HEVC SEI message, and back.
//!
//! - `cc_data` (ATSC A/53 Part 4): `0xC0 | cc_count` (process_cc_data_flag set), the reserved
//!   byte `0xFF`, then per pair `0xFC` (marker bits, cc_valid, cc_type 0: field 1) and the two
//!   bytes, then the marker byte `0xFF`.
//! - The SEI payload, `user_data_registered_itu_t_t35` (payload type 4): country code `0xB5`,
//!   provider code `0x0031`, the user identifier `GA94`, user_data_type_code `0x03` and the
//!   `cc_data`.
//! - The NAL unit: the header (H.264 type 6; HEVC prefix SEI, type 39, layer 0, temporal id 0),
//!   the message, the RBSP trailing bits `0x80`, with an emulation prevention byte `0x03` after
//!   each `00 00` that a byte from `00` to `03` follows.
//! - Access units are length-prefixed: each NAL unit follows its length as 4 bytes, big-endian.

/// The video codec of the access units.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    /// ITU-T H.264.
    H264,
    /// ITU-T H.265.
    Hevc,
}

/// Why an SEI NAL unit could not be put into an access unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InsertError {
    /// A NAL unit's length runs past the end of the access unit.
    Truncated,
    /// The access unit has no picture NAL unit.
    NoPicture,
    /// The SEI NAL unit is longer than a 4-byte length can say.
    TooLong,
}

impl std::fmt::Display for InsertError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            InsertError::Truncated => "a NAL unit runs past the end of the access unit",
            InsertError::NoPicture => "the access unit has no picture",
            InsertError::TooLong => "the SEI NAL unit is too long",
        })
    }
}

impl std::error::Error for InsertError {}

/// SEI payload type `user_data_registered_itu_t_t35`.
const T35: u8 = 4;
/// The payload up to `cc_data`: the United States, ATSC, `GA94`, closed captions.
const T35_HEAD: [u8; 8] = [0xB5, 0x00, 0x31, b'G', b'A', b'9', b'4', 0x03];
/// A valid field 1 pair.
const FIELD_1: u8 = 0xFC;

/// Append the `cc_data` of `pairs` (at most [`crate::MAX_PAIRS_PER_FRAME`]; more are left out)
/// to `out`.
pub fn cc_data(pairs: &[[u8; 2]], out: &mut Vec<u8>) {
    let pairs = pairs.get(..crate::MAX_PAIRS_PER_FRAME).unwrap_or(pairs);
    out.push(0xC0 | pairs.len() as u8);
    out.push(0xFF);
    for p in pairs {
        out.extend_from_slice(&[FIELD_1, p[0], p[1]]);
    }
    out.push(0xFF);
}

/// Append `b` to `out` with emulation prevention, `zeros` counting the zero bytes just before.
pub(crate) fn escaped(out: &mut Vec<u8>, zeros: &mut usize, b: u8) {
    if *zeros >= 2 && b <= 3 {
        out.push(3);
        *zeros = 0;
    }
    out.push(b);
    *zeros = if b == 0 { *zeros + 1 } else { 0 };
}

/// Append the SEI NAL unit (header included, without a length or start code) carrying `pairs`
/// to `out`.
pub fn sei_nal(codec: Codec, pairs: &[[u8; 2]], out: &mut Vec<u8>) {
    match codec {
        Codec::H264 => out.push(0x06),
        Codec::Hevc => out.extend_from_slice(&[39 << 1, 0x01]),
    }
    let mut payload = Vec::with_capacity(T35_HEAD.len() + 3 + 3 * pairs.len().min(crate::MAX_PAIRS_PER_FRAME));
    payload.extend_from_slice(&T35_HEAD);
    cc_data(pairs, &mut payload);
    let mut zeros = 0;
    escaped(out, &mut zeros, T35);
    let mut size = payload.len();
    while size >= 255 {
        escaped(out, &mut zeros, 0xFF);
        size -= 255;
    }
    escaped(out, &mut zeros, size as u8);
    for &b in &payload {
        escaped(out, &mut zeros, b);
    }
    escaped(out, &mut zeros, 0x80);
}

/// The NAL units of a length-prefixed access unit: each one's offset of its length field and its
/// bytes. Stops at the first length that runs past the end.
fn nal_units(au: &[u8]) -> impl Iterator<Item = Result<(usize, &[u8]), InsertError>> + '_ {
    let mut at = 0usize;
    std::iter::from_fn(move || {
        if at >= au.len() {
            return None;
        }
        let start = at;
        let Some(len) = au.get(at..at.saturating_add(4)).and_then(|b| <[u8; 4]>::try_from(b).ok()) else {
            at = au.len();
            return Some(Err(InsertError::Truncated));
        };
        let len = u32::from_be_bytes(len) as usize;
        let body = start.checked_add(4).and_then(|s| s.checked_add(len).map(|e| (s, e)));
        match body.and_then(|(s, e)| au.get(s..e)) {
            Some(nal) => {
                at = start + 4 + len;
                Some(Ok((start, nal)))
            }
            None => {
                at = au.len();
                Some(Err(InsertError::Truncated))
            }
        }
    })
}

/// The NAL unit type.
fn nal_type(codec: Codec, nal: &[u8]) -> Option<u8> {
    let h = *nal.first()?;
    Some(match codec {
        Codec::H264 => h & 0x1f,
        Codec::Hevc => (h >> 1) & 0x3f,
    })
}

/// Whether a NAL unit of `ty` holds part of a picture.
fn is_picture(codec: Codec, ty: u8) -> bool {
    match codec {
        Codec::H264 => (1..=5).contains(&ty),
        Codec::Hevc => ty <= 31,
    }
}

/// Put the NAL unit `nal` into the length-prefixed access unit `au`, before its first picture
/// NAL unit. `au` is unchanged on an error.
pub fn insert_sei(codec: Codec, au: &mut Vec<u8>, nal: &[u8]) -> Result<(), InsertError> {
    let len = u32::try_from(nal.len()).map_err(|_| InsertError::TooLong)?;
    let mut at = None;
    for unit in nal_units(au) {
        let (start, unit) = unit?;
        if nal_type(codec, unit).is_some_and(|t| is_picture(codec, t)) {
            at = Some(start);
            break;
        }
    }
    let at = at.ok_or(InsertError::NoPicture)?;
    au.splice(at..at, len.to_be_bytes().into_iter().chain(nal.iter().copied()));
    Ok(())
}

/// The RBSP of a NAL unit's body: emulation prevention bytes removed.
pub(crate) fn unescape(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len());
    let mut zeros = 0;
    for &b in body {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}

/// Read one SEI value coded as a run of `0xFF` bytes and a last byte.
fn sei_value(rbsp: &[u8], at: &mut usize) -> Option<usize> {
    let mut v = 0usize;
    loop {
        let b = *rbsp.get(*at)?;
        *at += 1;
        v = v.checked_add(usize::from(b))?;
        if b != 0xFF {
            return Some(v);
        }
    }
}

/// Append the valid field 1 pairs of one `user_data_registered_itu_t_t35` payload.
fn t35_pairs(payload: &[u8], out: &mut Vec<[u8; 2]>) {
    let Some(rest) = payload.strip_prefix(&T35_HEAD[..]) else { return };
    let Some(&flags) = rest.first() else { return };
    if flags & 0x40 == 0 {
        return;
    }
    let count = usize::from(flags & 0x1f);
    for i in 0..count {
        let Some(t) = rest.get(2 + 3 * i..5 + 3 * i) else { return };
        if t[0] & 0x04 != 0 && t[0] & 0x03 == 0 {
            out.push([t[1], t[2]]);
        }
    }
}

/// The field 1 CEA-608 pairs carried in the closed-caption SEI messages of the length-prefixed
/// access unit `au`, in order. A malformed NAL unit or message ends the reading of that unit.
pub fn read_sei(codec: Codec, au: &[u8]) -> Vec<[u8; 2]> {
    let mut out = Vec::new();
    let header = match codec {
        Codec::H264 => 1,
        Codec::Hevc => 2,
    };
    for unit in nal_units(au) {
        let Ok((_, nal)) = unit else { break };
        let sei = match codec {
            Codec::H264 => nal_type(codec, nal) == Some(6),
            Codec::Hevc => matches!(nal_type(codec, nal), Some(39 | 40)),
        };
        if !sei {
            continue;
        }
        let rbsp = unescape(nal.get(header..).unwrap_or_default());
        let mut at = 0;
        // Messages run until the trailing bits, the last byte.
        while at + 1 < rbsp.len() {
            let (Some(ty), Some(size)) = (sei_value(&rbsp, &mut at), sei_value(&rbsp, &mut at)) else { break };
            let Some(payload) = at.checked_add(size).and_then(|end| rbsp.get(at..end)) else { break };
            if ty == usize::from(T35) {
                t35_pairs(payload, &mut out);
            }
            at += size;
        }
    }
    out
}
