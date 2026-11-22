//! Audio filter benchmark: the work the mixer thread does for one 1024-frame stereo block through
//! the noise gate, compressor and limiter, including writing their gain-reduction readings.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use studiocraft_media::audio::BLOCK_SAMPLES;
use studiocraft_plugin::Filter;
use studiocraft_sources::filters::{Compressor, Limiter, NoiseGate};

fn filters(c: &mut Criterion) {
    let types: [&dyn Filter; 3] = [&NoiseGate, &Compressor, &Limiter];
    // A signal that crosses the gate's thresholds and the compressor's threshold.
    let input: Vec<f32> = (0..BLOCK_SAMPLES).map(|i| ((i / 2) as f32 * 0.05).sin() * if i < BLOCK_SAMPLES / 2 { 0.8 } else { 0.01 }).collect();
    for f in types {
        let mut instance = f.create(&f.schema().defaults()).unwrap();
        let mut block = input.clone();
        c.bench_function(&format!("{}, 1024-frame stereo block", f.name()), |b| {
            b.iter(|| {
                block.copy_from_slice(&input);
                instance.process_audio(black_box(&mut block));
            })
        });
    }
}

criterion_group!(benches, filters);
criterion_main!(benches);
