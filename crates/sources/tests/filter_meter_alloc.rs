//! The noise gate, compressor and limiter neither allocate nor free memory per block while they
//! process audio and write their gain-reduction readings, and reading the meters does not either.
//! This file holds one test, so no other test in the same process allocates while it counts.

use std::alloc::System;

use stats_alloc::{INSTRUMENTED_SYSTEM, Region, StatsAlloc};
use studiocraft_media::audio::BLOCK_SAMPLES;
use studiocraft_plugin::{Filter, FilterInstance};
use studiocraft_sources::filters::{Compressor, Limiter, NoiseGate};

#[global_allocator]
static GLOBAL: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

#[test]
fn metered_filters_process_blocks_without_allocating() {
    let types: [&dyn Filter; 3] = [&NoiseGate, &Compressor, &Limiter];
    let mut filters: Vec<Box<dyn FilterInstance>> = types.iter().map(|f| f.create(&f.schema().defaults()).unwrap()).collect();
    let meters: Vec<_> = filters.iter().map(|f| f.meter().unwrap()).collect();
    let loud = vec![0.8f32; BLOCK_SAMPLES];
    let quiet = vec![0.0001f32; BLOCK_SAMPLES];
    let mut block = vec![0.0f32; BLOCK_SAMPLES];
    let mut run = |filters: &mut [Box<dyn FilterInstance>], blocks: usize| {
        let mut deepest = 0.0f32;
        for i in 0..blocks {
            for f in filters.iter_mut() {
                block.copy_from_slice(if (i / 50) % 2 == 0 { &loud } else { &quiet });
                f.process_audio(&mut block);
            }
            deepest = meters.iter().fold(deepest, |d, m| d.max(m.get()));
        }
        deepest
    };
    // Warm up, then count about ten seconds of blocks that swing between loud and quiet.
    run(&mut filters, 50);
    let region = Region::new(GLOBAL);
    let deepest = run(&mut filters, 470);
    let stats = region.change();
    assert!(deepest > 1.0, "the meters moved ({deepest} dB)");
    assert_eq!(stats.allocations, 0, "{stats:?}");
    assert_eq!(stats.deallocations, 0, "{stats:?}");
    assert_eq!(stats.reallocations, 0, "{stats:?}");
}
