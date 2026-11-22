//! Annex B byte streams (ITU-T H.264 and H.265 Annex B): the NAL units between start codes, and
//! the samples a container stores, each NAL unit after a 4-byte big-endian length. [`nals`] is
//! `craft-media`'s `annexb_nals`, which splits the stream at its start codes.

pub use craft_media::bitstream::annexb_nals as nals;

use crate::Codec;

/// The NAL unit type of `nal`, or `None` when it is too short for its header. AV1 has no NAL
/// units, so it is always `None` for AV1.
pub fn nal_type(codec: Codec, nal: &[u8]) -> Option<u8> {
    match codec {
        Codec::H264 => nal.first().map(|b| b & 0x1f),
        Codec::Hevc => match nal {
            [b, _, ..] => Some((b >> 1) & 0x3f),
            _ => None,
        },
        Codec::Av1 => None,
    }
}

/// Annex B to 4-byte length-prefixed NAL units. The parameter sets go in the `avcC` or `hvcC`
/// record, so H.264 drops SPS, PPS and access unit delimiters (types 7 to 9), and HEVC drops VPS,
/// SPS, PPS, access unit delimiters and end-of-sequence and end-of-bitstream units (32 to 37). A
/// stream with nothing left is an error, and so is an AV1 stream, whose samples [`crate::av1`]
/// writes.
pub fn to_length_prefixed(codec: Codec, data: &[u8]) -> Result<Vec<u8>, String> {
    if codec == Codec::Av1 {
        return Err("an AV1 stream is made of OBUs, not Annex B NAL units".into());
    }
    let mut out = Vec::with_capacity(data.len());
    for n in nals(data) {
        let dropped = match (codec, nal_type(codec, n)) {
            (_, None) | (Codec::Av1, _) => true,
            (Codec::H264, Some(t)) => matches!(t, 7..=9),
            (Codec::Hevc, Some(t)) => matches!(t, 32..=37),
        };
        if dropped {
            continue;
        }
        let len = u32::try_from(n.len()).map_err(|_| "a NAL unit longer than 4 GiB".to_string())?;
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(n);
    }
    if out.is_empty() {
        return Err("the encoder produced no picture data".into());
    }
    Ok(out)
}

/// The first NAL unit of each type in `types`, in that order, or an error naming the missing ones.
pub fn parameter_sets(codec: Codec, data: &[u8], types: &[(u8, &str)]) -> Result<Vec<Vec<u8>>, String> {
    let nals = nals(data);
    let mut found = Vec::with_capacity(types.len());
    let mut missing = Vec::new();
    for &(t, name) in types {
        match nals.iter().find(|n| nal_type(codec, n) == Some(t) && n.len() > 1) {
            Some(n) => found.push(n.to_vec()),
            None => missing.push(name),
        }
    }
    if missing.is_empty() { Ok(found) } else { Err(format!("the encoder returned no {}", missing.join(" or "))) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_start_code_forms_split() {
        let s = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9, 9, 0];
        assert_eq!(nals(&s), vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 9, 9][..]]);
        assert_eq!(to_length_prefixed(Codec::H264, &s).unwrap(), [0, 0, 0, 3, 0x65, 9, 9]);
        let ps = parameter_sets(Codec::H264, &s, &[(7, "SPS"), (8, "PPS")]).unwrap();
        assert_eq!(ps, vec![vec![0x67, 1, 2], vec![0x68, 3]]);
    }

    #[test]
    fn hostile_streams_are_errors() {
        for s in [&[][..], &[0, 0, 1], &[1, 2, 3], &[0, 0, 1, 0x09, 0xf0], &[0, 0, 0, 1, 0x67, 1], &[0, 0, 1, 0, 0, 1]] {
            assert!(to_length_prefixed(Codec::H264, s).is_err(), "{s:?}");
        }
        let e = parameter_sets(Codec::H264, &[0, 0, 1, 0x65, 1], &[(7, "SPS"), (8, "PPS")]).unwrap_err();
        assert!(e.contains("SPS or PPS"), "{e}");
    }

    // HEVC headers: VPS 0x40, SPS 0x42, PPS 0x44, AUD 0x46, EOS 0x48, EOB 0x4a, IDR_W_RADL 0x26,
    // IDR_N_LP 0x28, TRAIL_R 0x02, prefix SEI 0x4e, suffix SEI 0x50.
    #[test]
    fn hevc_types_come_from_the_first_header_byte() {
        assert_eq!(nal_type(Codec::Hevc, &[0x40, 0x01]), Some(32));
        assert_eq!(nal_type(Codec::Hevc, &[0x28, 0x01]), Some(20));
        assert_eq!(nal_type(Codec::H264, &[0x28, 0x01]), Some(8), "the H.264 reading of the same byte");
        assert_eq!(nal_type(Codec::Hevc, &[0x40]), None);
        assert_eq!(nal_type(Codec::H264, &[]), None);
        assert_eq!(nal_type(Codec::Av1, &[0x40, 0x01]), None, "AV1 has no NAL units");
        let s = [0, 0, 0, 1, 0x65, 9];
        assert!(to_length_prefixed(Codec::Av1, &s).unwrap_err().contains("OBUs"));
    }

    #[test]
    fn hevc_samples_keep_slices_and_sei_and_lose_parameter_sets() {
        let s = [
            &[0, 0, 0, 1, 0x46, 0x01, 0x50][..],
            &[0, 0, 0, 1, 0x40, 0x01, 1, 2],
            &[0, 0, 0, 1, 0x42, 0x01, 3],
            &[0, 0, 1, 0x44, 0x01, 4],
            &[0, 0, 1, 0x4e, 0x01, 5, 5],
            &[0, 0, 0, 1, 0x28, 0x01, 6, 6, 6],
            &[0, 0, 1, 0x02, 0x01, 8],
            &[0, 0, 1, 0x48, 0x01],
            &[0, 0, 1, 0x4a, 0x01],
        ]
        .concat();
        let expected = [&[0, 0, 0, 4, 0x4e, 0x01, 5, 5][..], &[0, 0, 0, 5, 0x28, 0x01, 6, 6, 6], &[0, 0, 0, 3, 0x02, 0x01, 8]].concat();
        assert_eq!(to_length_prefixed(Codec::Hevc, &s).unwrap(), expected);
        let ps = parameter_sets(Codec::Hevc, &s, &[(32, "VPS"), (33, "SPS"), (34, "PPS")]).unwrap();
        assert_eq!(ps, vec![vec![0x40, 1, 1, 2], vec![0x42, 1, 3], vec![0x44, 1, 4]]);
        assert!(to_length_prefixed(Codec::Hevc, &[0, 0, 1, 0x46, 0x01, 0x50, 0, 0, 1, 0x26]).is_err());
    }
}
