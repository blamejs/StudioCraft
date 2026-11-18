use super::*;

/// The smallest valid avcC: version 1, High profile, level 3.1, 4-byte lengths, no parameter sets.
const AVCC: [u8; 7] = [1, 0x64, 0, 0x1F, 0xFF, 0xE0, 0];
/// AAC LC, 48 kHz, stereo.
const ASC: [u8; 2] = [0x11, 0x90];

fn written(f: impl FnOnce(&mut Writer<Vec<u8>>)) -> Vec<u8> {
    let mut w = Writer::new(Vec::new(), Header { audio: true, video: true }).unwrap();
    f(&mut w);
    w.into_inner()
}

/// The bytes after the 13-byte file header and first PreviousTagSize.
fn tags_of(bytes: &[u8]) -> &[u8] {
    &bytes[13..]
}

fn tag_bytes(body: Body<'_>, timestamp: i32) -> Vec<u8> {
    let out = written(|w| {
        w.write(timestamp, &body).unwrap();
    });
    tags_of(&out).to_vec()
}

#[test]
fn header_matches_e2() {
    assert_eq!(Header { audio: true, video: true }.bytes(), [0x46, 0x4C, 0x56, 1, 0x05, 0, 0, 0, 9, 0, 0, 0, 0]);
    assert_eq!(Header { audio: true, video: false }.bytes()[4], 0x04);
    assert_eq!(Header { audio: false, video: true }.bytes()[4], 0x01);
    let w = Writer::new(Vec::new(), Header { audio: true, video: true }).unwrap();
    assert_eq!(w.bytes(), 13);
    assert_eq!(w.into_inner(), Header { audio: true, video: true }.bytes());
}

#[test]
fn avc_sequence_header_tag_bytes() {
    let t = tag_bytes(Body::AvcSequenceHeader(&AVCC), 0);
    let mut want = vec![9, 0, 0, 12, 0, 0, 0, 0, 0, 0, 0, 0x17, 0, 0, 0, 0];
    want.extend_from_slice(&AVCC);
    want.extend_from_slice(&[0, 0, 0, 23]);
    assert_eq!(t, want);
}

#[test]
fn annexb_becomes_length_prefixed() {
    // A 4-byte start code, then a trailing zero byte before a 3-byte start code.
    let au = [0, 0, 0, 1, 0x65, 0xAA, 0, 0, 0, 1, 0x41, 0xBB];
    let body = Body::AvcAnnexB { key: true, composition_time: 0, data: &au };
    assert_eq!(body.size().unwrap(), 5 + 6 + 6);
    let mut out = Vec::new();
    assert_eq!(body.write_to(&mut out).unwrap(), 17);
    assert_eq!(out, [0x17, 1, 0, 0, 0, 0, 0, 0, 2, 0x65, 0xAA, 0, 0, 0, 2, 0x41, 0xBB]);
}

#[test]
fn annexb_skips_empty_nal_units_and_needs_a_start_code() {
    let au = [0, 0, 1, 0, 0, 1, 0x09, 0xF0, 0, 0, 0];
    assert_eq!(AnnexB::new(&au).collect::<Vec<_>>(), [&[0x09, 0xF0][..]]);
    let no_start = [0x65, 0, 0, 1, 0x41];
    assert!(matches!(Body::AvcAnnexB { key: false, composition_time: 0, data: &no_start }.size(), Err(Error::BadInput(_))));
    let only_codes = [0, 0, 1, 0, 0, 0, 1];
    assert!(matches!(Body::AvcAnnexB { key: false, composition_time: 0, data: &only_codes }.size(), Err(Error::BadInput(_))));
}

#[test]
fn composition_time_is_a_signed_24_bit_field() {
    let nal = [0, 0, 0, 1, 0x41];
    let mut out = Vec::new();
    Body::AvcNalus { key: false, composition_time: -1, data: &nal }.write_to(&mut out).unwrap();
    assert_eq!(&out[..5], [0x27, 1, 0xFF, 0xFF, 0xFF]);
    out.clear();
    Body::AvcNalus { key: false, composition_time: 66, data: &nal }.write_to(&mut out).unwrap();
    assert_eq!(&out[..5], [0x27, 1, 0, 0, 66]);
    for ct in [MAX_COMPOSITION_TIME, MIN_COMPOSITION_TIME] {
        assert!(Body::AvcNalus { key: false, composition_time: ct, data: &nal }.size().is_ok());
    }
    for ct in [MAX_COMPOSITION_TIME + 1, MIN_COMPOSITION_TIME - 1] {
        assert!(matches!(Body::AvcNalus { key: false, composition_time: ct, data: &nal }.size(), Err(Error::CompositionTime(c)) if c == ct));
    }
    match parse_video(&[0x27, 1, 0xFF, 0xFF, 0xF6, 0, 0, 0, 1, 0x41]).unwrap() {
        Video::AvcNalus { key: false, composition_time: -10, .. } => {}
        v => panic!("{v:?}"),
    }
}

#[test]
fn timestamp_extended_holds_the_upper_byte() {
    let t = tag_bytes(Body::AvcEndOfSequence, 0x1234_5678);
    assert_eq!(&t[..11], [9, 0, 0, 5, 0x34, 0x56, 0x78, 0x12, 0, 0, 0]);
    assert_eq!(&t[11..16], [0x17, 2, 0, 0, 0]);
    assert_eq!(&t[16..], [0, 0, 0, 16]);
}

#[test]
fn tag_header_refuses_a_size_over_24_bits() {
    assert_eq!(tag_header(TagType::Audio, MAX_TAG_DATA, 0).unwrap()[1..4], [0xFF, 0xFF, 0xFF]);
    assert!(matches!(tag_header(TagType::Audio, MAX_TAG_DATA + 1, 0), Err(Error::TooLarge { size, .. }) if size == MAX_TAG_DATA + 1));
    assert!(tag_header(TagType::Video, usize::MAX, 0).is_err());
}

#[test]
fn aac_tags_match_e4_2() {
    assert_eq!(AAC_TAG_HEADER, 0xAF);
    let t = tag_bytes(Body::AacSequenceHeader(&ASC), 0);
    assert_eq!(t, [8, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0xAF, 0, 0x11, 0x90, 0, 0, 0, 15]);
    let mut out = Vec::new();
    Body::AacRaw(&[0x21, 0x10]).write_to(&mut out).unwrap();
    assert_eq!(out, [0xAF, 1, 0x21, 0x10]);
}

#[test]
fn bad_inputs_are_refused_before_writing() {
    let cases: &[Body<'_>] = &[
        Body::AvcSequenceHeader(&[1, 0x64, 0, 0x1F, 0xFF]),
        Body::AvcSequenceHeader(&[2, 0x64, 0, 0x1F, 0xFF, 0xE0, 0]),
        // 2-byte NAL unit lengths.
        Body::AvcSequenceHeader(&[1, 0x64, 0, 0x1F, 0xFD, 0xE0, 0]),
        Body::AvcNalus { key: true, composition_time: 0, data: &[] },
        Body::AvcNalus { key: true, composition_time: 0, data: &[0, 0, 0, 3, 0x65, 0] },
        Body::AvcNalus { key: true, composition_time: 0, data: &[0, 0, 0, 1, 0x65, 0, 0] },
        Body::AvcNalus { key: true, composition_time: 0, data: &[0, 0, 0, 0] },
        Body::AacSequenceHeader(&[0x11]),
        Body::AacSequenceHeader(&[0x01, 0x90]),
        Body::AacRaw(&[]),
        Body::AacRaw(&[0xFF, 0xF1, 0x50, 0x80, 0x01, 0x7F, 0xFC, 0x21]),
        Body::Script(&[]),
        Body::Script(&[0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ];
    for body in cases {
        let mut w = Writer::new(Vec::new(), Header::default()).unwrap();
        assert!(matches!(w.write(0, body), Err(Error::BadInput(_))), "{body:?}");
        assert_eq!(w.tags(), 0);
        assert_eq!(w.into_inner().len(), 13, "{body:?}");
    }
}

#[test]
fn a_body_over_the_24_bit_limit_is_refused() {
    let frame = vec![0x21; MAX_TAG_DATA - 1];
    assert!(matches!(Body::AacRaw(&frame).size(), Err(Error::TooLarge { size, limit: MAX_TAG_DATA }) if size == MAX_TAG_DATA + 1));
    assert_eq!(Body::AacRaw(&frame[..MAX_TAG_DATA - 2]).size().unwrap(), MAX_TAG_DATA);
}

#[test]
fn timestamps_do_not_go_back_within_a_tag_type() {
    let mut w = Writer::new(Vec::new(), Header { audio: true, video: true }).unwrap();
    assert!(matches!(w.write(-1, &Body::AvcEndOfSequence), Err(Error::Timestamp { timestamp: -1, min: 0 })));
    w.write(40, &Body::AacRaw(&[0x21])).unwrap();
    // Video may still start earlier than the audio already written.
    w.write(33, &Body::AvcSequenceHeader(&AVCC)).unwrap();
    w.write(33, &Body::AvcNalus { key: true, composition_time: 0, data: &[0, 0, 0, 1, 0x65] }).unwrap();
    assert!(matches!(w.write(39, &Body::AacRaw(&[0x21])), Err(Error::Timestamp { timestamp: 39, min: 40 })));
    assert!(matches!(w.write(32, &Body::AvcEndOfSequence), Err(Error::Timestamp { timestamp: 32, min: 33 })));
    assert_eq!(w.tags(), 3);
}

#[test]
fn metadata_bytes_follow_e5_and_amf0() {
    let m = Metadata { width: Some(1920.0), stereo: Some(true), ..Metadata::default() };
    let mut out = Vec::new();
    m.encode(&mut out).unwrap();
    let mut want = vec![2, 0, 10];
    want.extend_from_slice(b"onMetaData");
    want.extend_from_slice(&[8, 0, 0, 0, 2, 0, 5]);
    want.extend_from_slice(b"width");
    want.extend_from_slice(&[0, 0x40, 0x9E, 0, 0, 0, 0, 0, 0, 0, 6]);
    want.extend_from_slice(b"stereo");
    want.extend_from_slice(&[1, 1, 0, 0, 9]);
    assert_eq!(out, want);
    let (name, v) = parse_script(&out).unwrap();
    assert_eq!(name, "onMetaData");
    assert_eq!(Metadata::from_value(&v), m);
}

#[test]
fn h264_aac_metadata_reads_back() {
    let m = Metadata::h264_aac(1920, 1080, 60.0, 6000, 160, 48000, true);
    let out = written(|w| {
        w.write_metadata(0, &m).unwrap();
    });
    let mut r = Reader::new(&out[..]).unwrap();
    let mut buf = Vec::new();
    let tag = r.next_tag(&mut buf).unwrap().unwrap();
    assert_eq!(tag.kind, TagType::Script);
    let (name, v) = parse_script(tag.data).unwrap();
    assert_eq!(name, "onMetaData");
    let back = Metadata::from_value(&v);
    assert_eq!(back, m);
    assert_eq!(back.videocodecid, Some(7.0));
    assert_eq!(back.audiocodecid, Some(10.0));
    let Value::EcmaArray(props) = v else { panic!("not an ECMA array") };
    let names: Vec<&str> = props.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        names,
        [
            "duration",
            "width",
            "height",
            "videodatarate",
            "framerate",
            "videocodecid",
            "audiodatarate",
            "audiosamplerate",
            "audiosamplesize",
            "stereo",
            "audiocodecid"
        ]
    );
}

#[test]
fn a_written_stream_reads_back() {
    let key = [0, 0, 0, 1, 0x67, 0x64, 0, 0, 0, 1, 0x68, 0xEE, 0, 0, 1, 0x65, 0x88, 0x84];
    let inter = [0, 0, 1, 0x41, 0x9A];
    let out = written(|w| {
        w.write_metadata(0, &Metadata::h264_aac(1280, 720, 30.0, 3000, 128, 48000, true)).unwrap();
        w.write(0, &Body::AvcSequenceHeader(&AVCC)).unwrap();
        w.write(0, &Body::AacSequenceHeader(&ASC)).unwrap();
        w.write(0, &Body::AvcAnnexB { key: true, composition_time: 66, data: &key }).unwrap();
        w.write(21, &Body::AacRaw(&[0x21, 0x1B])).unwrap();
        w.write(33, &Body::AvcAnnexB { key: false, composition_time: -33, data: &inter }).unwrap();
        w.write(0x0100_0000, &Body::AvcEndOfSequence).unwrap();
    });
    let mut r = Reader::new(&out[..]).unwrap();
    assert_eq!(r.header(), Header { audio: true, video: true });
    let mut buf = Vec::new();
    let mut seen = Vec::new();
    while let Some(tag) = r.next_tag(&mut buf).unwrap() {
        seen.push(match tag.kind {
            TagType::Script => format!("{} script {}", tag.timestamp, parse_script(tag.data).unwrap().0),
            TagType::Audio => format!("{} {:?}", tag.timestamp, parse_audio(tag.data).unwrap()),
            TagType::Video => match parse_video(tag.data).unwrap() {
                Video::AvcNalus { key, composition_time, data } => {
                    format!("{} key={key} ct={composition_time} {:?}", tag.timestamp, Nalus::new(data).collect::<Vec<_>>())
                }
                v => format!("{} {v:?}", tag.timestamp),
            },
        });
    }
    assert_eq!(r.tags(), 7);
    assert_eq!(
        seen,
        [
            "0 script onMetaData".to_string(),
            format!("0 AvcSequenceHeader({AVCC:?})"),
            format!("0 AacSequenceHeader({ASC:?})"),
            "0 key=true ct=66 [[103, 100], [104, 238], [101, 136, 132]]".to_string(),
            "21 AacRaw([33, 27])".to_string(),
            "33 key=false ct=-33 [[65, 154]]".to_string(),
            "16777216 AvcEndOfSequence".to_string(),
        ]
    );
}

#[test]
fn reader_skips_a_larger_data_offset() {
    let mut bytes = vec![b'F', b'L', b'V', 1, 1, 0, 0, 0, 12, 0xAA, 0xBB, 0xCC, 0, 0, 0, 0];
    bytes.extend_from_slice(&tag_bytes(Body::AvcEndOfSequence, 5));
    let mut r = Reader::new(&bytes[..]).unwrap();
    assert_eq!(r.header(), Header { audio: false, video: true });
    let mut buf = Vec::new();
    let tag = r.next_tag(&mut buf).unwrap().unwrap();
    assert_eq!((tag.kind, tag.timestamp), (TagType::Video, 5));
    assert!(r.next_tag(&mut buf).unwrap().is_none());
}

#[test]
fn reader_refuses_broken_headers() {
    let good = Header { audio: true, video: true }.bytes();
    let mut cases: Vec<(Vec<u8>, &str)> = Vec::new();
    let mut b = good.to_vec();
    b[0] = b'G';
    cases.push((b, "signature"));
    let mut b = good.to_vec();
    b[3] = 2;
    cases.push((b, "version"));
    let mut b = good.to_vec();
    b[4] = 0x15;
    cases.push((b, "reserved flags"));
    let mut b = good.to_vec();
    b[8] = 8;
    cases.push((b, "data offset"));
    let mut b = good.to_vec();
    b[12] = 1;
    cases.push((b, "first PreviousTagSize"));
    let mut b = good.to_vec();
    b[5..9].copy_from_slice(&0x0001_0001u32.to_be_bytes());
    b.extend(std::iter::repeat_n(0u8, 0x1_0000));
    cases.push((b, "data offset over 64 KiB"));
    cases.push((good[..12].to_vec(), "cut off"));
    cases.push((good[..5].to_vec(), "cut off header"));
    for (bytes, what) in cases {
        assert!(Reader::new(&bytes[..]).is_err(), "{what}");
    }
}

#[test]
fn reader_refuses_broken_tags() {
    let one = written(|w| {
        w.write(7, &Body::AacRaw(&[0x21, 0x10])).unwrap();
    });
    let check = |bytes: &[u8]| {
        let mut r = Reader::new(bytes).unwrap();
        let mut buf = Vec::new();
        r.next_tag(&mut buf).map(|t| t.map(|t| t.timestamp))
    };
    assert_eq!(check(&one).unwrap(), Some(7));
    let edit = |i: usize, v: u8| {
        let mut b = one.clone();
        b[13 + i] = v;
        b
    };
    assert!(matches!(check(&edit(0, 0x28)), Err(Error::Unsupported(_))), "filter bit");
    assert!(matches!(check(&edit(0, 0x48)), Err(Error::Invalid(_))), "reserved bits");
    assert!(matches!(check(&edit(0, 7)), Err(Error::Invalid(_))), "tag type");
    assert!(matches!(check(&edit(10, 1)), Err(Error::Invalid(_))), "stream id");
    let last = one.len() - 13 - 1;
    assert!(matches!(check(&edit(last, 16)), Err(Error::PreviousTagSize { expected: 15, found: 16 })));
    for cut in [14, 20, 26, one.len() - 1] {
        assert!(matches!(check(&one[..cut]), Err(Error::Truncated(_))), "cut at {cut}");
    }
    assert_eq!(check(&one[..13]).unwrap(), None);
}

#[test]
fn reader_reuses_its_buffer() {
    let big = vec![0x21; 4000];
    let out = written(|w| {
        w.write(0, &Body::AacRaw(&big)).unwrap();
        w.write(1, &Body::AacRaw(&[0x21])).unwrap();
        w.write(2, &Body::AacRaw(&big[..100])).unwrap();
    });
    let mut r = Reader::new(&out[..]).unwrap();
    let mut buf = Vec::new();
    r.next_tag(&mut buf).unwrap();
    let cap = buf.capacity();
    let ptr = buf.as_ptr();
    while r.next_tag(&mut buf).unwrap().is_some() {
        assert_eq!((buf.capacity(), buf.as_ptr()), (cap, ptr));
    }
}

#[test]
fn parse_video_and_audio_cover_other_codecs() {
    assert_eq!(parse_video(&[0x22, 1, 2]).unwrap(), Video::Other { frame_type: 2, codec: 2 });
    assert_eq!(parse_video(&[0x57, 0]).unwrap(), Video::Other { frame_type: 5, codec: 7 });
    assert_eq!(parse_video(&[0x17, 2, 0, 0, 0]).unwrap(), Video::AvcEndOfSequence);
    assert!(matches!(parse_video(&[0x17, 3, 0, 0, 0]), Err(Error::Invalid(_))));
    assert!(matches!(parse_video(&[0x17, 0, 0, 0, 0]), Err(Error::Truncated(_))));
    assert!(matches!(parse_video(&[0x17, 1, 0]), Err(Error::Truncated(_))));
    // NAL unit lengths that do not cover the data exactly.
    for bad in
        [&[0x17, 1, 0, 0, 0, 0, 0, 0, 9, 0x65][..], &[0x27, 1, 0, 0, 0, 0, 0, 0, 1, 0x41, 0], &[0x27, 1, 0, 0, 0, 0, 0, 0, 0], &[0x27, 1, 0, 0, 0]]
    {
        assert!(matches!(parse_video(bad), Err(Error::Invalid(_))), "{bad:?}");
    }
    assert!(matches!(parse_video(&[]), Err(Error::Truncated(_))));
    assert_eq!(parse_audio(&[0x2F, 0xFF]).unwrap(), Audio::Other { format: 2 });
    assert!(matches!(parse_audio(&[0xAF]), Err(Error::Truncated(_))));
    assert!(matches!(parse_audio(&[0xAF, 0]), Err(Error::Truncated(_))));
    assert!(matches!(parse_audio(&[0xAF, 2, 1]), Err(Error::Invalid(_))));
    // An empty raw frame is refused, as the writer refuses one.
    assert!(matches!(parse_audio(&[0xAF, 1]), Err(Error::Invalid(_))));
    assert_eq!(parse_audio(&[0xAF, 1, 0x21]).unwrap(), Audio::AacRaw(&[0x21]));
}

#[test]
fn reserved_and_extended_frame_types_are_other() {
    // FrameType 0, 6 and 7 are reserved, and a set high bit marks the extended video header,
    // whose low nibble is a packet type rather than a CodecID.
    for first in [0x07u8, 0x67, 0x77, 0x87, 0x97, 0xF7] {
        let tag = [first, 1, 0, 0, 0, 0, 0, 0, 1, 0x41];
        assert_eq!(parse_video(&tag).unwrap(), Video::Other { frame_type: first >> 4, codec: 7 }, "{first:#x}");
    }
    for first in [0x17u8, 0x27, 0x37, 0x47] {
        assert!(matches!(parse_video(&[first, 1, 0, 0, 0, 0, 0, 0, 1, 0x41]).unwrap(), Video::AvcNalus { .. }), "{first:#x}");
    }
}

#[test]
fn annexb_converts_once_into_a_shared_buffer() {
    let au = [0, 0, 0, 1, 0x65, 0xAA, 0, 0, 0, 1, 0x41, 0xBB];
    let mut nalus = vec![0xEE];
    assert_eq!(annexb_to_nalus(&au, &mut nalus).unwrap(), 12);
    assert_eq!(nalus, [0xEE, 0, 0, 0, 2, 0x65, 0xAA, 0, 0, 0, 2, 0x41, 0xBB]);
    // The converted access unit writes the same bytes as the Annex B one.
    let mut a = Vec::new();
    let mut b = Vec::new();
    Body::AvcAnnexB { key: true, composition_time: 5, data: &au }.write_to(&mut a).unwrap();
    Body::AvcNalus { key: true, composition_time: 5, data: &nalus[1..] }.write_to(&mut b).unwrap();
    assert_eq!(a, b);
    for bad in [&[0x65, 0, 0, 1, 0x41][..], &[0, 0, 1, 0, 0, 0, 1]] {
        let mut out = vec![7];
        assert!(matches!(annexb_to_nalus(bad, &mut out), Err(Error::BadInput(_))));
        assert_eq!(out, [7]);
    }
    let mut big = vec![0, 0, 1];
    big.resize(MAX_TAG_DATA, 0x21);
    let mut out = vec![7];
    assert!(matches!(annexb_to_nalus(&big, &mut out), Err(Error::TooLarge { .. })));
    assert_eq!(out, [7]);
}

#[test]
fn an_oversized_annexb_access_unit_stops_at_the_limit() {
    // Many NAL units past the limit: the check stops at the first that crosses it.
    let mut au = Vec::new();
    while au.len() < 2 * MAX_TAG_DATA {
        au.extend_from_slice(&[0, 0, 1]);
        au.extend(std::iter::repeat_n(0x21, 4093));
    }
    let body = Body::AvcAnnexB { key: false, composition_time: 0, data: &au };
    match body.size() {
        Err(Error::TooLarge { size, limit }) => assert!(size > limit && size <= limit + 4 + 4093, "{size}"),
        r => panic!("{r:?}"),
    }
}

#[test]
fn one_checked_body_goes_to_several_writers() {
    let au = [0, 0, 0, 1, 0x65, 0xAA];
    let checked = Body::AvcAnnexB { key: true, composition_time: 0, data: &au }.check().unwrap();
    assert_eq!(checked.len(), 5 + 6);
    assert_eq!(checked.tag_type(), TagType::Video);
    let mut writers: Vec<_> = (0..3).map(|_| Writer::new(Vec::new(), Header { audio: false, video: true }).unwrap()).collect();
    for w in &mut writers {
        assert_eq!(w.write_checked(40, &checked).unwrap(), 11 + 11 + 4);
    }
    let one = tag_bytes(Body::AvcAnnexB { key: true, composition_time: 0, data: &au }, 40);
    for w in writers {
        assert_eq!(tags_of(&w.into_inner()), one);
    }
    // The RTMP path: the length, then the body bytes, without a second check.
    let mut rtmp = Vec::new();
    assert_eq!(checked.write_to(&mut rtmp).unwrap(), checked.len());
    assert_eq!(rtmp, one[11..one.len() - 4]);
}

#[test]
fn reset_timestamps_lets_a_long_stream_start_again() {
    let mut w = Writer::new(Vec::new(), Header { audio: true, video: false }).unwrap();
    w.write(i32::MAX, &Body::AacRaw(&[0x21])).unwrap();
    assert!(matches!(w.write(0, &Body::AacRaw(&[0x21])), Err(Error::Timestamp { .. })));
    w.reset_timestamps();
    w.write(0, &Body::AacSequenceHeader(&ASC)).unwrap();
    w.write(0, &Body::AacRaw(&[0x21])).unwrap();
    assert_eq!(w.tags(), 3);
}

#[test]
fn tag_header_refuses_a_negative_timestamp() {
    assert!(matches!(tag_header(TagType::Audio, 1, -1), Err(Error::Timestamp { timestamp: -1, min: 0 })));
    assert!(tag_header(TagType::Audio, 1, i32::MAX).is_ok());
}

/// Accepts `limit` bytes, fails once with `TimedOut`, then accepts everything.
struct FailsOnce {
    out: Vec<u8>,
    limit: usize,
    failed: bool,
}

impl io::Write for FailsOnce {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if !self.failed && self.out.len() + buf.len() > self.limit {
            let n = self.limit - self.out.len();
            if n == 0 {
                self.failed = true;
                return Err(io::ErrorKind::TimedOut.into());
            }
            self.out.extend_from_slice(&buf[..n]);
            return Ok(n);
        }
        self.out.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_writer_refuses_tags_after_a_write_fails_partway() {
    let out = FailsOnce { out: Vec::new(), limit: 13 + 11 + 3, failed: false };
    let mut w = Writer::new(out, Header { audio: true, video: false }).unwrap();
    assert!(matches!(w.write(0, &Body::AacRaw(&[0x21; 10])), Err(Error::Io(_))));
    let len = w.get_ref().out.len();
    assert_eq!(len, 13 + 11 + 3);
    // The output holds part of a tag, so a later tag would follow it out of step.
    assert!(w.write(1, &Body::AacRaw(&[0x21])).is_err());
    assert!(matches!(w.write(1, &Body::AacRaw(&[0x21])), Err(Error::Broken)));
    // A body that would be refused anyway still reports the broken stream first.
    assert!(matches!(w.write(1, &Body::AacRaw(&[])), Err(Error::Broken)));
    assert!(w.is_broken());
    assert_eq!(w.get_ref().out.len(), len);
    assert_eq!(w.tags(), 0);
}

/// Counts the calls that reach the output.
struct CountsWrites {
    out: Vec<u8>,
    calls: usize,
}

impl io::Write for CountsWrites {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.calls += 1;
        self.out.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn write_vectored(&mut self, bufs: &[io::IoSlice<'_>]) -> io::Result<usize> {
        self.calls += 1;
        let mut n = 0;
        for b in bufs {
            self.out.extend_from_slice(b);
            n += b.len();
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_tag_goes_out_in_one_vectored_write() {
    let mut w = Writer::new(CountsWrites { out: Vec::new(), calls: 0 }, Header { audio: true, video: true }).unwrap();
    let before = w.get_ref().calls;
    w.write(0, &Body::AvcNalus { key: true, composition_time: 0, data: &[0, 0, 0, 1, 0x65] }).unwrap();
    w.write(0, &Body::AacRaw(&[0x21, 0x10])).unwrap();
    w.write(0, &Body::AvcEndOfSequence).unwrap();
    assert_eq!(w.get_ref().calls - before, 3);
    let bytes = w.into_inner().out;
    let mut r = Reader::new(&bytes[..]).unwrap();
    let mut buf = Vec::new();
    let mut n = 0;
    while r.next_tag(&mut buf).unwrap().is_some() {
        n += 1;
    }
    assert_eq!(n, 3);
}

/// Hands out `data`, but fails once with `WouldBlock` when `stall` bytes have been read.
struct StallsOnce {
    data: Vec<u8>,
    pos: usize,
    stall: usize,
    stalled: bool,
}

impl io::Read for StallsOnce {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if !self.stalled && self.pos == self.stall {
            self.stalled = true;
            return Err(io::ErrorKind::WouldBlock.into());
        }
        let end = if self.stalled { self.data.len() } else { self.stall };
        let n = buf.len().min(end - self.pos);
        buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

#[test]
fn a_reader_refuses_tags_after_a_read_fails_partway() {
    // The first tag's frame is itself the bytes of a whole tag, so a reader that went on after
    // losing the first tag's header would read it as a tag and not notice.
    let inner = [8, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0, 0xAF, 1, 0x21, 0, 0, 0, 14];
    let data = written(|w| {
        w.write(0, &Body::AacRaw(&inner)).unwrap();
    });
    let mut r = Reader::new(StallsOnce { data, pos: 0, stall: 13 + 11 + 2, stalled: false }).unwrap();
    let mut buf = Vec::new();
    assert!(matches!(r.next_tag(&mut buf), Err(Error::Io(_))));
    assert!(r.next_tag(&mut buf).is_err());
    assert!(matches!(r.next_tag(&mut buf), Err(Error::Broken)));
    assert!(r.is_broken());
    assert_eq!(r.tags(), 0);
}

#[test]
fn avcc_parameter_sets_must_fit_the_record() {
    // One SPS of 3 bytes and one PPS of 2 bytes, then an extension byte.
    let full = [1, 0x64, 0, 0x1F, 0xFF, 0xE1, 0, 3, 0x67, 0x64, 0x1F, 1, 0, 2, 0x68, 0xEE, 0xFD];
    assert!(Body::AvcSequenceHeader(&full).size().is_ok());
    let mut tag = vec![0x17, 0, 0, 0, 0];
    tag.extend_from_slice(&full);
    assert_eq!(parse_video(&tag).unwrap(), Video::AvcSequenceHeader(&full));
    let cut: &[&[u8]] = &[
        // An SPS is declared but its length is missing.
        &[1, 0x64, 0, 0x1F, 0xFF, 0xE1, 0],
        // The SPS runs past the end.
        &[1, 0x64, 0, 0x1F, 0xFF, 0xE1, 0, 9, 0x67],
        // The PPS count is missing.
        &full[..11],
        // The PPS runs past the end.
        &full[..15],
    ];
    for avcc in cut {
        assert!(matches!(Body::AvcSequenceHeader(avcc).size(), Err(Error::BadInput(_))), "{avcc:?}");
        let mut tag = vec![0x17, 0, 0, 0, 0];
        tag.extend_from_slice(avcc);
        assert!(matches!(parse_video(&tag), Err(Error::Invalid(_))), "{avcc:?}");
    }
}

/// Accepts writes, but its first flush fails.
struct FlushFails(Vec<u8>);

impl io::Write for FlushFails {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Err(io::ErrorKind::TimedOut.into())
    }
}

#[test]
fn a_failed_flush_breaks_the_writer() {
    let mut w = Writer::new(FlushFails(Vec::new()), Header { audio: true, video: false }).unwrap();
    w.write(0, &Body::AacRaw(&[0x21])).unwrap();
    assert!(matches!(w.flush(), Err(Error::Io(_))));
    assert!(w.is_broken());
    assert!(matches!(w.write(1, &Body::AacRaw(&[0x21])), Err(Error::Broken)));
}

#[test]
fn a_reader_does_not_reserve_a_hostile_tag_length() {
    let mut bytes = Header { audio: true, video: false }.bytes().to_vec();
    bytes.extend_from_slice(&[8, 0xFF, 0xFF, 0xFF, 0, 0, 0, 0, 0, 0, 0]);
    bytes.extend_from_slice(&[0xAF; 10]);
    let mut r = Reader::new(&bytes[..]).unwrap();
    let mut buf = Vec::new();
    assert!(matches!(r.next_tag(&mut buf), Err(Error::Truncated(_))));
    assert!(buf.capacity() < 1 << 16, "{} bytes reserved", buf.capacity());
}

#[test]
fn parse_script_needs_a_name_and_one_value() {
    let mut out = Vec::new();
    amf0::write_string(&mut out, "onCuePoint").unwrap();
    amf0::write_null(&mut out);
    assert_eq!(parse_script(&out).unwrap(), ("onCuePoint".to_string(), Value::Null));
    out.push(amf0::marker::NULL);
    assert!(matches!(parse_script(&out), Err(Error::Invalid(_))));
    let mut num = Vec::new();
    amf0::write_number(&mut num, 1.0);
    amf0::write_null(&mut num);
    assert!(matches!(parse_script(&num), Err(Error::Invalid(_))));
}

#[test]
fn nalus_stops_at_a_length_past_the_end() {
    let data = [0, 0, 0, 1, 0x65, 0, 0, 0, 9, 0x41];
    assert_eq!(Nalus::new(&data).collect::<Vec<_>>(), [&[0x65][..]]);
}

#[test]
fn tag_type_values_are_the_rtmp_message_types() {
    assert_eq!(TagType::Audio as u8, 8);
    assert_eq!(TagType::Video as u8, 9);
    assert_eq!(TagType::Script as u8, 18);
    for v in 0..=255u8 {
        assert_eq!(TagType::from_u8(v).map(|t| t as u8), [8, 9, 18].contains(&v).then_some(v));
    }
}

mod amf0_tests {
    use super::*;

    fn round_trip(v: &Value) -> Vec<u8> {
        let mut out = Vec::new();
        v.write(&mut out).unwrap();
        let (back, n) = amf0::read(&out).unwrap();
        assert_eq!(n, out.len());
        assert_eq!(&back, v);
        out
    }

    #[test]
    fn markers_follow_the_specification() {
        assert_eq!(round_trip(&Value::Number(1.5)), [0, 0x3F, 0xF8, 0, 0, 0, 0, 0, 0]);
        assert_eq!(round_trip(&Value::Boolean(true)), [1, 1]);
        assert_eq!(round_trip(&Value::String("ab".into())), [2, 0, 2, b'a', b'b']);
        assert_eq!(round_trip(&Value::Null), [5]);
        assert_eq!(round_trip(&Value::Undefined), [6]);
        assert_eq!(round_trip(&Value::Object(vec![("a".into(), Value::Null)])), [3, 0, 1, b'a', 5, 0, 0, 9]);
        assert_eq!(round_trip(&Value::EcmaArray(vec![])), [8, 0, 0, 0, 0, 0, 0, 9]);
        assert_eq!(round_trip(&Value::StrictArray(vec![Value::Null, Value::Boolean(false)])), [0x0A, 0, 0, 0, 2, 5, 1, 0]);
        assert_eq!(round_trip(&Value::Date { millis: 0.0, offset_minutes: -60 }), [0x0B, 0, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0xC4]);
    }

    #[test]
    fn long_strings_use_the_long_string_marker() {
        let s = "x".repeat(65_536);
        let out = round_trip(&Value::String(s));
        assert_eq!(&out[..5], [0x0C, 0, 1, 0, 0]);
        let s = "x".repeat(65_535);
        let out = round_trip(&Value::String(s));
        assert_eq!(&out[..3], [0x02, 0xFF, 0xFF]);
    }

    #[test]
    fn nested_values_round_trip() {
        let v = Value::Object(vec![
            ("app".into(), Value::String("live".into())),
            ("list".into(), Value::StrictArray(vec![Value::Number(1.0), Value::EcmaArray(vec![("k".into(), Value::Boolean(false))])])),
        ]);
        round_trip(&v);
        assert_eq!(v.get("app").and_then(Value::as_str), Some("live"));
        assert_eq!(v.get("none"), None);
        assert_eq!(Value::Null.get("app"), None);
    }

    #[test]
    fn read_all_reads_a_command() {
        let mut out = Vec::new();
        amf0::write_string(&mut out, "_result").unwrap();
        amf0::write_number(&mut out, 1.0);
        amf0::write_object_start(&mut out);
        amf0::write_property_name(&mut out, "level").unwrap();
        amf0::write_string(&mut out, "status").unwrap();
        amf0::write_object_end(&mut out);
        let values = amf0::read_all(&out).unwrap();
        assert_eq!(values.len(), 3);
        assert_eq!(values[0].as_str(), Some("_result"));
        assert_eq!(values[1].as_number(), Some(1.0));
        assert_eq!(values[2].get("level").and_then(Value::as_str), Some("status"));
    }

    #[test]
    fn hostile_values_are_refused() {
        let mut deep = Vec::new();
        for _ in 0..=amf0::MAX_DEPTH + 1 {
            deep.push(0x0A);
            deep.extend_from_slice(&[0, 0, 0, 1]);
        }
        deep.push(5);
        assert!(matches!(amf0::read(&deep), Err(Error::Invalid(_))));
        assert!(matches!(amf0::read(&[0x0A, 0xFF, 0xFF, 0xFF, 0xFF, 5]), Err(Error::Truncated(_))));
        assert!(matches!(amf0::read(&[0x07, 0, 1]), Err(Error::Unsupported(_))));
        assert!(matches!(amf0::read(&[0x11]), Err(Error::Unsupported(_))));
        assert!(matches!(amf0::read(&[0x09]), Err(Error::Invalid(_))));
        assert!(matches!(amf0::read(&[2, 0, 2, 0xC3, 0x28]), Err(Error::Invalid(_))));
        assert!(matches!(amf0::read(&[2, 0, 5, b'a']), Err(Error::Truncated(_))));
        assert!(matches!(amf0::read(&[3, 0, 1, b'a', 5]), Err(Error::Truncated(_))));
        assert!(matches!(amf0::read(&[0x0C, 0xFF, 0xFF, 0xFF, 0xFF]), Err(Error::Truncated(_))));
        assert!(matches!(amf0::read(&[]), Err(Error::Truncated(_))));
    }

    #[test]
    fn property_names_are_checked() {
        let mut out = Vec::new();
        assert!(matches!(amf0::write_property_name(&mut out, &"n".repeat(65_536)), Err(Error::BadInput(_))));
        assert!(out.is_empty());
    }

    #[test]
    fn one_read_decodes_at_most_max_values() {
        let nulls = vec![5u8; amf0::MAX_VALUES];
        assert_eq!(amf0::read_all(&nulls).unwrap().len(), amf0::MAX_VALUES);
        let more = vec![5u8; amf0::MAX_VALUES + 1];
        assert!(matches!(amf0::read_all(&more), Err(Error::Invalid(_))));
        // The array counts as one value, so MAX_VALUES items are one too many.
        let mut array = vec![0x0A];
        array.extend_from_slice(&(amf0::MAX_VALUES as u32).to_be_bytes());
        array.extend(std::iter::repeat_n(5u8, amf0::MAX_VALUES));
        assert!(matches!(amf0::read(&array), Err(Error::Invalid(_))));
    }

    #[test]
    fn values_at_the_byte_limit_read_back() {
        // One string that takes the whole budget reads back; one more byte is refused both ways.
        let fits = Value::StrictArray(vec![Value::String("x".repeat(amf0::MAX_DECODED_BYTES - std::mem::size_of::<Value>()))]);
        round_trip(&fits);
        let over = Value::StrictArray(vec![Value::String("x".repeat(amf0::MAX_DECODED_BYTES - std::mem::size_of::<Value>() + 1))]);
        let mut out = Vec::new();
        assert!(matches!(over.write(&mut out), Err(Error::BadInput(_))));
        let mut bytes = vec![0x0A, 0, 0, 0, 1];
        amf0::write_string(&mut bytes, &"x".repeat(amf0::MAX_DECODED_BYTES - std::mem::size_of::<Value>() + 1)).unwrap();
        assert!(matches!(amf0::read(&bytes), Err(Error::Invalid(_))));
    }

    /// A strict array of `n` objects that each hold one null with an empty name: 7 input bytes and
    /// 2 values each, but each object's property list takes far more memory than that.
    fn objects_of_one_null(n: u32) -> Vec<u8> {
        let mut out = vec![0x0A];
        out.extend_from_slice(&n.to_be_bytes());
        for _ in 0..n {
            out.extend_from_slice(&[3, 0, 0, 5, 0, 0, 9]);
        }
        out
    }

    #[test]
    fn decoded_memory_is_bounded_by_bytes_as_well_as_values() {
        // 100 000 objects are 200 001 values, but their property lists alone take over 16 MiB.
        assert!(matches!(amf0::read(&objects_of_one_null(100_000)), Err(Error::Invalid(_))));
        assert!(amf0::read(&objects_of_one_null(1_000)).is_ok());
        let v = Value::StrictArray(vec![Value::Object(vec![(String::new(), Value::Null)]); 100_000]);
        assert!(matches!(v.write(&mut Vec::new()), Err(Error::BadInput(_))));
    }

    #[test]
    fn writing_stops_at_the_value_count_reading_allows() {
        // The array counts as one value, so it holds at most MAX_VALUES - 1 items.
        let fits = Value::StrictArray(vec![Value::Null; amf0::MAX_VALUES - 1]);
        round_trip(&fits);
        let mut out = Vec::new();
        assert!(matches!(Value::StrictArray(vec![Value::Null; amf0::MAX_VALUES]).write(&mut out), Err(Error::BadInput(_))));
        let mut out = Vec::new();
        let props = Value::Object(vec![(String::new(), Value::Null); amf0::MAX_VALUES]);
        assert!(matches!(props.write(&mut out), Err(Error::BadInput(_))));
    }

    #[test]
    fn a_long_strict_array_reads_back() {
        let v = Value::StrictArray((0..1000).map(|i| Value::Number(f64::from(i))).collect());
        round_trip(&v);
        // A count the bytes cannot hold is refused before any value is read.
        let mut hostile = vec![0x0A, 0x00, 0x10, 0x00, 0x00];
        hostile.extend(std::iter::repeat_n(5u8, 0x0F_FFFF));
        assert!(matches!(amf0::read(&hostile), Err(Error::Truncated(_))));
    }

    #[test]
    fn an_empty_property_name_round_trips() {
        // Only an empty name followed by the object end marker ends the properties.
        let out = round_trip(&Value::Object(vec![(String::new(), Value::Number(2.0)), ("b".into(), Value::Null)]));
        assert_eq!(&out[..4], [3, 0, 0, 0]);
        round_trip(&Value::EcmaArray(vec![(String::new(), Value::Null)]));
    }

    #[test]
    fn writing_stops_at_the_depth_reading_allows() {
        let nest = |levels: usize| (0..levels).fold(Value::Null, |v, _| Value::StrictArray(vec![v]));
        round_trip(&nest(amf0::MAX_DEPTH));
        let mut out = Vec::new();
        assert!(matches!(nest(amf0::MAX_DEPTH + 1).write(&mut out), Err(Error::BadInput(_))));
        assert!(matches!(nest(1_000).write(&mut out), Err(Error::BadInput(_))));
        let mut deep = Vec::new();
        nest(amf0::MAX_DEPTH + 1).write(&mut deep).ok();
        assert!(amf0::read(&deep).is_err());
    }
}
