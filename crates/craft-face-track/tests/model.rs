//! Checks on the published Face Landmarker bundle. The test never downloads: it looks for the
//! bundle at `$CRAFT_FACE_LANDMARKER` (when the test runs, else when it was built), then at `<workspace>/target/models/face_landmarker.task`,
//! and skips when neither exists.
//!
//! The reference sums come from TensorFlow Lite's own interpreter running the same models on the
//! same input.

use std::path::PathBuf;
use std::sync::Arc;

use craft_face_track::{Bundle, Frame, HeadPose, Landmarker, Mesh, Pool, Roi, TrackFrame, Tracker, TrackerOptions};

fn bundle_path() -> Option<PathBuf> {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    // The variable is read when the test runs, then as it was when the test was built (cargo's
    // `[env]` settings reach the compiler).
    std::env::var_os("CRAFT_FACE_LANDMARKER")
        .map(PathBuf::from)
        .into_iter()
        .chain(option_env!("CRAFT_FACE_LANDMARKER").map(PathBuf::from))
        .chain([workspace.join("target/models/face_landmarker.task")])
        .find(|p| p.is_file())
}

fn bundle() -> Option<craft_face_track::Result<Arc<Bundle>>> {
    let Some(p) = bundle_path() else {
        eprintln!("skipped: no face_landmarker.task (set CRAFT_FACE_LANDMARKER)");
        return None;
    };
    Some(Bundle::open(&p).map(Arc::new))
}

fn input(n: usize) -> Vec<f32> {
    (0..n).map(|i| (i as f32 * 0.013).sin() * 0.5).collect()
}

fn close(got: f64, want: f64) {
    assert!((got - want).abs() <= 1e-5 * want.abs().max(1.0), "{got} != {want}");
}

#[test]
fn the_models_match_the_reference_interpreter() {
    let Some(b) = bundle() else { return };
    let b = b.unwrap();
    let (det_in, mesh_in) = (input(128 * 128 * 3), input(256 * 256 * 3));
    for pool in [Pool::single(), Pool::new(2).unwrap()] {
        let sum = |m: &craft_face_track::tflite::Model, x: &[f32], k: usize| {
            let mut ws = m.workspace();
            m.run_into(&mut ws, &[x], &pool).unwrap();
            m.output_values(&ws, k).unwrap().iter().map(|&v| v as f64).sum::<f64>()
        };
        close(sum(b.detector(), &det_in, 0), 54_202.040_273);
        close(sum(b.detector(), &det_in, 1), -3_061.866_384);
        close(sum(b.mesh(), &mesh_in, 0), 119_864.749_257);
        close(sum(b.mesh(), &mesh_in, 1), -19.777_233);
    }
    // A workspace is reused across runs: the second run gives the first run's results.
    let m = b.mesh();
    let mut ws = m.workspace();
    let pool = Pool::new(2).unwrap();
    m.run_into(&mut ws, &[&vec![0.25f32; 256 * 256 * 3]], &pool).unwrap();
    m.run_into(&mut ws, &[&mesh_in], &pool).unwrap();
    close(m.output_values(&ws, 0).unwrap().iter().map(|&v| v as f64).sum(), 119_864.749_257);
}

#[test]
fn the_canonical_face_is_read() {
    let Some(b) = bundle() else { return };
    let b = b.unwrap();
    let c = b.canonical();
    assert_eq!(c.len(), 468);
    // The nose tip is in front, the forehead up, the chin down, and the subject's right eye's
    // outer corner on the left of the picture.
    assert!((c[1][2] - 7.476).abs() < 1e-2, "{:?}", c[1]);
    assert!((c[10][1] - 8.26).abs() < 1e-2, "{:?}", c[10]);
    assert!((c[152][1] + 9.4).abs() < 1e-2, "{:?}", c[152]);
    assert!((c[33][0] + 4.446).abs() < 1e-2, "{:?}", c[33]);
    assert_eq!(b.basis().len(), 33);
    let w: f32 = b.basis().iter().map(|p| p.1).sum();
    assert!((w - 1.055).abs() < 1e-2, "{w}");
}

#[test]
fn a_posed_canonical_face_gives_its_pose() {
    let Some(b) = bundle() else { return };
    let b = b.unwrap();
    let (yaw, pitch, roll) = (25f64.to_radians(), 12f64.to_radians(), -8f64.to_radians());
    // Yaw about y, then pitch (up is a negative turn about x), then roll about z.
    let ry = |v: [f64; 3]| [yaw.cos() * v[0] + yaw.sin() * v[2], v[1], -yaw.sin() * v[0] + yaw.cos() * v[2]];
    let rx = |v: [f64; 3]| [v[0], pitch.cos() * v[1] + pitch.sin() * v[2], -pitch.sin() * v[1] + pitch.cos() * v[2]];
    let rz = |v: [f64; 3]| [roll.cos() * v[0] - roll.sin() * v[1], roll.sin() * v[0] + roll.cos() * v[1], v[2]];
    let mut pts: Vec<[f32; 3]> = b
        .canonical()
        .iter()
        .map(|c| {
            let v = ry(rx(rz([c[0] as f64, c[1] as f64, c[2] as f64])));
            [(640.0 + 20.0 * v[0]) as f32, (360.0 - 20.0 * v[1]) as f32, (-20.0 * v[2]) as f32]
        })
        .collect();
    pts.resize(478, [0.0; 3]);
    let p = HeadPose::of(b.canonical(), b.basis(), &pts).unwrap();
    assert!((p.yaw - 25.0).abs() < 0.05 && (p.pitch - 12.0).abs() < 0.05 && (p.roll + 8.0).abs() < 0.05, "{p:?}");
    assert!((p.scale - 20.0).abs() < 1e-2 && (p.center[0] - 640.0).abs() < 0.1 && (p.center[1] - 360.0).abs() < 0.1, "{p:?}");
}

#[test]
fn pictures_without_a_face_find_none() {
    let Some(b) = bundle() else { return };
    let b = b.unwrap();
    let mut t = Tracker::new(b.clone(), Pool::new(2).unwrap(), TrackerOptions::default()).unwrap();
    let mut out = TrackFrame::new();
    let gray = vec![128u8; 320 * 240 * 4];
    let noise: Vec<u8> = (0..320 * 240 * 4u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8).collect();
    for (k, px) in [&gray, &noise].into_iter().enumerate() {
        let f = Frame::packed(px, 320, 240).unwrap();
        t.track(&f, k as f64 / 30.0, &mut out).unwrap();
        assert!(!out.found && !t.is_following());
    }
    // A crop far outside the picture, a tiny one and one that is not a number run without
    // failing.
    let mut l = Landmarker::new(b);
    let f = Frame::packed(&gray, 320, 240).unwrap();
    let mut m = Mesh::new();
    for roi in [
        Roi { center: [-5e3, 9e3], size: 300.0, angle: 2.0 },
        Roi { center: [10.0, 10.0], size: 1.0, angle: 0.0 },
        Roi { center: [f32::NAN; 2], size: 50.0, angle: 0.0 },
    ] {
        l.mesh(&f, &roi, &Pool::single(), &mut m).unwrap();
        assert!(m.points.iter().flatten().all(|v| v.is_finite()));
    }
    assert!(t.track(&f, f64::NAN, &mut out).is_err());
}
