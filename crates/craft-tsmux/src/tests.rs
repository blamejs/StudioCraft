use super::*;

/// One packet's header fields.
#[derive(Debug, Clone, Copy)]
struct Pk {
    pid: u16,
    pusi: bool,
    cc: u8,
    payload: bool,
    rai: bool,
    pcr: Option<i64>,
}

fn walk(ts: &[u8]) -> Vec<Pk> {
    assert_eq!(ts.len() % PACKET_SIZE, 0);
    ts.chunks(PACKET_SIZE)
        .map(|p| {
            assert_eq!(p[0], 0x47);
            let afc = p[3] >> 4 & 3;
            let (mut rai, mut pcr) = (false, None);
            if afc & 2 != 0 {
                let len = p[4] as usize;
                assert!(len <= 183);
                if afc == 2 {
                    assert_eq!(len, 183);
                }
                if len > 0 {
                    rai = p[5] & 0x40 != 0;
                    if p[5] & 0x10 != 0 {
                        let b = &p[6..12];
                        let base = (b[0] as i64) << 25 | (b[1] as i64) << 17 | (b[2] as i64) << 9 | (b[3] as i64) << 1 | (b[4] >> 7) as i64;
                        pcr = Some(base * 300 + (((b[4] & 1) as i64) << 8 | b[5] as i64));
                    }
                }
            }
            Pk { pid: ((p[1] & 0x1F) as u16) << 8 | p[2] as u16, pusi: p[1] & 0x40 != 0, cc: p[3] & 0x0F, payload: afc & 1 != 0, rai, pcr }
        })
        .collect()
}

fn h264(key: bool) -> Vec<u8> {
    if key {
        vec![0, 0, 0, 1, 0x67, 0x42, 0, 0x1E, 0, 0, 0, 1, 0x68, 0xCE, 0, 0, 1, 0x65, 0x88, 0x84, 0x21]
    } else {
        vec![0, 0, 0, 1, 0x41, 0x9A, 0x11]
    }
}

fn mux(cfg: Config, streams: &[Stream], f: impl FnOnce(&mut Muxer<Vec<u8>>)) -> Vec<u8> {
    let mut m = Muxer::new(Vec::new(), cfg, streams).unwrap();
    f(&mut m);
    m.finish().unwrap()
}

#[test]
fn starts_with_pat_pmt_and_a_pcr_on_the_first_frame() {
    let ts = mux(Config::default(), &[Stream::new(0x100, Codec::H264)], |m| {
        m.write_frame(0, &Frame { data: &h264(true), pts: 90_000, dts: None, key: true }).unwrap();
    });
    let pk = walk(&ts);
    assert_eq!(pk.len(), 3);
    assert_eq!((pk[0].pid, pk[0].pusi), (0, true));
    assert_eq!((pk[1].pid, pk[1].pusi), (0x1000, true));
    assert_eq!((pk[2].pid, pk[2].pusi, pk[2].rai), (0x100, true, true));
    assert_eq!(pk[2].pcr, Some((90_000 - 63_000) * 300));
    // the payload ends the packet: the access unit delimiter, then the frame
    let frame = h264(true);
    let p = &ts[2 * PACKET_SIZE..];
    assert_eq!(&p[PACKET_SIZE - frame.len()..PACKET_SIZE], &frame[..]);
    assert_eq!(&p[PACKET_SIZE - frame.len() - 6 - 14..PACKET_SIZE - frame.len() - 6][..4], &[0, 0, 1, 0xE0]);
    assert_eq!(&p[PACKET_SIZE - frame.len() - 6..PACKET_SIZE - frame.len()], &[0, 0, 0, 1, 9, 0xF0]);
}

#[test]
fn every_stuffing_length_fills_the_packet() {
    // frames whose PES packet ends at every offset of a packet, with and without a PCR
    for n in 1..=400usize {
        let ts = mux(Config { pcr_pid: Some(0x200), ..Config::default() }, &[Stream::new(0x100, Codec::MpegAudio { low_sample_rate: false })], |m| {
            let mut data = vec![0u8; n];
            data[0] = 0xFF;
            if n > 1 {
                data[1] = 0xFB;
            }
            if n == 1 {
                assert!(matches!(m.write_frame(0, &Frame { data: &data, pts: 90_000, dts: None, key: true }), Err(Error::BadFrame(_))));
                return;
            }
            m.write_frame(0, &Frame { data: &data, pts: 90_000, dts: None, key: true }).unwrap();
            m.write_frame(0, &Frame { data: &data, pts: 92_000, dts: None, key: false }).unwrap();
        });
        if n == 1 {
            continue;
        }
        let pk = walk(&ts);
        let mut payload = 0;
        for (p, raw) in pk.iter().zip(ts.chunks(PACKET_SIZE)) {
            if p.pid == 0x100 {
                let start = if raw[3] & 0x20 != 0 { 5 + raw[4] as usize } else { 4 };
                payload += PACKET_SIZE - start;
            }
        }
        assert_eq!(payload, 2 * (14 + n), "{n}");
    }
}

#[test]
fn continuity_counters_count_payload_packets_per_pid() {
    let ts = mux(Config::default(), &[Stream::new(0x100, Codec::H264), Stream::new(0x101, Codec::Ac3)], |m| {
        let big = {
            let mut v = h264(true);
            v.resize(10_000, 0x55);
            v
        };
        for k in 0..40i64 {
            m.write_frame(0, &Frame { data: &big, pts: 90_000 + k * 3000, dts: None, key: k % 10 == 0 }).unwrap();
            m.write_frame(1, &Frame { data: &[0x0B, 0x77, 1, 2, 3], pts: 90_000 + k * 3000, dts: None, key: true }).unwrap();
        }
    });
    let mut last: std::collections::HashMap<u16, u8> = Default::default();
    for p in walk(&ts) {
        if let Some(&c) = last.get(&p.pid) {
            let want = if p.payload { (c + 1) & 0x0F } else { c };
            assert_eq!(p.cc, want, "PID {:#x}", p.pid);
        }
        last.insert(p.pid, p.cc);
    }
}

#[test]
fn pcr_never_lapses_past_the_interval() {
    for interval in [900u32, 3600, MAX_PCR_INTERVAL] {
        // video on 0x100 carries the PCR; a 2 s gap in the video is filled from the audio clock
        let ts =
            mux(Config { pcr_interval: interval, ..Config::default() }, &[Stream::new(0x100, Codec::H264), Stream::new(0x101, Codec::Ac3)], |m| {
                let mut t = 90_000i64;
                for k in 0..200i64 {
                    if !(50..110).contains(&k) {
                        m.write_frame(0, &Frame { data: &h264(k == 0), pts: t, dts: None, key: k == 0 }).unwrap();
                    }
                    m.write_frame(1, &Frame { data: &[0x0B, 0x77, 0], pts: t, dts: None, key: true }).unwrap();
                    t += 3000;
                }
            });
        let pcrs: Vec<i64> = walk(&ts).iter().filter(|p| p.pid == 0x100).filter_map(|p| p.pcr).collect();
        assert!(pcrs.len() > 1);
        for w in pcrs.windows(2) {
            assert!(w[1] > w[0], "PCR goes forward");
            assert!(w[1] - w[0] <= i64::from(interval) * 300, "interval {interval}: {} apart", w[1] - w[0]);
        }
    }
}

#[test]
fn pcr_on_a_pid_of_its_own_and_through_advance() {
    let mut m = Muxer::new(Vec::new(), Config { pcr_pid: Some(0x1FF), ..Config::default() }, &[Stream::new(0x101, Codec::Ac3)]).unwrap();
    assert_eq!(m.pcr_pid(), 0x1FF);
    m.advance(63_000).unwrap();
    m.advance(63_000 + 90_000).unwrap();
    m.write_frame(0, &Frame { data: &[0x0B, 0x77], pts: 63_000 + 90_000, dts: None, key: true }).unwrap();
    let st = m.stats();
    let ts = m.finish().unwrap();
    let pk = walk(&ts);
    let pcrs: Vec<i64> = pk.iter().filter(|p| p.pid == 0x1FF).map(|p| p.pcr.unwrap()).collect();
    assert_eq!(pcrs.first(), Some(&0));
    assert_eq!(pcrs.last(), Some(&(90_000 * 300)));
    assert!(pcrs.windows(2).all(|w| w[1] - w[0] <= 3600 * 300));
    assert!(pk.iter().filter(|p| p.pid == 0x1FF).all(|p| !p.payload && p.cc == 0));
    assert_eq!(st.pcrs as usize, pcrs.len());
    assert_eq!(pcrs.len(), 26, "one PCR, 24 to fill the second, one at its end");
    // at the start, then every 100 ms of the gap, the last one at its end
    assert_eq!(st.psi, 11);
    let mut last_psi = None;
    let mut pcr = 0;
    for p in &pk {
        if let Some(v) = p.pcr {
            pcr = v;
        }
        if p.pid == 0 {
            if let Some(l) = last_psi {
                assert!(pcr - l <= (9000 + 3600) * 300, "PAT {} after the last", pcr - l);
            }
            last_psi = Some(pcr);
        }
    }
    assert_eq!(st.bytes(), ts.len() as u64);
}

#[test]
fn a_pcr_packet_before_the_first_payload_keeps_the_counter_continuous() {
    // the audio frame comes first, so the video PID carries a PCR-only packet before its first
    // payload packet
    let ts = mux(Config::default(), &[Stream::new(0x100, Codec::H264), Stream::new(0x101, Codec::Ac3)], |m| {
        m.write_frame(1, &Frame { data: &[0x0B, 0x77], pts: 90_000, dts: None, key: true }).unwrap();
        m.write_frame(0, &Frame { data: &h264(true), pts: 90_000, dts: None, key: true }).unwrap();
    });
    let video: Vec<Pk> = walk(&ts).into_iter().filter(|p| p.pid == 0x100).collect();
    assert_eq!(video.len(), 2);
    assert!(!video[0].payload && video[0].pcr.is_some());
    assert!(video[1].payload);
    assert_eq!(video[1].cc, (video[0].cc + 1) & 0x0F);
}

#[test]
fn a_long_gap_is_closed_by_one_pcr() {
    let mut m = Muxer::new(Vec::new(), Config { pcr_interval: 1, ..Config::default() }, &[Stream::new(0x100, Codec::Ac3)]).unwrap();
    m.write_frame(0, &Frame { data: &[0x0B, 0x77], pts: 63_000, dts: None, key: true }).unwrap();
    m.write_frame(0, &Frame { data: &[0x0B, 0x77], pts: 63_000 + 90_000, dts: None, key: true }).unwrap();
    assert_eq!(m.stats().pcr_gaps, 1);
    let pcrs: Vec<i64> = walk(&m.finish().unwrap()).iter().filter_map(|p| p.pcr).collect();
    assert_eq!(pcrs.len() as u32, 2 + MAX_PCR_FILL);
    assert_eq!(pcrs.last(), Some(&(90_000 * 300)));
}

#[test]
fn psi_repeats_on_keyframes_and_new_writers() {
    let mut m = Muxer::new(Vec::new(), Config::default(), &[Stream::new(0x100, Codec::H264)]).unwrap();
    m.write_frame(0, &Frame { data: &h264(true), pts: 90_000, dts: None, key: true }).unwrap();
    m.write_frame(0, &Frame { data: &h264(false), pts: 93_000, dts: None, key: false }).unwrap();
    assert_eq!(m.stats().psi, 1);
    m.write_frame(0, &Frame { data: &h264(true), pts: 96_000, dts: None, key: true }).unwrap();
    assert_eq!(m.stats().psi, 2);
    let first = m.replace_writer(Vec::new());
    m.write_frame(0, &Frame { data: &h264(false), pts: 99_000, dts: None, key: false }).unwrap();
    let second = m.finish().unwrap();
    let pk = walk(&second);
    assert_eq!((pk[0].pid, pk[1].pid), (0, 0x1000), "a new segment starts with the PAT and PMT");
    let pk_first = walk(&first);
    let last_video = pk_first.iter().rev().find(|p| p.pid == 0x100).unwrap().cc;
    assert_eq!(pk.iter().find(|p| p.pid == 0x100).unwrap().cc, (last_video + 1) & 0x0F, "counters carry on");
}

#[test]
fn every_new_segment_carries_a_pcr() {
    for pcr_pid in [None, Some(0x1F0)] {
        let mut m = Muxer::new(Vec::new(), Config { pcr_pid, ..Config::default() }, &[Stream::new(0x100, Codec::H264)]).unwrap();
        m.write_frame(0, &Frame { data: &h264(true), pts: 90_000, dts: None, key: true }).unwrap();
        m.replace_writer(Vec::new());
        // the same clock and a frame within the PCR interval
        m.write_frame(0, &Frame { data: &h264(false), pts: 90_000, dts: None, key: false }).unwrap();
        let segment = m.replace_writer(Vec::new());
        m.advance(91_000).unwrap();
        let third = m.finish().unwrap();
        let want = pcr_pid.unwrap_or(0x100);
        for (n, ts) in [segment, third].iter().enumerate() {
            let pk = walk(ts);
            assert_eq!((pk[0].pid, pk[1].pid), (0, 0x1000), "{pcr_pid:?} segment {n}");
            assert!(pk.iter().any(|p| p.pid == want && p.pcr.is_some()), "{pcr_pid:?} segment {n} has a PCR");
        }
    }
}

#[test]
fn a_segment_after_an_idle_gap_starts_with_the_tables() {
    let mut m = Muxer::new(Vec::new(), Config::default(), &[Stream::new(0x100, Codec::H264)]).unwrap();
    m.write_frame(0, &Frame { data: &h264(true), pts: 90_000, dts: None, key: true }).unwrap();
    m.replace_writer(Vec::new());
    m.advance(180_000).unwrap();
    let pk = walk(&m.finish().unwrap());
    assert_eq!((pk[0].pid, pk[1].pid), (0, 0x1000));
    assert!(pk.len() > 10, "the gap is filled with PCRs");
}

#[test]
fn short_table_intervals_hold_in_idle_gaps() {
    let mut m = Muxer::new(Vec::new(), Config { psi_interval: 900, ..Config::default() }, &[Stream::new(0x100, Codec::Ac3)]).unwrap();
    m.advance(63_000).unwrap();
    m.advance(63_000 + 90_000).unwrap();
    // every 900 ticks: at the start, 99 times inside the second, and at its end
    assert_eq!(m.stats().psi, 101);
    // a table interval of one tick fills at most MAX_PCR_FILL repeats per gap, and the PCRs still
    // fill the gap
    let mut m = Muxer::new(Vec::new(), Config { psi_interval: 1, ..Config::default() }, &[Stream::new(0x100, Codec::Ac3)]).unwrap();
    m.advance(63_000).unwrap();
    m.advance(63_000 + 90_000).unwrap();
    assert_eq!(m.stats().psi, 2 + u64::from(MAX_PCR_FILL));
    assert_eq!((m.stats().pcrs, m.stats().pcr_gaps), (26, 0));
}

#[test]
fn frames_behind_the_sent_clock_are_refused() {
    let mut m = Muxer::new(Vec::new(), Config::default(), &[Stream::new(0x100, Codec::H264), Stream::new(0x101, Codec::Ac3)]).unwrap();
    m.write_frame(0, &Frame { data: &h264(true), pts: 900_000, dts: None, key: true }).unwrap();
    // the clock is 900 000 - 63 000: audio up to the PCR delay behind the video is fine
    m.write_frame(1, &Frame { data: &[0x0B, 0x77], pts: 900_000 - 63_000, dts: None, key: true }).unwrap();
    let before = m.get_ref().len();
    assert!(matches!(m.write_frame(1, &Frame { data: &[0x0B, 0x77], pts: 900_000 - 63_001, dts: None, key: true }), Err(Error::Timestamp(_))));
    m.advance(2_000_000).unwrap();
    let after = m.get_ref().len();
    assert!(after > before);
    assert!(matches!(m.write_frame(0, &Frame { data: &h264(false), pts: 1_000_000, dts: None, key: false }), Err(Error::Timestamp(_))));
    assert_eq!(m.get_ref().len(), after, "nothing is written for a refused frame");
}

#[test]
fn timestamps_and_dts() {
    let ts = mux(Config::default(), &[Stream::new(0x100, Codec::H264)], |m| {
        m.write_frame(0, &Frame { data: &h264(true), pts: (1 << 33) + 6000, dts: Some((1 << 33) + 3000), key: true }).unwrap();
    });
    let p = &ts[2 * PACKET_SIZE..];
    let start = 5 + p[4] as usize;
    let h = &p[start..];
    assert_eq!(&h[..9], &[0, 0, 1, 0xE0, 0, (3 + 10 + 6 + h264(true).len()) as u8, 0x84, 0xC0, 10]);
    assert_eq!(h[9] >> 4, 3);
    assert_eq!(h[14] >> 4, 1);
}

#[test]
fn bad_frames_and_timestamps_write_nothing() {
    let mut m = Muxer::new(Vec::new(), Config::default(), &[Stream::new(0x100, Codec::H264), Stream::new(0x101, Codec::AacAdts)]).unwrap();
    fn f(data: &[u8], pts: i64, dts: Option<i64>) -> Frame<'_> {
        Frame { data, pts, dts, key: true }
    }
    assert!(matches!(m.write_frame(2, &f(&h264(true), 90_000, None)), Err(Error::NoSuchStream(2))));
    assert!(matches!(m.write_frame(0, &f(&[], 90_000, None)), Err(Error::EmptyFrame)));
    assert!(matches!(m.write_frame(0, &f(&h264(true), 90_000, Some(90_001))), Err(Error::Timestamp(_))));
    assert!(matches!(m.write_frame(0, &f(&[0x65, 0x88], 90_000, None)), Err(Error::BadFrame(_))));
    assert!(matches!(m.write_frame(1, &f(&[0xFF, 0xF1, 0, 0, 0, 0], 90_000, None)), Err(Error::BadFrame(_))));
    let mut long_adts = vec![0xFF, 0xF1, 0, 0, 0, 0, 0];
    long_adts.resize(70_000, 0);
    assert!(matches!(m.write_frame(1, &f(&long_adts, 90_000, None)), Err(Error::FrameTooLarge { .. })));
    assert!(matches!(m.write_frame(0, &f(&h264(true), i64::MIN, None)), Err(Error::Timestamp(_))));
    assert!(m.get_ref().is_empty());
    m.write_frame(0, &f(&h264(true), 90_000, None)).unwrap();
    assert!(matches!(m.write_frame(0, &f(&h264(true), 80_000, None)), Err(Error::Timestamp(_))));
    // video has no PES length limit
    let mut big = h264(true);
    big.resize(200_000, 1);
    m.write_frame(0, &f(&big, 93_000, None)).unwrap();
}

#[test]
fn settings_are_checked() {
    let one = [Stream::new(0x100, Codec::H264)];
    let bad = |cfg: Config, streams: &[Stream]| matches!(Muxer::new(Vec::new(), cfg, streams), Err(Error::Config(_)));
    let d = Config::default();
    assert!(bad(d, &[]));
    assert!(bad(d, &[Stream::new(0x100, Codec::H264); MAX_STREAMS + 1]));
    assert!(bad(Config { program_number: 0, ..d }, &one));
    assert!(bad(Config { pmt_pid: 0x0F, ..d }, &one));
    assert!(bad(Config { pmt_pid: 0x1FFF, ..d }, &one));
    assert!(bad(Config { pcr_interval: 0, ..d }, &one));
    assert!(bad(Config { pcr_interval: MAX_PCR_INTERVAL + 1, ..d }, &one));
    assert!(bad(Config { psi_interval: 0, ..d }, &one));
    assert!(bad(Config { pcr_pid: Some(0x1000), ..d }, &one));
    assert!(bad(Config { pcr_pid: Some(0x2000), ..d }, &one));
    assert!(bad(d, &[Stream::new(0x1000, Codec::H264)]));
    assert!(bad(d, &[Stream::new(0x100, Codec::H264), Stream::new(0x100, Codec::Ac3)]));
    assert!(bad(d, &[Stream::new(0x100, Codec::Opus { channels: 0 })]));
    assert!(bad(d, &[Stream::new(0x100, Codec::Opus { channels: 9 })]));
    assert!(bad(d, &[Stream::new(0x100, Codec::Aac { object_type: 0, sample_rate_index: 3, channels: 2 })]));
    assert!(bad(d, &[Stream::new(0x100, Codec::Aac { object_type: 2, sample_rate_index: 13, channels: 2 })]));
    assert!(bad(d, &[Stream::new(0x100, Codec::Aac { object_type: 2, sample_rate_index: 3, channels: 0 })]));
    assert!(bad(d, &[Stream::new(0x100, Codec::Ac3).with_language(*b"e1g")]));
    assert!(bad(d, &[Stream::new(0x100, Codec::Ac3).with_language(*b"ENG")]));
    assert!(!bad(d, &[Stream::new(0x100, Codec::Ac3).with_language(*b"eng")]));
    assert!(!bad(Config { pcr_pid: Some(0x1FFE), ..d }, &one));
    // the first video stream carries the PCR by default
    let m = Muxer::new(Vec::new(), d, &[Stream::new(0x101, Codec::Ac3), Stream::new(0x100, Codec::H264)]).unwrap();
    assert_eq!(m.pcr_pid(), 0x100);
}

#[test]
fn wide_programs_split_the_pmt_across_packets() {
    let streams: Vec<Stream> = (0..MAX_STREAMS as u16).map(|i| Stream::new(0x100 + i, Codec::Opus { channels: 2 }).with_language(*b"eng")).collect();
    let ts = mux(Config::default(), &streams, |m| {
        m.write_frame(0, &Frame { data: &[0xFC, 1], pts: 90_000, dts: None, key: true }).unwrap();
    });
    let pk = walk(&ts);
    let pmt: Vec<_> = pk.iter().filter(|p| p.pid == 0x1000).collect();
    assert_eq!(pmt.len(), 4, "32 stream entries of 21 bytes");
    assert!(pmt[0].pusi && pmt[1..].iter().all(|p| !p.pusi));
    assert_eq!(pmt.iter().map(|p| p.cc).collect::<Vec<_>>(), [0, 1, 2, 3]);
    let mut section: Vec<u8> =
        ts.chunks(PACKET_SIZE).zip(&pk).filter(|(_, p)| p.pid == 0x1000).flat_map(|(raw, p)| raw[if p.pusi { 5 } else { 4 }..].to_vec()).collect();
    let len = 3 + ((section[1] as usize & 0x0F) << 8 | section[2] as usize);
    section.truncate(len);
    assert_eq!(psi::crc32(&section), 0);
}
