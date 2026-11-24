use crate::{AacConfig, Codec, Depacketizer, Error};

#[derive(Debug, PartialEq, Eq)]
struct Got {
    timestamp: u32,
    key: bool,
    data: Vec<u8>,
}

/// Pushes each `(timestamp, marker, payload)` and collects the frames and errors.
fn run(d: &mut Depacketizer, packets: &[(u32, bool, &[u8])]) -> (Vec<Got>, Vec<Error>) {
    let mut frames = Vec::new();
    let mut errors = Vec::new();
    for &(timestamp, marker, payload) in packets {
        let r = d.push(timestamp, marker, payload, |f| frames.push(Got { timestamp: f.timestamp, key: f.key, data: f.data.to_vec() }));
        if let Err(e) = r {
            errors.push(e);
        }
    }
    (frames, errors)
}

fn annexb(nals: &[&[u8]]) -> Vec<u8> {
    nals.iter().flat_map(|n| [&[0, 0, 0, 1][..], n].concat()).collect()
}

const IDR: &[u8] = &[0x65, 0x88, 0x84, 0x21];
const SLICE: &[u8] = &[0x41, 0x9A, 0x02];
const SPS: &[u8] = &[0x67, 0x42, 0xC0, 0x1F];
const PPS: &[u8] = &[0x68, 0xCE, 0x3C, 0x80];

#[test]
fn h264_single_nal_units_become_annex_b_access_units() {
    let mut d = Depacketizer::new(Codec::H264).unwrap();
    assert!(d.wants_key_frame());
    let (frames, errors) = run(&mut d, &[(90, false, SPS), (90, false, PPS), (90, true, IDR), (180, true, SLICE)]);
    assert_eq!(errors, []);
    assert_eq!(
        frames,
        [Got { timestamp: 90, key: true, data: annexb(&[SPS, PPS, IDR]) }, Got { timestamp: 180, key: false, data: annexb(&[SLICE]) }]
    );
    assert!(!d.wants_key_frame());
}

#[test]
fn h264_video_waits_for_a_key_frame() {
    let mut d = Depacketizer::new(Codec::H264).unwrap();
    let (frames, _) = run(&mut d, &[(0, true, SLICE), (3000, true, IDR), (6000, true, SLICE)]);
    assert_eq!(frames.iter().map(|f| f.timestamp).collect::<Vec<_>>(), [3000, 6000]);
    assert_eq!(d.stats().dropped, 1);
    assert_eq!(d.stats().frames, 2);
}

#[test]
fn h264_stap_a_holds_several_nal_units() {
    let mut d = Depacketizer::new(Codec::H264).unwrap();
    let mut stap = vec![0x78];
    for nal in [SPS, PPS, IDR] {
        stap.extend_from_slice(&(nal.len() as u16).to_be_bytes());
        stap.extend_from_slice(nal);
    }
    let (frames, errors) = run(&mut d, &[(0, true, &stap)]);
    assert_eq!(errors, []);
    assert_eq!(frames, [Got { timestamp: 0, key: true, data: annexb(&[SPS, PPS, IDR]) }]);
    let (_, errors) = run(&mut d, &[(1, true, &[0x78, 0, 0]), (2, true, &[0x78, 0, 9, 1]), (3, true, &[0x78])]);
    assert_eq!(
        errors,
        [
            Error::Invalid("an STAP-A NAL unit size is 0"),
            Error::Truncated("an STAP-A NAL unit"),
            Error::Truncated("an STAP-A packet holds no NAL unit")
        ]
    );
}

/// FU-A fragments of `nal`, `size` payload bytes each, per RFC 6184 section 5.8.
fn fu_a(nal: &[u8], size: usize) -> Vec<Vec<u8>> {
    let indicator = (nal[0] & 0xE0) | 28;
    let chunks: Vec<&[u8]> = nal[1..].chunks(size).collect();
    let last = chunks.len() - 1;
    chunks
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let s = if i == 0 { 0x80 } else { 0 };
            let e = if i == last { 0x40 } else { 0 };
            [&[indicator, s | e | (nal[0] & 0x1F)][..], c].concat()
        })
        .collect()
}

/// An IDR slice of 51 bytes whose slice header starts at macroblock 0.
fn long_idr() -> Vec<u8> {
    [0x65, 0x88].into_iter().chain(2..=50).collect()
}

#[test]
fn h264_fu_a_fragments_are_joined() {
    let nal = long_idr();
    let parts = fu_a(&nal, 16);
    assert_eq!(parts.len(), 4);
    let mut d = Depacketizer::new(Codec::H264).unwrap();
    let packets: Vec<(u32, bool, &[u8])> = parts.iter().enumerate().map(|(i, p)| (7, i == 3, p.as_slice())).collect();
    let (frames, errors) = run(&mut d, &packets);
    assert_eq!(errors, []);
    assert_eq!(frames, [Got { timestamp: 7, key: true, data: annexb(&[&nal]) }]);
}

#[test]
fn h264_a_lost_middle_fragment_drops_the_access_unit_until_the_next_key_frame() {
    let nal = long_idr();
    let parts = fu_a(&nal, 16);
    let mut d = Depacketizer::new(Codec::H264).unwrap();
    run(&mut d, &[(1, true, IDR)]);
    assert!(!d.wants_key_frame());
    run(&mut d, &[(2, false, &parts[0]), (2, false, &parts[1])]);
    // The jitter buffer reports the gap where the third fragment was.
    d.lost();
    let (frames, _) = run(&mut d, &[(2, true, &parts[3]), (3, true, SLICE)]);
    assert_eq!(frames, []);
    assert!(d.wants_key_frame());
    let (frames, _) = run(&mut d, &[(4, true, IDR)]);
    assert_eq!(frames.len(), 1);
    // The damaged access unit at 2 and the slice at 3, each counted once.
    assert_eq!(d.stats().dropped, 2);

    // Without a gap report, a fragment run that starts without its first fragment is dropped too.
    let mut d = Depacketizer::new(Codec::H264).unwrap();
    let (frames, _) = run(&mut d, &[(1, false, &parts[1]), (1, true, &parts[3])]);
    assert_eq!(frames, []);
    // And so is one that never ends.
    let (frames, _) = run(&mut d, &[(2, true, &parts[0])]);
    assert_eq!(frames, []);
}

#[test]
fn h264_after_a_gap_between_frames_video_waits_for_a_key_frame() {
    let mut d = Depacketizer::new(Codec::H264).unwrap();
    run(&mut d, &[(1, true, IDR)]);
    // A whole access unit was lost between two complete ones.
    d.lost();
    assert!(d.wants_key_frame());
    let (frames, _) = run(&mut d, &[(3, true, SLICE), (4, true, IDR)]);
    assert_eq!(frames.iter().map(|f| f.timestamp).collect::<Vec<_>>(), [4]);
    // Audio does not wait.
    let mut a = Depacketizer::new(Codec::Opus).unwrap();
    a.lost();
    assert!(!a.wants_key_frame());
}

#[test]
fn h264_a_frame_ends_when_the_timestamp_changes() {
    let mut d = Depacketizer::new(Codec::H264).unwrap();
    let (frames, _) = run(&mut d, &[(1, false, IDR), (2, false, SLICE)]);
    assert_eq!(frames, [Got { timestamp: 1, key: true, data: annexb(&[IDR]) }]);
}

#[test]
fn h264_payloads_that_do_not_read_are_refused() {
    let mut d = Depacketizer::new(Codec::H264).unwrap();
    let (_, errors) = run(
        &mut d,
        &[
            (1, true, &[]),
            (1, true, &[0x80 | 5]),
            (1, true, &[0x00]),
            (1, true, &[0x1E]),
            (1, true, &[0x1A, 1, 2]),
            (1, true, &[0x1C]),
            (1, true, &[0x1C, 0xC5, 1]),
        ],
    );
    assert_eq!(
        errors,
        [
            Error::Truncated("the H.264 payload header"),
            Error::Invalid("an H.264 payload has the forbidden bit set"),
            Error::Invalid("an H.264 payload has a reserved NAL unit type"),
            Error::Invalid("an H.264 payload has a reserved NAL unit type"),
            Error::Unsupported("the interleaved H.264 packetization mode"),
            Error::Truncated("the FU-A header"),
            Error::Invalid("a FU-A fragment has both the start and end bits set"),
        ]
    );
    assert_eq!(d.stats().errors, 7);
}

#[test]
fn h264_frames_over_the_limit_are_dropped() {
    let mut d = Depacketizer::with_max_frame(Codec::H264, 10).unwrap();
    let big = [0x65; 20];
    let (frames, errors) = run(&mut d, &[(1, true, &big), (2, true, IDR)]);
    assert_eq!(errors, [Error::TooLarge { size: 24, limit: 10 }]);
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].timestamp, 2);
}

#[test]
fn vp8_partitions_are_joined_into_a_frame() {
    let mut d = Depacketizer::new(Codec::Vp8).unwrap();
    // Start of partition 0 with a 15-bit PictureID, TL0PICIDX and TID: X, S; I, L, T.
    let first = [0x90, 0xE0, 0x81, 0x23, 0x05, 0x40, 0x10, 0x02, 0x9D];
    // Partition 1 of the same frame, without the extension byte.
    let second = [0x01, 0x01, 0x02, 0x03];
    let (frames, errors) = run(&mut d, &[(10, false, &first), (10, true, &second)]);
    assert_eq!(errors, []);
    assert_eq!(frames, [Got { timestamp: 10, key: true, data: vec![0x10, 0x02, 0x9D, 0x01, 0x02, 0x03] }]);
    // An interframe: bit 0 of the frame's first byte is 1.
    let (frames, _) = run(&mut d, &[(20, true, &[0x10, 0x31, 0x00])]);
    assert_eq!(frames, [Got { timestamp: 20, key: false, data: vec![0x31, 0x00] }]);
}

#[test]
fn vp8_waits_for_a_key_frame_and_drops_frames_without_their_start() {
    let mut d = Depacketizer::new(Codec::Vp8).unwrap();
    let (frames, _) = run(&mut d, &[(1, true, &[0x10, 0x31]), (2, true, &[0x00, 0x10]), (3, true, &[0x10, 0x10])]);
    assert_eq!(frames.iter().map(|f| f.timestamp).collect::<Vec<_>>(), [3]);
    let (_, errors) = run(&mut d, &[(4, true, &[]), (4, true, &[0x80]), (4, true, &[0x90, 0x80]), (4, true, &[0x10])]);
    assert_eq!(
        errors,
        [
            Error::Truncated("the VP8 payload descriptor"),
            Error::Truncated("the VP8 payload descriptor"),
            Error::Truncated("the VP8 PictureID"),
            Error::Truncated("a VP8 payload holds no frame data"),
        ]
    );
}

#[test]
fn opus_packets_pass_through() {
    let mut d = Depacketizer::new(Codec::Opus).unwrap();
    let (frames, errors) = run(&mut d, &[(960, false, &[0xFC, 1, 2]), (1920, true, &[])]);
    assert_eq!(frames, [Got { timestamp: 960, key: true, data: vec![0xFC, 1, 2] }]);
    assert_eq!(errors, [Error::Invalid("an Opus packet is empty")]);
    assert!(!d.wants_key_frame());
}

/// An AAC-hbr payload: the AU-headers (13-bit size, 3-bit index of 0) and the units.
fn aac_hbr(units: &[&[u8]], size_override: Option<u16>) -> Vec<u8> {
    let mut p = ((units.len() * 16) as u16).to_be_bytes().to_vec();
    for u in units {
        let size = size_override.unwrap_or(u.len() as u16);
        p.extend_from_slice(&(size << 3).to_be_bytes());
    }
    for u in units {
        p.extend_from_slice(u);
    }
    p
}

#[test]
fn aac_hbr_units_get_their_own_timestamps() {
    let mut d = Depacketizer::new(Codec::Aac(AacConfig::HBR)).unwrap();
    let p = aac_hbr(&[&[1, 2, 3], &[4, 5]], None);
    let (frames, errors) = run(&mut d, &[(5000, true, &p)]);
    assert_eq!(errors, []);
    assert_eq!(frames, [Got { timestamp: 5000, key: true, data: vec![1, 2, 3] }, Got { timestamp: 6024, key: true, data: vec![4, 5] }]);
}

#[test]
fn aac_lbr_headers_are_read() {
    let mut d = Depacketizer::new(Codec::Aac(AacConfig::LBR)).unwrap();
    // Two 8-bit AU-headers: size 2 and size 1, each with an index of 0.
    let p = [0, 16, 2 << 2, 1 << 2, 9, 8, 7];
    let (frames, errors) = run(&mut d, &[(0, true, &p)]);
    assert_eq!(errors, []);
    assert_eq!(frames.iter().map(|f| f.data.clone()).collect::<Vec<_>>(), [vec![9, 8], vec![7]]);
}

#[test]
fn aac_fragments_are_joined_into_one_unit() {
    let mut d = Depacketizer::new(Codec::Aac(AacConfig::HBR)).unwrap();
    let unit: Vec<u8> = (0..30).collect();
    let a = aac_hbr(&[&unit[..12]], Some(30));
    let b = aac_hbr(&[&unit[12..24]], Some(30));
    let c = aac_hbr(&[&unit[24..]], Some(30));
    let (frames, errors) = run(&mut d, &[(100, false, &a), (100, false, &b), (100, true, &c)]);
    assert_eq!(errors, []);
    assert_eq!(frames, [Got { timestamp: 100, key: true, data: unit.clone() }]);
    // A fragment run cut off by a gap is dropped.
    run(&mut d, &[(200, false, &a)]);
    d.lost();
    let (frames, _) = run(&mut d, &[(200, true, &c)]);
    assert_eq!(frames, []);
    // So is one cut off by the next timestamp.
    let whole = aac_hbr(&[&[1]], None);
    let (frames, _) = run(&mut d, &[(300, false, &a), (400, true, &whole)]);
    assert_eq!(frames, [Got { timestamp: 400, key: true, data: vec![1] }]);
    assert_eq!(d.stats().dropped, 2);
}

#[test]
fn aac_payloads_that_do_not_read_are_refused() {
    let mut d = Depacketizer::new(Codec::Aac(AacConfig::HBR)).unwrap();
    let mut interleaved = aac_hbr(&[&[1], &[2]], None);
    interleaved[5] |= 1;
    let (_, errors) = run(
        &mut d,
        &[
            (0, true, &[0]),
            (0, true, &[0, 15, 0, 0]),
            (0, true, &[0, 16, 0]),
            (0, true, &aac_hbr(&[&[]], None)),
            (0, true, &aac_hbr(&[&[1, 2]], Some(5))),
            (0, true, &[0, 32, 0, 1 << 3, 0, 9 << 3, 1]),
            (0, true, &aac_hbr(&[&[1, 2]], Some(1))),
            (0, true, &interleaved),
        ],
    );
    assert_eq!(
        errors,
        [
            Error::Truncated("the AU-headers-length"),
            Error::Invalid("the AU-headers-length is not a whole number of AU-headers"),
            Error::Truncated("the AU-headers"),
            Error::Invalid("an AAC access unit size is 0"),
            Error::Invalid("an AAC access unit is longer than its packet"),
            Error::Truncated("an AAC access unit"),
            Error::Invalid("the AAC access units do not fill the packet"),
            Error::Unsupported("interleaved AAC access units"),
        ]
    );
    let bad = AacConfig { size_length: 0, ..AacConfig::HBR };
    assert!(matches!(Depacketizer::new(Codec::Aac(bad)), Err(Error::BadInput(_))));
    let bad = AacConfig { frame_samples: 0, ..AacConfig::HBR };
    assert!(matches!(Depacketizer::new(Codec::Aac(bad)), Err(Error::BadInput(_))));
}

#[test]
fn h264_a_broken_frame_asks_for_a_key_frame_at_once() {
    let mut d = Depacketizer::new(Codec::H264).unwrap();
    run(&mut d, &[(1, true, IDR)]);
    assert!(!d.wants_key_frame());
    let (_, errors) = run(&mut d, &[(2, false, &[])]);
    assert_eq!(errors, [Error::Truncated("the H.264 payload header")]);
    assert!(d.wants_key_frame());
}

/// The second slice of an IDR picture: `first_mb_in_slice` is 1 (`010`).
const IDR_SECOND_SLICE: &[u8] = &[0x65, 0x40, 0x11];

#[test]
fn h264_after_a_gap_an_idr_without_its_first_slice_is_dropped() {
    let mut d = Depacketizer::new(Codec::H264).unwrap();
    run(&mut d, &[(1, true, IDR)]);
    // The first slice of the next access unit was lost; only its second slice arrives.
    d.lost();
    let (frames, _) = run(&mut d, &[(2, true, IDR_SECOND_SLICE)]);
    assert_eq!(frames, []);
    assert!(d.wants_key_frame());
    // A whole two-slice IDR restores decoding.
    let (frames, _) = run(&mut d, &[(3, false, IDR), (3, true, IDR_SECOND_SLICE)]);
    assert_eq!(frames, [Got { timestamp: 3, key: true, data: annexb(&[IDR, IDR_SECOND_SLICE]) }]);
    assert!(!d.wants_key_frame());

    // A gap inside an access unit drops the rest of it, counted once.
    let mut d = Depacketizer::new(Codec::H264).unwrap();
    run(&mut d, &[(1, true, IDR), (2, false, IDR)]);
    d.lost();
    let (frames, _) = run(&mut d, &[(2, true, IDR)]);
    assert_eq!(frames, []);
    assert!(d.wants_key_frame());
    assert_eq!(d.stats().dropped, 1);
}

#[test]
fn h264_stap_a_refuses_nested_packetization_units() {
    let mut d = Depacketizer::new(Codec::H264).unwrap();
    let nested = [0x78, 0, 1, 0x41];
    let mut stap = vec![0x78];
    for nal in [IDR, &nested[..]] {
        stap.extend_from_slice(&(nal.len() as u16).to_be_bytes());
        stap.extend_from_slice(nal);
    }
    let (frames, errors) = run(&mut d, &[(0, true, &stap)]);
    assert_eq!(frames, []);
    assert_eq!(errors, [Error::Invalid("an STAP-A holds a NAL unit of a packetization type")]);
}

#[test]
fn h264_fu_a_fragments_must_continue_one_nal_unit() {
    let parts = fu_a(&long_idr(), 16);
    let mut d = Depacketizer::new(Codec::H264).unwrap();
    // An end fragment of a non-IDR slice after the start of an IDR slice.
    let end = [0x5C, 0x40 | 1, 9, 9];
    let (frames, errors) = run(&mut d, &[(1, false, &parts[0]), (1, true, &end)]);
    assert_eq!(frames, []);
    assert_eq!(errors, [Error::Invalid("a FU-A fragment does not continue the NAL unit before it")]);
    // A start fragment that carries an aggregation packet.
    let (frames, errors) = run(&mut d, &[(2, false, &[0x7C, 0x80 | 24, 0, 1]), (2, true, &[0x7C, 0x40 | 24, 0x65])]);
    assert_eq!(frames, []);
    assert_eq!(errors, [Error::Invalid("a FU-A fragment carries a NAL unit of a packetization type")]);
}

#[test]
fn h264_an_empty_fu_a_start_fragment_takes_the_first_slice_from_the_next_fragment() {
    // RFC 6184 section 5.8 allows a FU payload of no bytes. The slice header comes in the next
    // fragment.
    let mut d = Depacketizer::new(Codec::H264).unwrap();
    let (frames, errors) = run(&mut d, &[(1, false, &[0x7C, 0x85]), (1, true, &[0x7C, 0x45, 0x88, 0x84, 0x21])]);
    assert_eq!(errors, []);
    assert_eq!(frames, [Got { timestamp: 1, key: true, data: annexb(&[IDR]) }]);
    assert!(!d.wants_key_frame());

    // A first slice whose header, read from the later fragment, does not start at macroblock 0.
    let mut d = Depacketizer::new(Codec::H264).unwrap();
    let (frames, _) = run(&mut d, &[(1, false, &[0x7C, 0x85]), (1, false, &[0x7C, 0x45, 0x40, 0x11]), (1, true, IDR)]);
    assert_eq!(frames, []);
    assert!(d.wants_key_frame());

    // A fragment run with no body byte at all does not start the frame, and the slice after it
    // does not decide for it.
    let mut d = Depacketizer::new(Codec::H264).unwrap();
    let (frames, _) = run(&mut d, &[(1, false, &[0x7C, 0x85]), (1, false, &[0x7C, 0x45]), (1, true, IDR)]);
    assert_eq!(frames, []);
    assert!(d.wants_key_frame());
}

#[test]
fn opus_payloads_over_the_limit_are_dropped() {
    let mut d = Depacketizer::with_max_frame(Codec::Opus, 4).unwrap();
    let (frames, errors) = run(&mut d, &[(0, true, &[1, 2, 3, 4, 5]), (960, true, &[1, 2, 3, 4])]);
    assert_eq!(errors, [Error::TooLarge { size: 5, limit: 4 }]);
    assert_eq!(frames.len(), 1);
    assert_eq!(d.stats().dropped, 1);
}

#[test]
fn aac_units_that_do_not_read_count_as_dropped() {
    let mut d = Depacketizer::new(Codec::Aac(AacConfig::HBR)).unwrap();
    let (_, errors) = run(&mut d, &[(0, true, &aac_hbr(&[&[1, 2]], Some(5)))]);
    assert_eq!(errors.len(), 1);
    assert_eq!((d.stats().errors, d.stats().dropped), (1, 1));
}

#[test]
fn aac_the_last_fragment_has_the_marker_bit() {
    let mut d = Depacketizer::new(Codec::Aac(AacConfig::HBR)).unwrap();
    let a = aac_hbr(&[&[1, 2]], Some(4));
    let b = aac_hbr(&[&[3, 4]], Some(4));
    let (frames, errors) = run(&mut d, &[(0, false, &a), (0, false, &b)]);
    assert_eq!(frames, []);
    assert_eq!(errors, [Error::Invalid("the last AAC fragment does not have the marker bit")]);
    let (frames, errors) = run(&mut d, &[(1024, false, &a), (1024, true, &b)]);
    assert_eq!(errors, []);
    assert_eq!(frames, [Got { timestamp: 1024, key: true, data: vec![1, 2, 3, 4] }]);
}

#[test]
fn aac_complete_units_over_the_limit_are_dropped() {
    let mut d = Depacketizer::with_max_frame(Codec::Aac(AacConfig::HBR), 2).unwrap();
    let (frames, errors) = run(&mut d, &[(0, true, &aac_hbr(&[&[1], &[1, 2, 3]], None))]);
    assert_eq!(frames, []);
    assert_eq!(errors, [Error::TooLarge { size: 3, limit: 2 }]);
}
