//! The face models' run times on one and two threads: the detector (a search), the mesh (one
//! followed picture) and the crop, mesh and pose fit of a followed picture. The expression
//! weights, their smoothing and the filled output that `Tracker::track` adds for each picture are
//! not measured. It needs the Face Landmarker bundle at `$CRAFT_FACE_LANDMARKER` and does nothing
//! without it.

use std::path::PathBuf;
use std::sync::Arc;

use craft_face_track::{Bundle, Frame, HeadPose, Landmarker, Mesh, Pool, Roi};
use criterion::{Criterion, criterion_group, criterion_main};

fn face_models(c: &mut Criterion) {
    let found = std::env::var_os("CRAFT_FACE_LANDMARKER")
        .map(PathBuf::from)
        .into_iter()
        .chain(option_env!("CRAFT_FACE_LANDMARKER").map(PathBuf::from))
        .find(|p| p.is_file());
    let Some(p) = found else {
        eprintln!("skipped: set CRAFT_FACE_LANDMARKER to face_landmarker.task");
        return;
    };
    let Ok(b) = Bundle::open(&p) else { return };
    let b = Arc::new(b);
    let px: Vec<u8> = (0..1280 * 720 * 4u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8).collect();
    let Ok(f) = Frame::packed(&px, 1280, 720) else { return };
    let whole = Roi { center: [640.0, 360.0], size: 1280.0, angle: 0.0 };
    let face = Roi { center: [640.0, 360.0], size: 420.0, angle: 0.1 };
    for threads in [1, 2] {
        let Ok(pool) = Pool::new(threads) else { return };
        let mut l = Landmarker::new(b.clone());
        let mut m = Mesh::new();
        c.bench_function(&format!("detector_{threads}_threads"), |bn| bn.iter(|| l.detect(&f, &whole, &pool).map(<[_]>::len)));
        c.bench_function(&format!("mesh_{threads}_threads"), |bn| bn.iter(|| l.mesh(&f, &face, &pool, &mut m)));
        c.bench_function(&format!("followed_picture_{threads}_threads"), |bn| {
            bn.iter(|| {
                let _ = l.mesh(&f, &face, &pool, &mut m);
                HeadPose::of(b.canonical(), b.basis(), &m.points)
            })
        });
    }
}

criterion_group!(benches, face_models);
criterion_main!(benches);
