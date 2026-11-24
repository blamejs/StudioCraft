use crate::{Error, Extension, Packet, is_rtcp};

#[test]
fn header_bytes_match_rfc_3550_section_5_1() {
    let mut out = Vec::new();
    Packet::new(96, 0x1234, 0xDEAD_BEEF, 0x0102_0304, true, &[9, 8]).write(&mut out).unwrap();
    assert_eq!(out, [0x80, 0xE0, 0x12, 0x34, 0xDE, 0xAD, 0xBE, 0xEF, 1, 2, 3, 4, 9, 8]);
    let p = Packet::parse(&out).unwrap();
    assert!(p.marker);
    assert_eq!((p.payload_type, p.sequence, p.timestamp, p.ssrc), (96, 0x1234, 0xDEAD_BEEF, 0x0102_0304));
    assert_eq!(p.payload, [9, 8]);
    assert_eq!(p.len(), out.len());
}

#[test]
fn contributing_sources_extension_and_padding_round_trip() {
    let csrc = [0, 0, 0, 7, 0, 0, 0, 9];
    let ext = [1, 2, 3, 4, 5, 6, 7, 8];
    let p = Packet {
        marker: false,
        payload_type: 111,
        sequence: 65535,
        timestamp: 0,
        ssrc: 42,
        csrc: &csrc,
        extension: Some(Extension { profile: 0xBEDE, data: &ext }),
        payload: &[1, 2, 3],
        padding: 5,
    };
    let mut out = Vec::new();
    p.write(&mut out).unwrap();
    assert_eq!(out[0], 0x80 | 0x20 | 0x10 | 2);
    assert_eq!(&out[out.len() - 5..], [0, 0, 0, 0, 5]);
    assert_eq!(out.len(), p.len());
    let back = Packet::parse(&out).unwrap();
    assert_eq!(back, p);
    assert_eq!(back.csrcs().collect::<Vec<_>>(), [7, 9]);
}

#[test]
fn reading_refuses_broken_packets() {
    let mut good = Vec::new();
    Packet::new(96, 1, 2, 3, false, &[1, 2, 3, 4]).write(&mut good).unwrap();
    assert_eq!(Packet::parse(&good[..11]), Err(Error::Truncated("the RTP header")));
    assert_eq!(Packet::parse(&[]), Err(Error::Truncated("the RTP header")));
    let mut v1 = good.clone();
    v1[0] = 0x40;
    assert_eq!(Packet::parse(&v1), Err(Error::Invalid("the RTP version is not 2")));
    let mut csrc = good.clone();
    csrc[0] |= 2;
    assert_eq!(Packet::parse(&csrc), Err(Error::Truncated("the contributing source list")));
    let mut ext = good.clone();
    ext[0] |= 0x10;
    // The four payload bytes read as an extension header of 0x0304 words.
    assert_eq!(Packet::parse(&ext), Err(Error::Truncated("the header extension")));
    let mut pad0 = good.clone();
    pad0[0] |= 0x20;
    *pad0.last_mut().unwrap() = 0;
    assert_eq!(Packet::parse(&pad0), Err(Error::Invalid("the padding count is 0")));
    let mut pad_long = good.clone();
    pad_long[0] |= 0x20;
    *pad_long.last_mut().unwrap() = 5;
    assert_eq!(Packet::parse(&pad_long), Err(Error::Invalid("the padding is longer than the bytes after the header")));
    // Padding that covers every byte after the header leaves an empty payload.
    let mut pad_all = good.clone();
    pad_all[0] |= 0x20;
    *pad_all.last_mut().unwrap() = 4;
    assert_eq!(Packet::parse(&pad_all).unwrap().payload, []);
}

#[test]
fn writing_refuses_bad_fields_and_leaves_the_output() {
    let mut out = vec![7];
    let mut p = Packet::new(128, 1, 2, 3, false, &[]);
    assert!(matches!(p.write(&mut out), Err(Error::BadInput(_))));
    p.payload_type = 0;
    p.csrc = &[1, 2, 3];
    assert!(matches!(p.write(&mut out), Err(Error::BadInput(_))));
    let sixteen = [0u8; 64];
    p.csrc = &sixteen;
    assert!(matches!(p.write(&mut out), Err(Error::BadInput(_))));
    p.csrc = &[];
    p.extension = Some(Extension { profile: 1, data: &[1, 2] });
    assert!(matches!(p.write(&mut out), Err(Error::BadInput(_))));
    assert_eq!(out, [7]);
}

#[test]
fn rtcp_is_told_from_rtp_on_a_shared_port() {
    assert!(is_rtcp(&[0x80, 200]));
    assert!(is_rtcp(&[0x81, 206]));
    assert!(is_rtcp(&[0x80, 223]));
    assert!(!is_rtcp(&[0x80, 96]));
    assert!(!is_rtcp(&[0x80, 0xE0]));
    assert!(!is_rtcp(&[0x80]));
}
