//! Audio feed benchmark: the work an application capture's thread does for each packet, turning
//! 10 ms of 48 kHz stereo samples into pipeline blocks while the mixer takes them.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::hint::black_box;
use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use studiocraft_media::audio::SAMPLE_RATE;
use studiocraft_media::clock::MasterClock;
use studiocraft_plugin::AudioOut;
use studiocraft_sources::AudioFeed;

fn feed(c: &mut Criterion) {
    let audio = Arc::new(AudioOut::default());
    let mut feed = AudioFeed::new(SAMPLE_RATE, 2, Arc::clone(&audio), MasterClock::new());
    let packet: Vec<f32> = (0..480 * 2).map(|i| (i % 200) as f32 / 400.0 - 0.25).collect();
    c.bench_function("AudioFeed push, 480-frame stereo packet", |b| {
        b.iter(|| {
            feed.push(black_box(&packet));
            while let Some(block) = audio.pop() {
                black_box(&block);
            }
        })
    });
}

criterion_group!(benches, feed);
criterion_main!(benches);
