//! The collect thread's work for one mixer block: one block from each of 9 taps (8 sources and
//! the program mix) moved into chunks. The budget is 20 µs.

use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use studiocraft_audio_tracks::collect::{BLOCK_TICKS, ToDisk};
use studiocraft_audio_tracks::{Collector, TrackSpec};
use studiocraft_media::Tick;
use studiocraft_media::tap::TapSlot;
use studiocraft_model::SourceId;

fn collect_9_tracks(c: &mut Criterion) {
    let specs: Vec<TrackSpec> =
        (0..9u64).map(|i| TrackSpec { source: (i > 0).then_some(SourceId(i)), name: format!("Track {i}"), slot: Arc::new(TapSlot::new()) }).collect();
    let Some((mut col, free, out)) = Collector::new(96, 160) else { return };
    col.set_tracks(specs.clone());
    let mut n = 0i64;
    c.bench_function("collect_9_tracks_one_block", |b| {
        b.iter(|| {
            for s in &specs {
                if let Some(mut blk) = s.slot.claim() {
                    blk.pts = Tick(n * BLOCK_TICKS);
                    s.slot.push(blk);
                }
            }
            n += 1;
            col.poll();
            // The disk thread's part: chunks go back to the pool.
            while let Some(m) = out.pop() {
                if let ToDisk::Chunk(ch) = m {
                    let _ = free.push(ch.data);
                }
            }
        })
    });
}

criterion_group!(benches, collect_9_tracks);
criterion_main!(benches);
