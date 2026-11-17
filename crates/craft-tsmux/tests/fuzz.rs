//! Random programs, frames and timelines: the packet layer stays valid, and every frame reads back
//! with craft-media's demuxer. Random bytes for every codec never panic.
// Unwrapping and panicking on failure is fine here, unlike in shipped code (AGENTS.md › Never crash).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::*;
use craft_media::mpegts;
use craft_tsmux::{Codec, Config, Error, Frame, MAX_PCR_INTERVAL, Muxer, Stream};
use proptest::prelude::*;

#[derive(Debug, Clone)]
struct Plan {
    pcr_interval: u32,
    pcr_own: bool,
    psi_interval: u32,
    psi_before_keyframes: bool,
    /// (stream 0 = H.264, 1 = Opus, 2 = raw AAC; size; DTS step; PTS offset; key)
    frames: Vec<(usize, usize, i64, i64, bool)>,
}

fn plan() -> impl Strategy<Value = Plan> {
    (
        1u32..=MAX_PCR_INTERVAL,
        any::<bool>(),
        1u32..200_000,
        any::<bool>(),
        prop::collection::vec((0usize..3, 1usize..4000, 0i64..12_000, 0i64..9000, any::<bool>()), 5..60),
    )
        .prop_map(|(pcr_interval, pcr_own, psi_interval, psi_before_keyframes, frames)| Plan {
            pcr_interval,
            pcr_own,
            psi_interval,
            psi_before_keyframes,
            frames,
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn random_programs_read_back(p in plan()) {
        let streams = [
            Stream::new(0x100, Codec::H264),
            Stream::new(0x101, Codec::Opus { channels: 2 }),
            Stream::new(0x102, Codec::Aac { object_type: 2, sample_rate_index: 4, channels: 1 }),
        ];
        let cfg = Config {
            pcr_interval: p.pcr_interval,
            pcr_pid: p.pcr_own.then_some(0x1F0),
            psi_interval: p.psi_interval,
            psi_before_keyframes: p.psi_before_keyframes,
            ..Config::default()
        };
        let mut m = Muxer::new(Vec::new(), cfg, &streams).unwrap();
        let mut t = 63_000i64;
        let mut want: [Vec<(Vec<u8>, i64, i64)>; 3] = Default::default();
        for (n, &(s, size, step, offset, key)) in p.frames.iter().enumerate() {
            t += step;
            let (data, expect) = match s {
                0 => {
                    let d = h264(key, false, size.max(30), n as u8);
                    let e = with_aud(Codec::H264, &d);
                    (d, e)
                }
                1 => {
                    let mut d = vec![0xFC];
                    fill(&mut d, size.min(1500), n as u8);
                    let e = opus_unit(&d);
                    (d, e)
                }
                _ => {
                    let mut d = Vec::new();
                    fill(&mut d, size.min(2000), n as u8);
                    let len = d.len() + 7;
                    let mut e = vec![0xFF, 0xF1, 0x50, 0x40 | (len >> 11) as u8, (len >> 3) as u8, ((len & 7) as u8) << 5 | 0x1F, 0xFC];
                    e.extend_from_slice(&d);
                    (d, e)
                }
            };
            let pts = if s == 0 { t + offset } else { t };
            m.write_frame(s, &Frame { data: &data, pts, dts: Some(t), key }).unwrap();
            want[s].push((expect, pts, t));
        }
        let pcr_pid = m.pcr_pid();
        let gaps = m.stats().pcr_gaps;
        let ts = m.finish().unwrap();
        if gaps == 0 {
            check_packets(&ts, pcr_pid, i64::from(p.pcr_interval) * 300);
        }
        let file = mpegts::open(&ts).unwrap();
        prop_assert!(file.warnings.is_empty(), "{:?}", file.warnings);
        for (k, w) in want.iter().enumerate() {
            let units = &file.streams[k].units;
            prop_assert_eq!(units.len(), w.len());
            for (n, (u, (bytes, pts, dts))) in units.iter().zip(w).enumerate() {
                prop_assert_eq!(u.pts, Some(*pts));
                prop_assert_eq!(u.dts.or(u.pts), Some(*dts));
                prop_assert_eq!(&file.read_unit(&ts, k, n).unwrap(), bytes);
            }
        }
    }

    #[test]
    fn random_bytes_never_panic(
        codec in 0usize..9,
        frames in prop::collection::vec((prop::collection::vec(any::<u8>(), 0..3000), any::<i64>(), prop::option::of(any::<i64>()), any::<bool>()), 1..30),
        interval in 0u32..20_000,
    ) {
        let codec = [
            Codec::H264,
            Codec::Hevc,
            Codec::Mpeg2Video,
            Codec::AacAdts,
            Codec::Aac { object_type: 2, sample_rate_index: 3, channels: 2 },
            Codec::Opus { channels: 2 },
            Codec::Ac3,
            Codec::MpegAudio { low_sample_rate: false },
            Codec::MpegAudio { low_sample_rate: true },
        ][codec];
        let cfg = Config { pcr_interval: interval, ..Config::default() };
        let Ok(mut m) = Muxer::new(Vec::new(), cfg, &[Stream::new(0x100, codec)]) else {
            prop_assert!(interval == 0 || interval > MAX_PCR_INTERVAL);
            return Ok(());
        };
        for (data, pts, dts, key) in &frames {
            match m.write_frame(0, &Frame { data, pts: *pts, dts: *dts, key: *key }) {
                Ok(()) | Err(Error::BadFrame(_) | Error::EmptyFrame | Error::Timestamp(_) | Error::FrameTooLarge { .. }) => {}
                Err(e) => prop_assert!(false, "{e}"),
            }
            let _ = m.advance(*pts);
        }
        let ts = m.finish().unwrap();
        prop_assert_eq!(ts.len() % 188, 0);
        // the demuxer recognizes a transport stream by five sync bytes in a row
        if ts.len() >= 5 * 188 {
            prop_assert!(mpegts::open(&ts).is_ok());
        }
    }
}
