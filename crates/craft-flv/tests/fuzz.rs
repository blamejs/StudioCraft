//! Random streams written and read back, and random bytes fed to every reader.

use craft_flv::amf0::{self, Value};
use craft_flv::{Audio, Body, Header, Nalus, Reader, TagType, Video, Writer, parse_audio, parse_script, parse_video};
use proptest::prelude::*;

const AVCC: [u8; 7] = [1, 0x64, 0, 0x1F, 0xFF, 0xE0, 0];

#[derive(Debug, Clone)]
enum Frame {
    Video { key: bool, ct: i32, nals: Vec<Vec<u8>>, annexb: bool },
    Audio(Vec<u8>),
}

fn nal() -> impl Strategy<Value = Vec<u8>> {
    // No zero bytes inside, so no start code appears in the payload and none trail it.
    prop::collection::vec(1u8..=255, 1..64)
}

fn frame() -> impl Strategy<Value = Frame> {
    prop_oneof![
        (any::<bool>(), -1000i32..1000, prop::collection::vec(nal(), 1..5), any::<bool>()).prop_map(|(key, ct, nals, annexb)| Frame::Video {
            key,
            ct,
            nals,
            annexb
        }),
        prop::collection::vec(0u8..0xFF, 1..200).prop_map(Frame::Audio),
    ]
}

fn value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        any::<f64>().prop_filter("NaN does not compare equal", |n| !n.is_nan()).prop_map(Value::Number),
        any::<bool>().prop_map(Value::Boolean),
        ".{0,20}".prop_map(Value::String),
        Just(Value::Null),
        Just(Value::Undefined),
    ];
    leaf.prop_recursive(4, 32, 6, |inner| {
        prop_oneof![
            prop::collection::vec(("[a-z]{1,8}", inner.clone()), 0..6).prop_map(Value::Object),
            prop::collection::vec(("[a-z]{1,8}", inner.clone()), 0..6).prop_map(Value::EcmaArray),
            prop::collection::vec(inner, 0..6).prop_map(Value::StrictArray),
        ]
    })
}

/// The NAL units between 0x000001 start codes, found one byte at a time, without trailing zero
/// bytes, leaving out empty ones and the bytes before the first start code.
fn naive_annexb(data: &[u8]) -> Vec<&[u8]> {
    let starts: Vec<usize> = (0..data.len().saturating_sub(2)).filter(|&i| data[i..i + 3] == [0, 0, 1]).collect();
    let mut out = Vec::new();
    for (k, &s) in starts.iter().enumerate() {
        let end = starts.get(k + 1).copied().unwrap_or(data.len());
        let mut nal = &data[s + 3..end.max(s + 3)];
        while let [rest @ .., 0] = nal {
            nal = rest;
        }
        if !nal.is_empty() {
            out.push(nal);
        }
    }
    out
}

proptest! {
    #[test]
    fn streams_read_back(frames in prop::collection::vec(frame(), 0..40), steps in prop::collection::vec(0i32..50, 40)) {
        let mut w = Writer::new(Vec::new(), Header { audio: true, video: true }).unwrap();
        w.write(0, &Body::AvcSequenceHeader(&AVCC)).unwrap();
        let mut t = 0;
        let mut owned = Vec::new();
        for (f, step) in frames.iter().zip(&steps) {
            t += step;
            match f {
                Frame::Video { key, ct, nals, annexb } => {
                    owned.clear();
                    for n in nals {
                        if *annexb {
                            owned.extend_from_slice(&[0, 0, 0, 1]);
                        } else {
                            owned.extend_from_slice(&(n.len() as u32).to_be_bytes());
                        }
                        owned.extend_from_slice(n);
                    }
                    let body = if *annexb {
                        Body::AvcAnnexB { key: *key, composition_time: *ct, data: &owned }
                    } else {
                        Body::AvcNalus { key: *key, composition_time: *ct, data: &owned }
                    };
                    w.write(t, &body).unwrap();
                }
                Frame::Audio(a) => {
                    w.write(t, &Body::AacRaw(a)).unwrap();
                }
            }
        }
        let bytes = w.into_inner();
        let mut r = Reader::new(&bytes[..]).unwrap();
        let mut buf = Vec::new();
        let first = r.next_tag(&mut buf).unwrap().unwrap();
        prop_assert_eq!(parse_video(first.data).unwrap(), Video::AvcSequenceHeader(&AVCC));
        let mut t = 0;
        for (f, step) in frames.iter().zip(&steps) {
            t += step;
            let tag = r.next_tag(&mut buf).unwrap().unwrap();
            prop_assert_eq!(tag.timestamp, t);
            match f {
                Frame::Video { key, ct, nals, .. } => {
                    prop_assert_eq!(tag.kind, TagType::Video);
                    let Video::AvcNalus { key: k, composition_time, data } = parse_video(tag.data).unwrap() else {
                        panic!("not an access unit");
                    };
                    prop_assert_eq!((k, composition_time), (*key, *ct));
                    let back: Vec<&[u8]> = Nalus::new(data).collect();
                    let want: Vec<&[u8]> = nals.iter().map(Vec::as_slice).collect();
                    prop_assert_eq!(back, want);
                }
                Frame::Audio(a) => {
                    prop_assert_eq!(parse_audio(tag.data).unwrap(), Audio::AacRaw(a));
                }
            }
        }
        prop_assert!(r.next_tag(&mut buf).unwrap().is_none());
    }

    #[test]
    fn annexb_splits_like_a_byte_by_byte_scan(bytes in prop::collection::vec(prop_oneof![3 => Just(0u8), 1 => Just(1u8), 2 => any::<u8>()], 0..300)) {
        let fast: Vec<&[u8]> = craft_flv::AnnexB::new(&bytes).collect();
        prop_assert_eq!(fast, naive_annexb(&bytes));
    }

    #[test]
    fn amf0_values_read_back(v in value()) {
        let mut out = Vec::new();
        v.write(&mut out).unwrap();
        let (back, n) = amf0::read(&out).unwrap();
        prop_assert_eq!(n, out.len());
        prop_assert_eq!(back, v);
    }

    #[test]
    fn random_bytes_do_not_panic(bytes in prop::collection::vec(any::<u8>(), 0..512)) {
        let _ = parse_video(&bytes);
        let _ = parse_audio(&bytes);
        let _ = parse_script(&bytes);
        let _ = amf0::read_all(&bytes);
        let _ = Nalus::new(&bytes).count();
        let _ = craft_flv::AnnexB::new(&bytes).count();
        let mut stream = Header { audio: true, video: true }.bytes().to_vec();
        stream.extend_from_slice(&bytes);
        if let Ok(mut r) = Reader::new(&stream[..]) {
            let mut buf = Vec::new();
            while let Ok(Some(_)) = r.next_tag(&mut buf) {}
        }
    }
}
