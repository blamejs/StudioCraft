use super::close;
use crate::*;

fn look() -> LookAt {
    LookAt {
        kind: LookAtKind::Bone,
        offset_from_head: Vec3::ZERO,
        horizontal_inner: RangeMap::new(30.0, 6.0),
        horizontal_outer: RangeMap::new(40.0, 12.0),
        vertical_down: RangeMap::new(20.0, 8.0),
        vertical_up: RangeMap::new(25.0, 5.0),
    }
}

#[test]
fn a_range_map_scales_the_size_and_stops_at_its_maximum() {
    let m = RangeMap::new(40.0, 12.0);
    assert_eq!(m.map(10.0), 3.0);
    assert_eq!(m.map(-10.0), 3.0);
    assert_eq!(m.map(40.0), 12.0);
    assert_eq!(m.map(90.0), 12.0);
    assert_eq!(m.map(f32::INFINITY), 12.0);
    assert_eq!(m.map(f32::NAN), 0.0);
    // With no input range, any gaze gives the whole output and none gives nothing.
    let flat = RangeMap::new(0.0, 7.0);
    assert_eq!((flat.map(0.0), flat.map(0.1), flat.map(-3.0)), (0.0, 7.0, 7.0));
}

#[test]
fn the_default_maps_turn_the_eyes_10_degrees_at_90() {
    let l = LookAt::default();
    for m in [l.horizontal_inner, l.horizontal_outer, l.vertical_down, l.vertical_up] {
        assert_eq!(m, RangeMap::new(90.0, 10.0));
    }
    assert_eq!(l.kind, LookAtKind::Bone);
}

#[test]
fn each_eye_uses_the_map_for_its_direction() {
    let l = look();
    // Looking left (positive yaw): the left eye turns outward on the outer map, the right
    // eye inward on the inner map, both toward +X.
    let (left, right) = l.eye_rotations(20.0, 0.0);
    let (ly, _, _) = left.to_yaw_pitch_roll();
    let (ry, _, _) = right.to_yaw_pitch_roll();
    assert!(close(ly, 6.0, 1e-3) && close(ry, 4.0, 1e-3), "{ly} {ry}");
    // Looking right: the left eye uses the inner map and the right eye the outer map.
    let (left, right) = l.eye_rotations(-20.0, 0.0);
    let (ly, _, _) = left.to_yaw_pitch_roll();
    let (ry, _, _) = right.to_yaw_pitch_roll();
    assert!(close(ly, -4.0, 1e-3) && close(ry, -6.0, 1e-3), "{ly} {ry}");
    // Down is a positive pitch on the down map; up a negative one on the up map.
    let (left, right) = l.eye_rotations(0.0, 10.0);
    assert!(close(left.to_yaw_pitch_roll().1, 4.0, 1e-3) && close(right.to_yaw_pitch_roll().1, 4.0, 1e-3));
    let (left, _) = l.eye_rotations(0.0, -50.0);
    assert!(close(left.to_yaw_pitch_roll().1, -5.0, 1e-3));
    // A positive pitch turns the eye's forward axis down.
    let (down, _) = l.eye_rotations(0.0, 20.0);
    assert!(down.rotate(Vec3::Z).y < 0.0);
    let (to_left, _) = l.eye_rotations(40.0, 0.0);
    assert!(to_left.rotate(Vec3::Z).x > 0.0);
}

#[test]
fn look_expressions_use_the_outer_map_for_both_sides() {
    let mut l = look();
    l.kind = LookAtKind::Expression;
    l.horizontal_outer = RangeMap::new(40.0, 1.0);
    l.vertical_down = RangeMap::new(20.0, 1.0);
    l.vertical_up = RangeMap::new(25.0, 1.0);
    let w = l.expression_weights(20.0, 10.0);
    assert_eq!(w, LookWeights { up: 0.0, down: 0.5, left: 0.5, right: 0.0 });
    let w = l.expression_weights(-80.0, -5.0);
    assert_eq!(w, LookWeights { up: 0.2, down: 0.0, left: 0.0, right: 1.0 });
    assert_eq!(l.expression_weights(0.0, 0.0), LookWeights::default());
    // A weight never goes past 1 even when the output scale does.
    l.horizontal_outer = RangeMap::new(10.0, 3.0);
    assert_eq!(l.expression_weights(10.0, 0.0).left, 1.0);
}

#[test]
fn bad_look_at_settings_are_refused() {
    let mut m = super::humanoid();
    assert!(m.validate(&Limits::default()).is_ok());
    m.look_at.vertical_up = RangeMap::new(-1.0, 5.0);
    assert!(m.validate(&Limits::default()).is_err());
    m.look_at.vertical_up = RangeMap::new(10.0, f32::NAN);
    assert!(m.validate(&Limits::default()).is_err());
    m.look_at.vertical_up = RangeMap::new(10.0, -5.0);
    assert!(m.validate(&Limits::default()).is_err());
}
