//! Random packets written and read back, random streams through the jitter buffer and the
//! depacketizers, and random bytes fed to every reader.

use std::collections::BTreeSet;
use std::time::Duration;

use craft_rtp::rtcp::{self, Compound, ReportBlock, Rtcp, SenderInfo};
use craft_rtp::{AacConfig, Codec, Config, Depacketizer, Event, Extension, JitterBuffer, Packet, Push};
use proptest::prelude::*;

fn report_block() -> impl Strategy<Value = ReportBlock> {
    (any::<u32>(), any::<u8>(), rtcp::MIN_CUMULATIVE_LOST..=rtcp::MAX_CUMULATIVE_LOST, any::<[u32; 4]>()).prop_map(
        |(ssrc, fraction_lost, cumulative_lost, [highest_sequence, jitter, last_sr, delay_since_last_sr])| ReportBlock {
            ssrc,
            fraction_lost,
            cumulative_lost,
            highest_sequence,
            jitter,
            last_sr,
            delay_since_last_sr,
        },
    )
}

/// RTP packets of `nals` (one access unit) as RFC 6184 packetization mode 1 sends them: small NAL
/// units gathered into STAP-A packets when `aggregate` is set, NAL units over `mtu` bytes split
/// into FU-A fragments, and the marker bit on the last packet.
fn packetize(nals: &[Vec<u8>], mtu: usize, aggregate: bool) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = Vec::new();
    let mut stap: Vec<u8> = Vec::new();
    let flush = |stap: &mut Vec<u8>, out: &mut Vec<Vec<u8>>| {
        if !stap.is_empty() {
            out.push(std::mem::take(stap));
        }
    };
    for nal in nals {
        if nal.len() > mtu {
            flush(&mut stap, &mut out);
            let indicator = (nal[0] & 0xE0) | 28;
            let chunks: Vec<&[u8]> = nal[1..].chunks(mtu - 2).collect();
            for (i, c) in chunks.iter().enumerate() {
                let s = if i == 0 { 0x80 } else { 0 };
                let e = if i + 1 == chunks.len() { 0x40 } else { 0 };
                out.push([&[indicator, s | e | (nal[0] & 0x1F)][..], c].concat());
            }
        } else if aggregate {
            if stap.len() + 2 + nal.len() > mtu {
                flush(&mut stap, &mut out);
            }
            if stap.is_empty() {
                stap.push(24);
            }
            stap.extend_from_slice(&(nal.len() as u16).to_be_bytes());
            stap.extend_from_slice(nal);
        } else {
            out.push(nal.clone());
        }
    }
    flush(&mut stap, &mut out);
    out
}

/// A NAL unit of `kind` (1 to 23) with a payload of `len` random bytes.
fn nal() -> impl Strategy<Value = Vec<u8>> {
    (1u8..=23, 0u8..4, prop::collection::vec(any::<u8>(), 1..300)).prop_map(|(kind, nri, body)| {
        let mut n = vec![(nri << 5) | kind];
        n.extend(body);
        n
    })
}

proptest! {
    #[test]
    fn rtp_packets_round_trip(
        marker in any::<bool>(),
        payload_type in 0u8..=127,
        sequence in any::<u16>(),
        timestamp in any::<u32>(),
        ssrc in any::<u32>(),
        csrcs in prop::collection::vec(any::<u32>(), 0..=15),
        extension in prop::option::of((any::<u16>(), prop::collection::vec(any::<u8>(), 0..32))),
        payload in prop::collection::vec(any::<u8>(), 0..200),
        padding in any::<u8>(),
    ) {
        let csrc: Vec<u8> = csrcs.iter().flat_map(|c| c.to_be_bytes()).collect();
        let ext = extension.map(|(profile, mut data)| {
            data.truncate(data.len() / 4 * 4);
            (profile, data)
        });
        let p = Packet {
            marker,
            payload_type,
            sequence,
            timestamp,
            ssrc,
            csrc: &csrc,
            extension: ext.as_ref().map(|(profile, data)| Extension { profile: *profile, data }),
            payload: &payload,
            padding,
        };
        let mut out = Vec::new();
        p.write(&mut out).unwrap();
        prop_assert_eq!(out.len(), p.len());
        let back = Packet::parse(&out).unwrap();
        prop_assert_eq!(back, p);
        prop_assert_eq!(back.csrcs().collect::<Vec<_>>(), csrcs);
    }

    #[test]
    fn rtcp_reports_round_trip(
        ssrc in any::<u32>(),
        info in (any::<u32>(), any::<u64>(), any::<u32>(), any::<u32>(), any::<u32>()),
        blocks in prop::collection::vec(report_block(), 0..=31),
        sender in any::<bool>(),
    ) {
        let mut out = Vec::new();
        let info = SenderInfo { ssrc: info.0, ntp: info.1, rtp_timestamp: info.2, packets: info.3, octets: info.4 };
        if sender {
            rtcp::write_sender_report(&mut out, &info, &blocks).unwrap();
        } else {
            rtcp::write_receiver_report(&mut out, ssrc, &blocks).unwrap();
        }
        rtcp::write_pli(&mut out, ssrc, info.ssrc).unwrap();
        let items: Vec<_> = Compound::new(&out).collect::<Result<_, _>>().unwrap();
        prop_assert_eq!(items.len(), 2);
        match items[0] {
            Rtcp::SenderReport { info: back, reports } => {
                prop_assert!(sender);
                prop_assert_eq!(back, info);
                prop_assert_eq!(reports.collect::<Vec<_>>(), blocks);
            }
            Rtcp::ReceiverReport { ssrc: back, reports } => {
                prop_assert!(!sender);
                prop_assert_eq!(back, ssrc);
                prop_assert_eq!(reports.collect::<Vec<_>>(), blocks);
            }
            other => prop_assert!(false, "{other:?}"),
        }
        prop_assert_eq!(items[1], Rtcp::Pli { sender_ssrc: ssrc, media_ssrc: info.ssrc });
    }

    #[test]
    fn nacks_round_trip(lost in prop::collection::btree_set(any::<u16>(), 1..300)) {
        let lost: Vec<u16> = lost.into_iter().collect();
        let mut out = Vec::new();
        rtcp::write_nack(&mut out, 1, 2, &lost).unwrap();
        let items: Vec<_> = Compound::new(&out).collect::<Result<_, _>>().unwrap();
        let [Rtcp::Nack { lost: back, .. }] = items.as_slice() else { panic!("{items:?}") };
        prop_assert_eq!((*back).collect::<Vec<_>>(), lost);
    }

    #[test]
    fn the_jitter_buffer_releases_every_packet_once_in_order(
        start in any::<u16>(),
        order in prop::collection::vec((0u16..90, 0u64..50), 1..300),
    ) {
        // Packets 0 to 89 after `start`, in random order with repeats and holes, arriving up to
        // 50 ms apart. They stay within the 100 a packet may be reordered by, so none is stray.
        let mut jb = JitterBuffer::new(Config::new(90_000)).unwrap();
        let mut now = Duration::ZERO;
        let mut pushed = BTreeSet::new();
        let mut released = Vec::new();
        let mut gaps = 0u64;
        for (offset, wait) in order {
            now += Duration::from_millis(wait);
            while let Some(e) = jb.pop(now) {
                match e {
                    Event::Packet(r) => released.push(r.sequence.wrapping_sub(start)),
                    Event::Gap { missing } => gaps += missing,
                }
            }
            let seq = start.wrapping_add(offset);
            if jb.push(&Packet::new(96, seq, 0, 1, false, &[offset as u8]), now) == Push::Stored {
                prop_assert!(pushed.insert(offset), "{offset} stored twice");
            }
        }
        now += Duration::from_secs(1);
        while let Some(e) = jb.pop(now) {
            match e {
                Event::Packet(r) => {
                    prop_assert_eq!(r.payload, &[r.sequence.wrapping_sub(start) as u8][..]);
                    released.push(r.sequence.wrapping_sub(start));
                }
                Event::Gap { missing } => gaps += missing,
            }
        }
        prop_assert_eq!(jb.held(), 0);
        // In sequence order, each stored packet once (the first packet sets the start, so earlier
        // ones were late).
        prop_assert!(released.windows(2).all(|w| w[0] < w[1]), "{released:?}");
        prop_assert_eq!(released.iter().copied().collect::<BTreeSet<_>>(), pushed);
        let stats = jb.stats();
        prop_assert_eq!(stats.released, released.len() as u64);
        prop_assert!(gaps >= stats.missing);
    }

    #[test]
    fn h264_access_units_survive_packetization(
        units in prop::collection::vec(prop::collection::vec(nal(), 1..6), 1..6),
        mtu in 16usize..400,
        aggregate in any::<bool>(),
    ) {
        let mut units = units;
        // The first access unit holds an IDR picture whose first slice starts at macroblock 0, so
        // video starts with it.
        units[0][0][0] = (units[0][0][0] & 0xE0) | 5;
        units[0][0][1] |= 0x80;
        let mut d = Depacketizer::new(Codec::H264).unwrap();
        let mut got: Vec<Vec<u8>> = Vec::new();
        for (i, nals) in units.iter().enumerate() {
            let packets = packetize(nals, mtu, aggregate);
            for (k, p) in packets.iter().enumerate() {
                d.push(i as u32 * 3000, k + 1 == packets.len(), p, |f| got.push(f.data.to_vec())).unwrap();
            }
        }
        let want: Vec<Vec<u8>> = units.iter().map(|nals| nals.iter().flat_map(|n| [&[0, 0, 0, 1][..], n].concat()).collect()).collect();
        prop_assert_eq!(got, want);
    }

    #[test]
    fn random_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..400), marker in any::<bool>()) {
        let _ = Packet::parse(&bytes);
        for item in Compound::new(&bytes) {
            if let Ok(Rtcp::Nack { lost, .. }) = item {
                prop_assert!(lost.count() <= bytes.len() * 17);
            }
        }
        for codec in [Codec::H264, Codec::Vp8, Codec::Opus, Codec::Aac(AacConfig::HBR), Codec::Aac(AacConfig::LBR)] {
            let mut d = Depacketizer::with_max_frame(codec, 4096).unwrap();
            for chunk in bytes.chunks(37) {
                let _ = d.push(7, marker, chunk, |f| assert!(f.data.len() <= 4096));
            }
            d.lost();
        }
        let mut jb = JitterBuffer::new(Config::new(48_000)).unwrap();
        for (i, chunk) in bytes.chunks(14).enumerate() {
            if let Ok(p) = Packet::parse(chunk) {
                jb.push(&p, Duration::from_millis(i as u64));
            }
            let mut lost = Vec::new();
            jb.nacks(&mut lost, 64);
            while jb.pop(Duration::from_millis(i as u64)).is_some() {}
        }
    }
}
