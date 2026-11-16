//! Compositor frame time: CPU time to upload, record and submit a 1080p frame, with the GPU
//! finishing each frame before the next.

use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use criterion::{Criterion, criterion_group, criterion_main};
use studiocraft_media::Tick;
use studiocraft_media::queue::Fifo;
use studiocraft_media::video::PixelFormat;
use studiocraft_model::{SourceId, Transform, Vec2};
use studiocraft_plugin::{VideoContent, VideoOut};
use studiocraft_render::{Compositor, EncodeTarget, Gpu, ItemDesc, Plan, SceneDesc};

fn scene(capture: &Arc<VideoOut>) -> SceneDesc {
    let mut items = vec![ItemDesc { source: SourceId(1), video: Arc::clone(capture), transform: Transform::default(), effects: vec![] }];
    for i in 0..8u32 {
        let v = Arc::new(VideoOut::default());
        v.publish(VideoContent::Solid { rgba: [40 * i as u8, 90, 200, 200], width: 320, height: 180 });
        let transform = Transform { position: Vec2::new(100.0 * i as f32, 60.0 * i as f32), rotation: 5.0 * i as f32, ..Transform::default() };
        items.push(ItemDesc { source: SourceId(10 + u64::from(i)), video: v, transform, effects: vec![] });
    }
    SceneDesc { items }
}

fn publish_capture(v: &VideoOut) {
    let mut f = v.frame();
    if f.reshape(1920, 1080, PixelFormat::Bgra8) {
        v.publish(VideoContent::Frame(f));
    }
}

fn bench(c: &mut Criterion) {
    let gpu = match Gpu::headless() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("no GPU, compositor benchmarks skipped: {e}");
            return;
        }
    };
    let Ok(mut comp) = Compositor::new(gpu, 1920, 1080) else { return };
    let capture = Arc::new(VideoOut::default());
    comp.set_plan(Arc::new(Plan { program: scene(&capture), ..Plan::default() }));
    let lost = AtomicU64::new(0);
    let mut n = 0i64;

    c.bench_function("compositor/1080p 9 items + 1080p upload", |b| {
        b.iter(|| {
            publish_capture(&capture);
            n += 1;
            comp.frame(Tick(n), &lost);
            comp.finish(&lost);
        })
    });

    let sink = Arc::new(Fifo::new(8));
    comp.set_encode(Some(EncodeTarget { width: 1920, height: 1080, sinks: vec![Arc::clone(&sink)] }));
    c.bench_function("compositor/1080p 9 items + upload + NV12 read-back", |b| {
        b.iter(|| {
            publish_capture(&capture);
            n += 1;
            comp.frame(Tick(n), &lost);
            comp.finish(&lost);
            while let Some(f) = sink.pop() {
                black_box(f);
            }
        })
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
