use super::close;
use crate::*;

#[test]
fn the_vocabulary_has_52_distinct_names_in_index_order() {
    for (i, s) in Blendshape::ALL.iter().enumerate() {
        assert_eq!(s.index(), i);
        assert_eq!(Blendshape::from_name(s.name()), Some(*s));
    }
    let mut names: Vec<&str> = Blendshape::ALL.iter().map(|s| s.name()).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), 52);
    assert_eq!(Blendshape::from_name("EyeBlinkLeft"), None);
}

#[test]
fn mirroring_swaps_each_side_and_keeps_the_middle() {
    assert_eq!(Blendshape::EyeBlinkLeft.mirrored(), Blendshape::EyeBlinkRight);
    assert_eq!(Blendshape::MouthRight.mirrored(), Blendshape::MouthLeft);
    assert_eq!(Blendshape::EyeLookInLeft.mirrored(), Blendshape::EyeLookInRight);
    assert_eq!(Blendshape::JawOpen.mirrored(), Blendshape::JawOpen);
    assert_eq!(Blendshape::TongueOut.mirrored(), Blendshape::TongueOut);
    let sided = Blendshape::ALL.iter().filter(|s| s.mirrored() != **s).count();
    assert_eq!(sided, 40);
    for s in Blendshape::ALL {
        assert_eq!(s.mirrored().mirrored(), s);
    }
}

#[test]
fn a_mirrored_frame_swaps_shapes_and_turns_the_other_way() {
    let mut f = TrackFrame {
        present: true,
        rotation: Quat::from_yaw_pitch_roll(20.0, 10.0, -5.0),
        translation: Vec3::new(0.02, 0.01, 0.0),
        ..TrackFrame::default()
    };
    f.weights.set(Blendshape::EyeBlinkLeft, 0.9);
    f.weights.set(Blendshape::MouthSmileRight, 0.4);
    f.weights.set(Blendshape::JawOpen, 0.3);
    let m = f.mirrored();
    assert_eq!(m.weights.get(Blendshape::EyeBlinkRight), 0.9);
    assert_eq!(m.weights.get(Blendshape::EyeBlinkLeft), 0.0);
    assert_eq!(m.weights.get(Blendshape::MouthSmileLeft), 0.4);
    assert_eq!(m.weights.get(Blendshape::JawOpen), 0.3);
    let (y, p, r) = m.rotation.to_yaw_pitch_roll();
    assert!(close(y, -20.0, 1e-3) && close(p, 10.0, 1e-3) && close(r, 5.0, 1e-3), "{y} {p} {r}");
    assert_eq!(m.translation, Vec3::new(-0.02, 0.01, 0.0));
    assert_eq!(m.mirrored(), f);
}

#[test]
fn weights_are_kept_in_range() {
    let mut w = Weights::default();
    w.set(Blendshape::JawOpen, 1.5);
    w.set(Blendshape::MouthClose, -1.0);
    w.set(Blendshape::CheekPuff, f32::NAN);
    assert_eq!((w.get(Blendshape::JawOpen), w.get(Blendshape::MouthClose), w.get(Blendshape::CheekPuff)), (1.0, 0.0, 0.0));
    let raw = Weights([f32::INFINITY; 52]).sanitized();
    assert!(raw.0.iter().all(|v| *v == 1.0));
}

#[test]
fn weights_read_and_write_as_a_name_map() {
    let mut w = Weights::default();
    w.set(Blendshape::JawOpen, 0.25);
    let text = serde_json::to_string(&w).unwrap();
    assert!(text.contains("\"jawOpen\":0.25"));
    let back: Weights = serde_json::from_str(&text).unwrap();
    assert_eq!(back, w);
    // Unknown names are skipped, missing names read as 0, values outside 0 to 1 are clamped.
    let partial: Weights = serde_json::from_str(r#"{"jawOpen": 2.0, "somethingElse": 0.5}"#).unwrap();
    assert_eq!(partial.get(Blendshape::JawOpen), 1.0);
    assert_eq!(partial.0.iter().filter(|v| **v != 0.0).count(), 1);
    assert!(serde_json::from_str::<Weights>(r#"{"jawOpen": "x"}"#).is_err());
    let many: String = (0..=MAX_WEIGHT_ENTRIES).map(|i| format!("\"n{i}\":0")).collect::<Vec<_>>().join(",");
    assert!(serde_json::from_str::<Weights>(&format!("{{{many}}}")).is_err());
}

#[test]
fn sanitizing_replaces_values_that_are_not_finite() {
    let f = TrackFrame {
        present: true,
        time: f64::NAN,
        rotation: Quat::new(f32::NAN, 0.0, 0.0, 1.0),
        translation: Vec3::new(f32::INFINITY, 0.0, 0.0),
        ..TrackFrame::default()
    };
    let s = f.sanitized();
    assert_eq!((s.time, s.rotation, s.translation), (0.0, Quat::IDENTITY, Vec3::ZERO));
}
