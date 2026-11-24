use crate::Error;
use crate::rtcp::*;

fn all(datagram: &[u8]) -> Vec<Result<Rtcp<'_>, Error>> {
    Compound::new(datagram).collect()
}

fn block(ssrc: u32, cumulative_lost: i32) -> ReportBlock {
    ReportBlock {
        ssrc,
        fraction_lost: 0x40,
        cumulative_lost,
        highest_sequence: 0x0001_0005,
        jitter: 77,
        last_sr: 0x1234_5678,
        delay_since_last_sr: 0x8000,
    }
}

#[test]
fn sender_report_bytes_match_rfc_3550_section_6_4_1() {
    let info = SenderInfo { ssrc: 1, ntp: 0x0102_0304_0506_0708, rtp_timestamp: 9, packets: 10, octets: 11 };
    let mut out = Vec::new();
    write_sender_report(&mut out, &info, &[block(2, -1)]).unwrap();
    assert_eq!(out.len(), 52);
    assert_eq!(out[..4], [0x81, 200, 0, 12]);
    assert_eq!(out[4..8], [0, 0, 0, 1]);
    assert_eq!(out[8..16], [1, 2, 3, 4, 5, 6, 7, 8]);
    // fraction lost 0x40, cumulative lost -1 as a signed 24-bit field.
    assert_eq!(out[32..36], [0x40, 0xFF, 0xFF, 0xFF]);
    let items = all(&out);
    let [Ok(Rtcp::SenderReport { info: back, reports })] = items.as_slice() else { panic!("{items:?}") };
    assert_eq!(*back, info);
    assert_eq!(reports.len(), 1);
    assert_eq!((*reports).collect::<Vec<_>>(), [block(2, -1)]);
}

#[test]
fn receiver_report_round_trips_and_clamps_the_loss() {
    let mut out = Vec::new();
    write_receiver_report(&mut out, 5, &[block(6, 1000), block(7, MIN_CUMULATIVE_LOST), block(8, i32::MAX)]).unwrap();
    let items = all(&out);
    let [Ok(Rtcp::ReceiverReport { ssrc: 5, reports })] = items.as_slice() else { panic!("{items:?}") };
    let back: Vec<_> = (*reports).collect();
    assert_eq!(back[0], block(6, 1000));
    assert_eq!(back[1].cumulative_lost, MIN_CUMULATIVE_LOST);
    assert_eq!(back[2].cumulative_lost, MAX_CUMULATIVE_LOST);
    let mut empty = Vec::new();
    write_receiver_report(&mut empty, 5, &[]).unwrap();
    assert_eq!(empty, [0x80, 201, 0, 1, 0, 0, 0, 5]);
    assert!(matches!(write_receiver_report(&mut empty, 5, &[ReportBlock::default(); 32]), Err(Error::BadInput(_))));
}

#[test]
fn nack_groups_lost_packets_into_entries_of_17() {
    let mut out = Vec::new();
    write_nack(&mut out, 1, 2, &[10, 11, 26, 27, 100]).unwrap();
    // Entries (10, bits 0 and 15), (27, none), (100, none).
    assert_eq!(out[..4], [0x81, 205, 0, 5]);
    assert_eq!(out[12..], [0, 10, 0x80, 0x01, 0, 27, 0, 0, 0, 100, 0, 0]);
    let items = all(&out);
    let [Ok(Rtcp::Nack { sender_ssrc: 1, media_ssrc: 2, lost })] = items.as_slice() else { panic!("{items:?}") };
    assert_eq!((*lost).collect::<Vec<_>>(), [10, 11, 26, 27, 100]);

    let mut wrap = Vec::new();
    write_nack(&mut wrap, 1, 2, &[65535, 0, 1]).unwrap();
    assert_eq!(wrap.len(), 16);
    let items = all(&wrap);
    let [Ok(Rtcp::Nack { lost, .. })] = items.as_slice() else { panic!("{items:?}") };
    assert_eq!((*lost).collect::<Vec<_>>(), [65535, 0, 1]);
    assert!(matches!(write_nack(&mut wrap, 1, 2, &[]), Err(Error::BadInput(_))));
}

#[test]
fn picture_loss_indication_and_bye() {
    let mut out = Vec::new();
    write_pli(&mut out, 3, 4).unwrap();
    assert_eq!(out, [0x81, 206, 0, 2, 0, 0, 0, 3, 0, 0, 0, 4]);
    write_bye(&mut out, &[9, 10], Some("gone")).unwrap();
    let items = all(&out);
    assert_eq!(items.len(), 2);
    assert_eq!(items[0], Ok(Rtcp::Pli { sender_ssrc: 3, media_ssrc: 4 }));
    let Ok(bye @ Rtcp::Bye { reason, .. }) = items[1] else { panic!("{items:?}") };
    assert_eq!(reason, Some(&b"gone"[..]));
    assert_eq!(bye.bye_sources().collect::<Vec<_>>(), [9, 10]);
    assert!(out.len() % 4 == 0);
    assert!(matches!(write_bye(&mut out, &[0; 32], None), Err(Error::BadInput(_))));
    assert!(matches!(write_bye(&mut out, &[], Some(&"x".repeat(256))), Err(Error::BadInput(_))));
}

#[test]
fn compound_packets_are_read_in_order_and_other_types_pass_through() {
    let mut out = Vec::new();
    write_receiver_report(&mut out, 1, &[]).unwrap();
    // A source description chunk: SSRC 1, CNAME "a", end.
    out.extend_from_slice(&[0x81, 202, 0, 2, 0, 0, 0, 1, 1, 1, b'a', 0]);
    write_pli(&mut out, 1, 2).unwrap();
    let items = all(&out);
    assert_eq!(items.len(), 3);
    assert!(matches!(items[0], Ok(Rtcp::ReceiverReport { ssrc: 1, .. })));
    assert_eq!(items[1], Ok(Rtcp::Other { packet_type: SDES, count: 1, body: &[0, 0, 0, 1, 1, 1, b'a', 0] }));
    assert!(matches!(items[2], Ok(Rtcp::Pli { .. })));
}

#[test]
fn reading_refuses_broken_packets_and_stops() {
    let mut rr = Vec::new();
    write_receiver_report(&mut rr, 1, &[]).unwrap();
    let mut v1 = rr.clone();
    v1[0] = 0x40;
    assert_eq!(all(&v1), [Err(Error::Invalid("the RTCP version is not 2"))]);
    assert_eq!(all(&rr[..6]), [Err(Error::Truncated("an RTCP packet"))]);
    assert_eq!(all(&rr[..3]), [Err(Error::Truncated("the RTCP header"))]);
    // A report count of 1 with no block.
    let mut short = rr.clone();
    short[0] |= 1;
    assert_eq!(all(&short), [Err(Error::Truncated("the report blocks"))]);
    // Padding on a packet that is not the last.
    let mut padded = rr.clone();
    padded[0] |= 0x20;
    padded.extend_from_slice(&rr);
    let items = all(&padded);
    assert_eq!(items, [Err(Error::Invalid("an RTCP packet before the last one has padding"))]);
    // Padding on the last packet: the count is the last byte.
    let mut last = vec![0xA0, 201, 0, 2, 0, 0, 0, 1, 0, 0, 0, 4];
    assert!(matches!(all(&last)[..], [Ok(Rtcp::ReceiverReport { ssrc: 1, .. })]));
    last[11] = 0;
    assert_eq!(all(&last), [Err(Error::Invalid("the RTCP padding count is 0 or longer than the packet"))]);
    // A generic NACK without entries.
    let nack = [0x81, 205, 0, 2, 0, 0, 0, 1, 0, 0, 0, 2];
    assert_eq!(all(&nack), [Err(Error::Invalid("a generic NACK holds one or more 4-byte entries"))]);
    // A BYE whose reason runs past the packet.
    let bye = [0x81, 203, 0, 2, 0, 0, 0, 1, 9, b'a', b'b', b'c'];
    assert_eq!(all(&bye), [Err(Error::Truncated("the BYE reason"))]);
}

#[test]
fn ntp_times_convert_to_nanoseconds() {
    assert_eq!(ntp_nanos(1 << 32), 1_000_000_000);
    assert_eq!(ntp_nanos((3 << 32) | 0x8000_0000), 3_500_000_000);
    assert_eq!(ntp_middle(0x1122_3344_5566_7788), 0x3344_5566);
}

#[test]
fn time_map_follows_sender_reports() {
    assert!(matches!(TimeMap::new(0), Err(Error::BadInput(_))));
    let mut map = TimeMap::new(90_000).unwrap();
    assert_eq!(map.nanos(0), None);
    assert_eq!(map.sender_report(1000 << 32, 0), Mapping::First);
    assert_eq!(map.nanos(90_000), Some(1001 * 1_000_000_000));
    // A timestamp before the report, across the 32-bit wrap.
    assert_eq!(map.nanos(0u32.wrapping_sub(45_000)), Some(999_500_000_000));
    // The next report, 5 s later on both clocks, with 0.5 s of drift.
    assert_eq!(map.sender_report((1005 << 32) | 0x8000_0000, 450_000), Mapping::Kept);
    assert_eq!(map.nanos(450_000), Some(1_005_500_000_000));
    // The sender's clock was set 1.5 s ahead of where the last report puts it.
    assert_eq!(map.sender_report(1012 << 32, 900_000), Mapping::Reset);
    assert_eq!(map.nanos(900_000), Some(1_012_000_000_000));
    // Before the NTP epoch.
    let mut early = TimeMap::new(1000).unwrap();
    early.sender_report(0, 5000);
    assert_eq!(early.nanos(4000), None);
}
