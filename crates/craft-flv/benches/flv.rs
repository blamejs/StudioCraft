//! Writing and reading one second of a 6000 kb/s, 60 fps H.264 stream with 48 kHz AAC at 160 kb/s:
//! 60 video tags of about 12 500 bytes (a keyframe with SPS and PPS, then frames of two slices)
//! and 47 audio tags of 426 bytes. The output buffer is cleared between iterations and keeps its
//! capacity, so the benches measure the tag work, not growing a buffer.

use craft_flv::{AnnexB, Body, Header, Reader, TagType, Writer, parse_audio, parse_video};
use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;

const AVCC: [u8; 7] = [1, 0x64, 0, 0x28, 0xFF, 0xE0, 0];
const VIDEO_BYTES: usize = 12_500;
const AUDIO_BYTES: usize = 426;
const AUDIO_TAGS: usize = 47;

fn annexb_frame(key: bool) -> Vec<u8> {
    let mut f = Vec::with_capacity(VIDEO_BYTES);
    if key {
        f.extend_from_slice(&[0, 0, 0, 1, 0x67, 0x64, 0, 0x28, 0xAC]);
        f.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xEE, 0x3C, 0x80]);
    }
    let slice_type = if key { 0x65 } else { 0x41 };
    let half = (VIDEO_BYTES - f.len()) / 2;
    for _ in 0..2 {
        f.extend_from_slice(&[0, 0, 1, slice_type]);
        // Slice data with no zero bytes: emulation prevention leaves no start code inside a slice.
        f.extend((0..half - 4).map(|i| (i % 251 + 1) as u8));
    }
    f
}

fn length_prefixed(annexb: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(annexb.len() + 16);
    for nal in AnnexB::new(annexb) {
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}

struct Frames {
    key: Vec<u8>,
    inter: Vec<u8>,
    key_lp: Vec<u8>,
    inter_lp: Vec<u8>,
    audio: Vec<u8>,
}

/// Writes the file header and one second of tags into `out`, after clearing it, and returns the
/// tags written. A new `Writer` starts the timestamps again; the buffer keeps its capacity.
fn one_second(out: &mut Vec<u8>, f: &Frames, annexb: bool) -> u64 {
    out.clear();
    let Ok(mut w) = Writer::new(std::mem::take(out), Header { audio: true, video: true }) else { return 0 };
    w.write(0, &Body::AvcSequenceHeader(&AVCC)).ok();
    let mut audio_t = 0.0f64;
    let mut audio_tags = 0;
    for i in 0..60 {
        let t = i * 1000 / 60;
        while audio_t <= f64::from(t) && audio_tags < AUDIO_TAGS {
            w.write(audio_t as i32, &Body::AacRaw(&f.audio)).ok();
            audio_t += 1024.0 / 48.0;
            audio_tags += 1;
        }
        let key = i == 0;
        let body = match (annexb, key) {
            (true, true) => Body::AvcAnnexB { key, composition_time: 0, data: &f.key },
            (true, false) => Body::AvcAnnexB { key, composition_time: 0, data: &f.inter },
            (false, true) => Body::AvcNalus { key, composition_time: 0, data: &f.key_lp },
            (false, false) => Body::AvcNalus { key, composition_time: 0, data: &f.inter_lp },
        };
        w.write(t, &body).ok();
    }
    let tags = w.tags();
    *out = w.into_inner();
    tags
}

fn bench(c: &mut Criterion) {
    let key = annexb_frame(true);
    let inter = annexb_frame(false);
    let f = Frames { key_lp: length_prefixed(&key), inter_lp: length_prefixed(&inter), key, inter, audio: vec![0x21; AUDIO_BYTES] };
    let mut out = Vec::with_capacity(1 << 20);
    // Every tag must be accepted, or the benches would time refusals.
    let want = 1 + 60 + AUDIO_TAGS as u64;
    for annexb in [true, false] {
        let tags = one_second(&mut out, &f, annexb);
        if tags != want {
            eprintln!("only {tags} of {want} tags were written; the benches are left out");
            return;
        }
    }

    let mut g = c.benchmark_group("flv");
    g.throughput(Throughput::Bytes((60 * VIDEO_BYTES + AUDIO_TAGS * AUDIO_BYTES) as u64));
    g.bench_function("write 1 s, Annex B input", |b| b.iter(|| one_second(&mut out, &f, true)));
    g.bench_function("write 1 s, length-prefixed input", |b| b.iter(|| one_second(&mut out, &f, false)));

    // A whole stream with its header, as a pipe delivers it.
    one_second(&mut out, &f, true);
    let stream = out;
    let mut buf = Vec::with_capacity(VIDEO_BYTES * 2);
    let read = |buf: &mut Vec<u8>| {
        let Ok(mut r) = Reader::new(&stream[..]) else { return 0 };
        let mut parsed = 0u64;
        while let Ok(Some(tag)) = r.next_tag(buf) {
            parsed += match tag.kind {
                TagType::Video => u64::from(parse_video(tag.data).is_ok()),
                TagType::Audio => u64::from(parse_audio(tag.data).is_ok()),
                TagType::Script => 0,
            };
        }
        parsed
    };
    let parsed = read(&mut buf);
    if parsed != want {
        eprintln!("only {parsed} of {want} tags were read; the read bench is left out");
        return;
    }
    g.bench_function("read and parse 1 s", |b| b.iter(|| black_box(read(&mut buf))));
    g.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
