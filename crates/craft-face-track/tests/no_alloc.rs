//! Running the models and the tracker's search does not allocate once their buffers are warm.
//! This file holds one test, so no other test in the same process allocates while it counts.
//! It needs the Face Landmarker bundle (see `tests/model.rs`) and skips without it.

use std::alloc::System;
use std::path::PathBuf;
use std::sync::Arc;

use craft_face_track::{Bundle, Frame, HeadPose, Landmarker, Mesh, Pool, Roi, TrackFrame, Tracker, TrackerOptions};
use stats_alloc::{INSTRUMENTED_SYSTEM, Region, StatsAlloc};

#[global_allocator]
static GLOBAL: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

fn bundle_path() -> Option<PathBuf> {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    std::env::var_os("CRAFT_FACE_LANDMARKER")
        .map(PathBuf::from)
        .into_iter()
        .chain(option_env!("CRAFT_FACE_LANDMARKER").map(PathBuf::from))
        .chain([workspace.join("target/models/face_landmarker.task")])
        .find(|p| p.is_file())
}

#[test]
fn models_and_search_do_not_allocate() {
    let Some(p) = bundle_path() else {
        eprintln!("skipped: no face_landmarker.task (set CRAFT_FACE_LANDMARKER)");
        return;
    };
    let b = Arc::new(Bundle::open(&p).unwrap());
    let px: Vec<u8> = (0..640 * 480 * 4u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8).collect();
    let f = Frame::packed(&px, 640, 480).unwrap();
    let roi = Roi { center: [320.0, 240.0], size: 300.0, angle: 0.3 };
    let pool = Pool::single();
    let mut l = Landmarker::new(b.clone());
    let mut m = Mesh::new();
    let mut t = Tracker::new(b.clone(), pool.clone(), TrackerOptions::default()).unwrap();
    let mut out = TrackFrame::new();
    // Warm every buffer once.
    l.detect(&f, &roi, &pool).unwrap();
    l.mesh(&f, &roi, &pool, &mut m).unwrap();
    t.track(&f, 0.0, &mut out).unwrap();
    let region = Region::new(GLOBAL);
    for k in 1..=5 {
        l.detect(&f, &roi, &pool).unwrap();
        l.mesh(&f, &roi, &pool, &mut m).unwrap();
        let pose = HeadPose::of(b.canonical(), b.basis(), &m.points);
        std::hint::black_box(pose);
        t.track(&f, k as f64 / 30.0, &mut out).unwrap();
    }
    let s = region.change();
    assert_eq!((s.allocations, s.reallocations), (0, 0), "{s:?}");

    // The same on two worker threads.
    let workers = Pool::new(2).unwrap();
    l.mesh(&f, &roi, &workers, &mut m).unwrap();
    let region = Region::new(GLOBAL);
    let runs = 60;
    for _ in 0..runs {
        l.mesh(&f, &roi, &workers, &mut m).unwrap();
    }
    let s = region.change();
    assert_eq!((s.allocations, s.reallocations), (0, 0), "{runs} runs: {s:?}");
}
