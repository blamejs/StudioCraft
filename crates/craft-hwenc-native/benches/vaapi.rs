//! The work the VA-API encoder does on the CPU for each frame: copying a 1080p and a 4K NV12 frame
//! into a surface's pitched planes, and turning a 1080p picture's coded data into a packet. On a
//! machine with a VA-API H.264 encoder, a 1080p frame encoded end to end is measured as well;
//! elsewhere that case is left out.

use std::hint::black_box;

use craft_hwenc_native::Codec;
use craft_hwenc_native::vaapi::{self, Color, Config, Encoder, Input, Rate};
use criterion::{Criterion, criterion_group, criterion_main};

fn bench(c: &mut Criterion) {
    for (name, w, h) in [("copy nv12 1080p", 1920usize, 1080usize), ("copy nv12 4k", 3840, 2160)] {
        let frame = vec![128u8; w * h * 3 / 2];
        // H.264 surfaces are whole macroblocks, and their rows are rounded up to 64 bytes.
        let coded = [w.div_ceil(16) * 16, h.div_ceil(16) * 16];
        let pitch = coded[0].div_ceil(64) * 64;
        let chroma = pitch * coded[1];
        let mut image = vec![0u8; chroma + pitch * coded[1] / 2];
        c.bench_function(name, |b| b.iter(|| vaapi::copy_nv12(black_box(&frame), w, h, coded, &mut image, [pitch, pitch], [0, chroma])));
    }

    // A P picture of 1080p at 6 Mbit/s and 60 frames per second: 12.5 kB in slices of 1 kB.
    let sets = vec![vec![0x67, 0x64, 0, 0x2a], vec![0x68, 0xee, 0x3c, 0x80]];
    let mut coded = Vec::new();
    for _ in 0..12 {
        coded.extend_from_slice(&[0, 0, 0, 1, 0x41]);
        coded.extend((0..1024u32).map(|i| (i % 251 + 1) as u8));
    }
    c.bench_function("packet 1080p p picture", |b| b.iter(|| vaapi::packet(Codec::H264, &sets, black_box(&coded), 1, false)));

    let cfg = Config {
        codec: Codec::H264,
        input: Input::Nv12,
        width: 1920,
        height: 1080,
        fps: (60, 1),
        rate: Rate::Quality(23),
        keyint: 120,
        color: Color::Bt709,
    };
    let Ok(mut encoder) = Encoder::new(&cfg) else {
        eprintln!("no VA-API H.264 encoder: the encode case is left out");
        return;
    };
    let frame = vec![128u8; Input::Nv12.frame_bytes(1920, 1080)];
    let mut pts = 0;
    c.bench_function("vaapi h264 encode 1080p", |b| {
        b.iter(|| {
            pts += 1;
            encoder.encode(black_box(&frame), pts)
        })
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
