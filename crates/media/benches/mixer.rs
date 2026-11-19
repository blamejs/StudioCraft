//! Audio mixer benchmark: one mixer tick, mixing and metering eight sources into one block.
//!
//! `mix_8_sources_taps_off` is the tick with audio tracks off: each input checks for a tap and
//! has none, and must time the same as the plain case. `mix_8_sources_8_taps` is the tick with
//! every input and the program tapped: each input's block is mixed and stored into its tap in
//! one pass, and the clipped program is copied into its own tap.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use studiocraft_media::audio::BLOCK_SAMPLES;
use studiocraft_media::mix::{clip, mix_into, mix_into_tap};
use studiocraft_media::tap::TapSlot;

fn inputs() -> Vec<Vec<f32>> {
    (0..8).map(|k| (0..BLOCK_SAMPLES).map(|i| ((i * (k + 1)) % 200) as f32 / 400.0 - 0.25).collect()).collect()
}

fn mixer(c: &mut Criterion) {
    let inputs = inputs();
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

    let off: Vec<Option<TapSlot>> = (0..8).map(|_| None).collect();
    c.bench_function("mix_8_sources_taps_off", |b| {
        b.iter(|| {
            out.iter_mut().for_each(|s| *s = 0.0);
            for (input, tap) in inputs.iter().zip(black_box(&off)) {
                match tap {
                    Some(t) => {
                        if let Some(mut block) = t.claim() {
                            black_box(mix_into_tap(&mut out, input, 0.7, &mut block.samples));
                            t.push(block);
                        }
                    }
                    None => {
                        black_box(mix_into(&mut out, input, 0.7));
                    }
                }
            }
            clip(&mut out);
            black_box(&out);
        })
    });

    // The reader takes every block right away, as the collect thread does.
    let taps: Vec<TapSlot> = (0..8).map(|_| TapSlot::new()).collect();
    let program = TapSlot::new();
    c.bench_function("mix_8_sources_8_taps", |b| {
        b.iter(|| {
            out.iter_mut().for_each(|s| *s = 0.0);
            for (input, t) in inputs.iter().zip(&taps) {
                match t.claim() {
                    Some(mut block) => {
                        black_box(mix_into_tap(&mut out, input, 0.7, &mut block.samples));
                        t.push(block);
                    }
                    None => {
                        black_box(mix_into(&mut out, input, 0.7));
                    }
                }
            }
            clip(&mut out);
            if let Some(mut block) = program.claim() {
                block.samples.copy_from_slice(&out);
                program.push(block);
            }
            for t in taps.iter().chain([&program]) {
                drop(t.pop());
            }
            black_box(&out);
        })
    });
}

criterion_group!(benches, mixer);
criterion_main!(benches);
