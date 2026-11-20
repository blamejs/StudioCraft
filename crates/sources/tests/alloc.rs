//! An audio feed neither allocates nor frees memory per packet once its block pool has warmed up.
//! This file holds one test, so no other test in the same process allocates while it counts.

use std::alloc::System;
use std::sync::Arc;

use stats_alloc::{INSTRUMENTED_SYSTEM, Region, StatsAlloc};
use studiocraft_media::audio::SAMPLE_RATE;
use studiocraft_media::clock::MasterClock;
use studiocraft_plugin::AudioOut;
use studiocraft_sources::AudioFeed;

#[global_allocator]
static GLOBAL: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

#[test]
fn a_warm_feed_pushes_packets_without_allocating() {
    let audio = Arc::new(AudioOut::default());
    let mut feed = AudioFeed::new(SAMPLE_RATE, 2, Arc::clone(&audio), MasterClock::new());
    let packet = vec![0.1f32; 480 * 2];
    let drain = |audio: &AudioOut| {
        let mut n = 0;
        while audio.pop().is_some() {
            n += 1;
        }
        n
    };
    // Two seconds to warm the pool and the queue.
    for _ in 0..200 {
        feed.push(&packet);
        drain(&audio);
    }
    let region = Region::new(GLOBAL);
    let mut blocks = 0;
    // Ten seconds of 10 ms packets.
    for _ in 0..1000 {
        feed.push(&packet);
        blocks += drain(&audio);
    }
    let stats = region.change();
    assert!(blocks > 400, "the packets made blocks ({blocks})");
    assert_eq!(stats.allocations, 0, "{stats:?}");
    assert_eq!(stats.deallocations, 0, "{stats:?}");
    assert_eq!(stats.reallocations, 0, "{stats:?}");
}
