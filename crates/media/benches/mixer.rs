//! Audio mixer benchmark: one mixer tick, mixing and metering eight sources into one block.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use studiocraft_media::audio::BLOCK_SAMPLES;
use studiocraft_media::mix::{clip, mix_into};

fn mixer(c: &mut Criterion) {
    let inputs: Vec<Vec<f32>> = (0..8).map(|k| (0..BLOCK_SAMPLES).map(|i| ((i * (k + 1)) % 200) as f32 / 400.0 - 0.25).collect()).collect();
    let mut out = vec![0f32; BLOCK_SAMPLES];
    c.bench_function("mix 8 sources, 1024-frame stereo block", |b| {
        b.iter(|| {
            out.iter_mut().for_each(|s| *s = 0.0);
            for input in &inputs {
                black_box(mix_into(&mut out, input, 0.7));
            }
            clip(&mut out);
            black_box(&out);
        })
    });
}

criterion_group!(benches, mixer);
criterion_main!(benches);
