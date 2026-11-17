//! The muxer's output read back with craft-media's transport stream demuxer: streams, codecs,
//! every frame's bytes and timestamps, keyframes, the PCR, and no continuity or CRC warnings.
// Unwrapping and panicking on failure is fine here, unlike in shipped code (AGENTS.md › Never crash).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::*;
use craft_media::mpegts::{self, Codec as Demuxed, StreamId};
use craft_tsmux::{Codec, Config, MAX_PCR_INTERVAL, Muxer, Stream};

const DELAY: i64 = 63_000;

/// Muxes `items`, reads the file back and compares every unit. Returns the file and the PCRs.
fn round_trip(cfg: Config, streams: &[Stream], items: &mut [Item], want: &[Demuxed]) -> (Vec<u8>, Vec<i64>) {
    let mut m = Muxer::new(Vec::new(), cfg, streams).unwrap();
    write_all(&mut m, items);
    let pcr_pid = m.pcr_pid();
    let stats = m.stats();
    let ts = m.finish().unwrap();
    assert_eq!(stats.bytes(), ts.len() as u64);
    assert_eq!(stats.frames, items.len() as u64);
    let pcrs = check_packets(&ts, pcr_pid, i64::from(cfg.pcr_interval) * 300);

    let file = mpegts::open(&ts).unwrap();
    assert_eq!(file.format, mpegts::Format::Ts { packet_size: 188, sync_offset: 0 });
    assert!(file.warnings.is_empty(), "{:?}", file.warnings);
    assert_eq!(file.program, Some(cfg.program_number));
    assert_eq!(file.streams.len(), streams.len());
    assert!(file.pcr_range.is_some());
    for (s, (st, codec)) in file.streams.iter().zip(streams.iter().zip(want)) {
        assert_eq!(s.id, StreamId::Pid(st.pid));
        assert_eq!(&s.codec, codec, "PID {:#x}", st.pid);
        assert_eq!(s.stream_type, st.codec.stream_type());
        assert_eq!(s.language, st.language.map(|l| String::from_utf8(l.to_vec()).unwrap()));
    }
    for (k, _) in streams.iter().enumerate() {
        let mine: Vec<&Item> = items.iter().filter(|i| i.stream == k).collect();
        let units = &file.streams[k].units;
        assert_eq!(units.len(), mine.len(), "stream {k}");
        for (n, (u, i)) in units.iter().zip(&mine).enumerate() {
            assert_eq!(u.pts, Some(i.pts), "stream {k} unit {n}");
            assert_eq!(u.dts.or(u.pts), Some(i.dts), "stream {k} unit {n}");
            if streams[k].codec.is_video() {
                assert_eq!(u.key, i.key, "stream {k} unit {n}");
            }
            assert_eq!(file.read_unit(&ts, k, n).unwrap(), i.expect, "stream {k} unit {n}");
        }
    }
    (ts, pcrs)
}

#[test]
fn every_codec_in_one_program() {
    let streams = [
        Stream::new(0x100, Codec::H264),
        Stream::new(0x101, Codec::Aac { object_type: 2, sample_rate_index: 3, channels: 2 }).with_language(*b"eng"),
        Stream::new(0x102, Codec::Opus { channels: 2 }).with_language(*b"deu"),
        Stream::new(0x103, Codec::Ac3),
        Stream::new(0x104, Codec::MpegAudio { low_sample_rate: false }),
    ];
    let mut items = Vec::new();
    let t0 = 90_000;
    for k in 0..90i64 {
        // decode order I P B B P B B ...: P pictures shown 3 frames later, B pictures 1 later
        let key = k % 30 == 0;
        let (disposable, show) = match k % 3 {
            0 => (false, 3),
            _ => (true, 1),
        };
        let size = if key { 9000 } else { 300 + (k as usize * 37) % 2000 };
        let data = h264(key, disposable && !key, size, k as u8);
        let dts = t0 + k * 3000;
        items.push(Item { stream: 0, expect: with_aud(Codec::H264, &data), data, pts: dts + show * 3000, dts, key });
    }
    for k in 0..140i64 {
        let raw = {
            let mut v = Vec::new();
            fill(&mut v, 200 + (k as usize * 13) % 600, k as u8);
            v
        };
        let mut expect = adts(0, 0)[..7].to_vec();
        let len = raw.len() + 7;
        expect[3] = 0x80 | (len >> 11) as u8;
        expect[4] = (len >> 3) as u8;
        expect[5] = ((len & 7) as u8) << 5 | 0x1F;
        expect.extend_from_slice(&raw);
        items.push(Item { stream: 1, data: raw, pts: t0 + k * 1920, dts: t0 + k * 1920, key: true, expect });
    }
    for k in 0..150i64 {
        let mut p = vec![0xFC];
        fill(&mut p, 1 + (k as usize * 41) % 700, k as u8);
        items.push(Item { stream: 2, expect: opus_unit(&p), data: p, pts: t0 + k * 1800, dts: t0 + k * 1800, key: true });
    }
    for k in 0..90i64 {
        let f = ac3(k as u8);
        items.push(Item { stream: 3, expect: f.clone(), data: f, pts: t0 + k * 2880, dts: t0 + k * 2880, key: true });
    }
    for k in 0..120i64 {
        let f = mp3(false, k as u8);
        items.push(Item { stream: 4, expect: f.clone(), data: f, pts: t0 + k * 2160, dts: t0 + k * 2160, key: true });
    }
    let want = [Demuxed::H264, Demuxed::AacAdts, Demuxed::Unknown(6), Demuxed::Ac3, Demuxed::MpegAudio];
    let (ts, pcrs) = round_trip(Config::default(), &streams, &mut items, &want);
    let file = mpegts::open(&ts).unwrap();
    assert_eq!(file.pcr_range, Some((pcrs[0], *pcrs.last().unwrap())));
    assert_eq!(pcrs[0], (t0 - DELAY) * 300);
    // the frames' own headers read back with the demuxer's frame parser
    let aac = file.read_unit(&ts, 1, 0).unwrap();
    assert_eq!(mpegts::frame_info(&Demuxed::AacAdts, &aac).map(|i| (i.sample_rate, i.channels)), Some((48_000, 2)));
}

#[test]
fn hevc_with_adts_and_a_pcr_pid_of_its_own() {
    let streams = [Stream::new(0x1FFE, Codec::Hevc), Stream::new(0x0010, Codec::AacAdts)];
    let cfg = Config { pcr_pid: Some(0x0011), pcr_interval: MAX_PCR_INTERVAL, psi_interval: 45_000, ..Config::default() };
    let mut items = Vec::new();
    for k in 0..60i64 {
        let key = k % 20 == 0;
        let data = hevc(key, if key { 20_000 } else { 700 }, k as u8);
        let t = 70_000 + k * 3003;
        items.push(Item { stream: 0, expect: with_aud(Codec::Hevc, &data), data, pts: t, dts: t, key });
    }
    for k in 0..95i64 {
        let f = adts(150 + k as usize, k as u8);
        let t = 70_000 + k * 1920;
        items.push(Item { stream: 1, expect: f.clone(), data: f, pts: t, dts: t, key: true });
    }
    let (ts, _) = round_trip(cfg, &streams, &mut items, &[Demuxed::Hevc, Demuxed::AacAdts]);
    let file = mpegts::open(&ts).unwrap();
    assert!(file.pcr_range.is_some(), "the demuxer reads the PCR from the PMT's PCR_PID");
}

#[test]
fn mpeg2_video_and_low_rate_mpeg_audio() {
    let streams = [Stream::new(0x200, Codec::Mpeg2Video), Stream::new(0x201, Codec::MpegAudio { low_sample_rate: true })];
    let mut items = Vec::new();
    for k in 0..50i64 {
        let key = k % 12 == 0;
        let data = mpeg2(key, (k % 12) as u16, if key { 4000 } else { 900 }, k as u8);
        let t = 100_000 + k * 3600;
        items.push(Item { stream: 0, expect: data.clone(), data, pts: t, dts: t, key });
    }
    for k in 0..70i64 {
        let f = mp3(true, k as u8);
        let t = 100_000 + k * 2160;
        items.push(Item { stream: 1, expect: f.clone(), data: f, pts: t, dts: t, key: true });
    }
    round_trip(Config::default(), &streams, &mut items, &[Demuxed::Mpeg2Video, Demuxed::MpegAudio]);
}

#[test]
fn timestamps_run_past_the_33_bit_wrap() {
    let streams = [Stream::new(0x100, Codec::H264), Stream::new(0x101, Codec::Opus { channels: 1 })];
    let start = (1i64 << 33) - 90_000;
    let mut items = Vec::new();
    for k in 0..90i64 {
        let data = h264(k == 0, false, 500, k as u8);
        let t = start + k * 3000;
        items.push(Item { stream: 0, expect: with_aud(Codec::H264, &data), data, pts: t, dts: t, key: k == 0 });
    }
    for k in 0..140i64 {
        let p = vec![0x08, k as u8 | 1, 0x55];
        let t = start + k * 1800;
        items.push(Item { stream: 1, expect: opus_unit(&p), data: p, pts: t, dts: t, key: true });
    }
    round_trip(Config::default(), &streams, &mut items, &[Demuxed::H264, Demuxed::Unknown(6)]);
}

#[test]
fn a_single_frame_and_a_huge_one() {
    let streams = [Stream::new(0x100, Codec::H264)];
    // 1000 bytes: the demuxer needs five packets in a row to recognize the file
    let one = h264(true, false, 1000, 1);
    let mut items = vec![Item { stream: 0, expect: with_aud(Codec::H264, &one), data: one, pts: 90_000, dts: 90_000, key: true }];
    round_trip(Config::default(), &streams, &mut items, &[Demuxed::H264]);
    // past PES_packet_length's 65 535 bytes: an unbounded video PES packet
    let big = h264(true, false, 3_000_000, 2);
    let small = h264(false, false, 100, 3);
    let mut items = vec![
        Item { stream: 0, expect: with_aud(Codec::H264, &big), data: big, pts: 90_000, dts: 90_000, key: true },
        Item { stream: 0, expect: with_aud(Codec::H264, &small), data: small, pts: 93_000, dts: 93_000, key: false },
    ];
    round_trip(Config::default(), &streams, &mut items, &[Demuxed::H264]);
}
