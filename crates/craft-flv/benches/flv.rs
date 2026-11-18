//! Writing and reading one second of a 6000 kb/s, 60 fps H.264 stream with 48 kHz AAC at 160 kb/s:
//! 60 video tags of about 12 500 bytes (a keyframe with SPS and PPS, then frames of two slices)
//! and 47 audio tags of 426 bytes. Output buffers are cleared between iterations and keep their
//! capacity, so the benches measure the tag work, not growing a buffer.
//!
//! The destination benches send the same second to 4 outputs, as streaming to 4 servers does: once
//! with every destination converting the Annex B input itself, and once with each access unit
//! converted and checked once and the result shared.

use craft_flv::{AnnexB, Body, Header, Reader, TagType, Writer, annexb_to_nalus, parse_audio, parse_video};
use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;
use std::io::{self, BufWriter, Write};

const AVCC: [u8; 7] = [1, 0x64, 0, 0x28, 0xFF, 0xE0, 0];
const VIDEO_BYTES: usize = 12_500;
const AUDIO_BYTES: usize = 426;
const AUDIO_TAGS: usize = 47;
const TAGS: u64 = 1 + 60 + AUDIO_TAGS as u64;
const DESTINATIONS: usize = 4;

/// A small xorshift generator, so the slice bytes are the same on every run.
struct Rng(u64);

impl Rng {
    fn byte(&mut self) -> u8 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 24) as u8
    }
}

/// Slice data of `n` bytes. With `zeros`, the bytes are pseudo-random, so about one in 256 is 0,
/// as in entropy-coded slice data, with emulation prevention applied (a 0x03 after two zero bytes
/// that a byte of 3 or less follows). Without, no byte is 0, which is the scanner's best case.
fn slice_data(n: usize, zeros: bool, rng: &mut Rng) -> Vec<u8> {
    let mut d = Vec::with_capacity(n + n / 64);
    let mut run = 0;
    while d.len() < n - 1 {
        let b = if zeros { rng.byte() } else { (d.len() % 251 + 1) as u8 };
        if run >= 2 && b <= 3 {
            d.push(3);
            run = 0;
        }
        d.push(b);
        run = if b == 0 { run + 1 } else { 0 };
    }
    // The RBSP stop bit: slice data never ends with a zero byte.
    d.push(0x80);
    d
}

fn annexb_frame(key: bool, zeros: bool, rng: &mut Rng) -> Vec<u8> {
    let mut f = Vec::with_capacity(VIDEO_BYTES + 256);
    if key {
        f.extend_from_slice(&[0, 0, 0, 1, 0x67, 0x64, 0, 0x28, 0xAC]);
        f.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xEE, 0x3C, 0x80]);
    }
    let slice_type = if key { 0x65 } else { 0x41 };
    let half = (VIDEO_BYTES - f.len()) / 2;
    for _ in 0..2 {
        f.extend_from_slice(&[0, 0, 1, slice_type]);
        f.extend(slice_data(half - 4, zeros, rng));
    }
    f
}

fn length_prefixed(annexb: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    if annexb_to_nalus(annexb, &mut out).is_err() {
        fail("an Annex B frame did not convert");
    }
    out
}

struct Frames {
    key: Vec<u8>,
    inter: Vec<u8>,
    key_zeros: Vec<u8>,
    inter_zeros: Vec<u8>,
    key_lp: Vec<u8>,
    inter_lp: Vec<u8>,
    audio: Vec<u8>,
}

#[derive(Clone, Copy)]
enum Input {
    /// Annex B access units with no zero bytes in the slices, written to each destination.
    AnnexB,
    /// Annex B access units with zero bytes in the slices, written to each destination.
    AnnexBZeros,
    /// Length-prefixed access units, written to each destination.
    Nalus,
    /// Annex B access units with zero bytes, converted and checked once and shared.
    ConvertOnce,
}

/// Writes the sequence header and one second of tags to every writer and returns the tags written.
fn one_second<W: Write>(ws: &mut [Writer<W>], f: &Frames, input: Input, scratch: &mut Vec<u8>) -> u64 {
    let shared = matches!(input, Input::ConvertOnce);
    let send = |ws: &mut [Writer<W>], t: i32, body: &Body<'_>| {
        if shared {
            if let Ok(c) = body.check() {
                for w in ws.iter_mut() {
                    w.write_checked(t, &c).ok();
                }
            }
        } else {
            for w in ws.iter_mut() {
                w.write(t, body).ok();
            }
        }
    };
    send(ws, 0, &Body::AvcSequenceHeader(&AVCC));
    let mut audio_t = 0.0f64;
    let mut audio_tags = 0;
    for i in 0..60 {
        let t = i * 1000 / 60;
        while audio_t <= f64::from(t) && audio_tags < AUDIO_TAGS {
            send(ws, audio_t as i32, &Body::AacRaw(&f.audio));
            audio_t += 1024.0 / 48.0;
            audio_tags += 1;
        }
        let key = i == 0;
        match input {
            Input::AnnexB => {
                let data = if key { &f.key } else { &f.inter };
                send(ws, t, &Body::AvcAnnexB { key, composition_time: 0, data });
            }
            Input::AnnexBZeros => {
                let data = if key { &f.key_zeros } else { &f.inter_zeros };
                send(ws, t, &Body::AvcAnnexB { key, composition_time: 0, data });
            }
            Input::Nalus => {
                let data = if key { &f.key_lp } else { &f.inter_lp };
                send(ws, t, &Body::AvcNalus { key, composition_time: 0, data });
            }
            Input::ConvertOnce => {
                let data = if key { &f.key_zeros } else { &f.inter_zeros };
                scratch.clear();
                if annexb_to_nalus(data, scratch).is_ok() {
                    send(ws, t, &Body::AvcNalus { key, composition_time: 0, data: scratch });
                }
            }
        }
    }
    ws.iter().map(Writer::tags).sum()
}

/// Writes the file headers and one second of tags into `outs`, after clearing them, and returns
/// the tags written. New writers start the timestamps again; the buffers keep their capacity.
fn into_vecs(outs: &mut [Vec<u8>], f: &Frames, input: Input, scratch: &mut Vec<u8>) -> u64 {
    let mut ws = Vec::with_capacity(outs.len());
    for o in outs.iter_mut() {
        o.clear();
        match Writer::new(std::mem::take(o), Header { audio: true, video: true }) {
            Ok(w) => ws.push(w),
            Err(_) => return 0,
        }
    }
    let tags = one_second(&mut ws, f, input, scratch);
    for (o, w) in outs.iter_mut().zip(ws) {
        *o = w.into_inner();
    }
    tags
}

/// The RTMP path: each body is checked once and its bytes written into every destination's
/// buffer, without FLV tag headers. Returns the bodies written.
fn rtmp_bodies(outs: &mut [Vec<u8>], f: &Frames, scratch: &mut Vec<u8>) -> u64 {
    for o in outs.iter_mut() {
        o.clear();
    }
    let mut n = 0;
    let mut send = |body: &Body<'_>| {
        if let Ok(c) = body.check() {
            for o in outs.iter_mut() {
                if c.write_to(o).is_ok() {
                    n += 1;
                }
            }
        }
    };
    send(&Body::AvcSequenceHeader(&AVCC));
    for i in 0..60 {
        let key = i == 0;
        if i < AUDIO_TAGS {
            send(&Body::AacRaw(&f.audio));
        }
        scratch.clear();
        if annexb_to_nalus(if key { &f.key_zeros } else { &f.inter_zeros }, scratch).is_ok() {
            send(&Body::AvcNalus { key, composition_time: 0, data: scratch });
        }
    }
    n
}

fn fail(what: &str) -> ! {
    eprintln!("{what}; the benches would not measure what they name");
    std::process::exit(1)
}

fn bench(c: &mut Criterion) {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let key = annexb_frame(true, false, &mut rng);
    let inter = annexb_frame(false, false, &mut rng);
    let key_zeros = annexb_frame(true, true, &mut rng);
    let inter_zeros = annexb_frame(false, true, &mut rng);
    if AnnexB::new(&inter_zeros).count() != 2 || !inter_zeros.contains(&0x03) {
        fail("the slices with zero bytes do not split into two NAL units with emulation prevention");
    }
    let f = Frames {
        key_lp: length_prefixed(&key),
        inter_lp: length_prefixed(&inter),
        key,
        inter,
        key_zeros,
        inter_zeros,
        audio: vec![0x21; AUDIO_BYTES],
    };
    let mut one = vec![Vec::with_capacity(1 << 20)];
    let mut many: Vec<Vec<u8>> = (0..DESTINATIONS).map(|_| Vec::with_capacity(1 << 20)).collect();
    let mut scratch = Vec::with_capacity(VIDEO_BYTES * 2);
    // Every tag must be accepted, or the benches would time refusals.
    for input in [Input::AnnexB, Input::AnnexBZeros, Input::Nalus, Input::ConvertOnce] {
        if into_vecs(&mut one, &f, input, &mut scratch) != TAGS {
            fail("a writer refused a tag");
        }
        if into_vecs(&mut many, &f, input, &mut scratch) != TAGS * DESTINATIONS as u64 {
            fail("a writer refused a tag on the way to several destinations");
        }
    }
    if many.windows(2).any(|w| w[0] != w[1]) {
        fail("the destinations received different bytes");
    }
    if rtmp_bodies(&mut many, &f, &mut scratch) != (1 + 60 + AUDIO_TAGS as u64) * DESTINATIONS as u64 {
        fail("an RTMP body was refused");
    }

    let mut g = c.benchmark_group("flv");
    g.throughput(Throughput::Bytes((60 * VIDEO_BYTES + AUDIO_TAGS * AUDIO_BYTES) as u64));
    g.bench_function("write 1 s, Annex B input", |b| b.iter(|| into_vecs(&mut one, &f, Input::AnnexB, &mut scratch)));
    g.bench_function("write 1 s, Annex B input with zero bytes", |b| b.iter(|| into_vecs(&mut one, &f, Input::AnnexBZeros, &mut scratch)));
    g.bench_function("write 1 s, length-prefixed input", |b| b.iter(|| into_vecs(&mut one, &f, Input::Nalus, &mut scratch)));
    g.bench_function("write 1 s to 4 destinations, Annex B per destination", |b| {
        b.iter(|| into_vecs(&mut many, &f, Input::AnnexBZeros, &mut scratch))
    });
    g.bench_function("write 1 s to 4 destinations, converted once", |b| b.iter(|| into_vecs(&mut many, &f, Input::ConvertOnce, &mut scratch)));
    g.bench_function("RTMP bodies 1 s to 4 destinations, converted once", |b| b.iter(|| rtmp_bodies(&mut many, &f, &mut scratch)));

    // An unbuffered output sees one call per tag; a BufWriter in front of it batches them.
    let Ok(mut buffered) = Writer::new(BufWriter::with_capacity(64 << 10, io::sink()), Header { audio: true, video: true }) else {
        fail("the buffered writer did not start")
    };
    g.bench_function("write 1 s through a BufWriter, length-prefixed input", |b| {
        b.iter(|| {
            buffered.reset_timestamps();
            one_second(std::slice::from_mut(&mut buffered), &f, Input::Nalus, &mut scratch)
        })
    });

    // A whole stream with its header, as a pipe delivers it.
    into_vecs(&mut one, &f, Input::AnnexB, &mut scratch);
    let stream = std::mem::take(&mut one[0]);
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
    if read(&mut buf) != TAGS {
        fail("the reader did not parse every tag");
    }
    g.bench_function("read and parse 1 s", |b| b.iter(|| black_box(read(&mut buf))));
    g.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
