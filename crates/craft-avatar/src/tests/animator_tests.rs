use super::{close, humanoid};
use crate::*;

fn still() -> AnimatorSettings {
    AnimatorSettings { idle: IdleMotion { breathing: false, blinking: false, sway: false }, ..AnimatorSettings::default() }
}

fn face(t: f64, yaw: f32) -> TrackFrame {
    let mut f = TrackFrame { time: t, present: true, rotation: Quat::from_yaw_pitch_roll(yaw, 0.0, 0.0), ..TrackFrame::default() };
    f.weights.set(Blendshape::JawOpen, 0.8);
    f.weights.set(Blendshape::EyeBlinkLeft, 0.5);
    f.weights.set(Blendshape::EyeBlinkRight, 1.0);
    f
}

/// Steps at 60 fps from `from` to `to` seconds, giving `frame(t)` each time.
fn run(a: &mut Animator, from: f64, to: f64, frame: impl Fn(f64) -> Option<TrackFrame>) {
    let mut t = from;
    while t < to {
        let f = frame(t);
        a.step(f.as_ref(), t);
        t += 1.0 / 60.0;
    }
}

#[test]
fn the_avatar_follows_the_face() {
    let m = humanoid();
    let mut a = Animator::new(&m, AnimatorSettings { mirror: false, ..still() }, 1).unwrap();
    run(&mut a, 0.0, 1.0, |t| Some(face(t, 20.0)));
    let p = a.pose();
    assert_eq!(p.state, PresenceState::Tracking);
    assert_eq!(p.tracked, 1.0);
    assert!(close(p.head.to_yaw_pitch_roll().0, 20.0, 1e-3));
    // blink, aa, happy: blink is the smaller of the two eyes.
    assert_eq!(p.expressions.len(), 3);
    assert!(close(p.expressions[0], 0.5, 1e-6) && close(p.expressions[1], 0.8, 1e-6) && p.expressions[2] == 0.0, "{:?}", p.expressions);
    // With no new frame the last one stays.
    a.step(None, 1.1);
    assert!(close(a.pose().head.to_yaw_pitch_roll().0, 20.0, 1e-3));
    assert_eq!(a.pose().state, PresenceState::Tracking);
}

#[test]
fn a_mirrored_avatar_turns_the_other_way() {
    let m = humanoid();
    let mut a = Animator::new(&m, still(), 1).unwrap();
    assert!(a.settings().mirror);
    run(&mut a, 0.0, 1.0, |t| {
        let mut f = face(t, 20.0);
        f.weights.set(Blendshape::EyeLookOutLeft, 1.0);
        f.weights.set(Blendshape::EyeLookInRight, 1.0);
        Some(f)
    });
    let p = a.pose();
    assert!(close(p.head.to_yaw_pitch_roll().0, -20.0, 1e-3));
    // The user looks to their own left; the mirrored avatar looks to its right.
    assert!(p.left_eye.to_yaw_pitch_roll().0 < 0.0 && p.right_eye.to_yaw_pitch_roll().0 < 0.0);
}

#[test]
fn a_lost_face_goes_idle_and_comes_back() {
    let m = humanoid();
    let mut a = Animator::new(&m, AnimatorSettings { mirror: false, ..AnimatorSettings::default() }, 4).unwrap();
    run(&mut a, 0.0, 1.0, |t| Some(face(t, 20.0)));
    let lost = |t: f64| Some(TrackFrame { time: t, ..TrackFrame::default() });
    run(&mut a, 1.0, 1.3, lost);
    assert_eq!(a.pose().state, PresenceState::Holding);
    assert!(close(a.pose().head.to_yaw_pitch_roll().0, 20.0, 1e-3));
    // No frames at all while lost keeps it lost.
    run(&mut a, 1.3, 4.0, |_| None);
    let p = a.pose();
    assert_eq!((p.state, p.tracked), (PresenceState::Lost, 0.0));
    let (yaw, _, _) = p.head.to_yaw_pitch_roll();
    assert!(yaw.abs() <= SWAY_DEGREES[0] + 1e-3, "idle yaw {yaw}");
    assert_eq!(p.expressions[1], 0.0, "the mouth closes while idle");
    // Idle blinks drive the blink expression while lost.
    let mut blinked = false;
    let mut t = 4.0;
    while t < 12.0 {
        blinked |= a.step(None, t).expressions[0] > 0.5;
        t += 1.0 / 60.0;
    }
    assert!(blinked);
    run(&mut a, 12.0, 12.5, |t| Some(face(t, 20.0)));
    assert_eq!(a.pose().state, PresenceState::Tracking);
    assert!(close(a.pose().head.to_yaw_pitch_roll().0, 20.0, 1e-3));
}

#[test]
fn idle_blinks_drive_a_lone_one_eye_blink() {
    let mut m = humanoid();
    m.expressions[0].name = ExpressionName::Preset(Preset::BlinkLeft);
    let mut a = Animator::new(&m, AnimatorSettings { idle: IdleMotion { breathing: false, blinking: true, sway: false }, ..still() }, 4).unwrap();
    let mut blinked = false;
    let mut t = 0.0;
    while t < 8.0 {
        blinked |= a.step(Some(&TrackFrame { time: t, ..TrackFrame::default() }), t).expressions[0] > 0.5;
        t += 1.0 / 60.0;
    }
    assert!(blinked);
}

#[test]
fn a_lost_face_can_show_an_expression() {
    let m = humanoid();
    let s = AnimatorSettings { when_lost: WhenLost::Expression { name: ExpressionName::Preset(Preset::Happy) }, ..still() };
    let mut a = Animator::new(&m, s, 1).unwrap();
    run(&mut a, 0.0, 1.0, |t| Some(face(t, 0.0)));
    assert_eq!(a.pose().expressions[2], 0.0);
    run(&mut a, 1.0, 3.0, |t| Some(TrackFrame { time: t, ..TrackFrame::default() }));
    assert_eq!(a.pose().expressions[2], 1.0);
    // A look expression chosen for a lost face shows even when the gaze drives the looks.
    let mut look = humanoid();
    look.look_at.kind = LookAtKind::Expression;
    look.expressions.push(Expression::new(ExpressionName::Preset(Preset::LookDown)));
    let s = AnimatorSettings { when_lost: WhenLost::Expression { name: ExpressionName::Preset(Preset::LookDown) }, ..still() };
    let mut a = Animator::new(&look, s, 1).unwrap();
    run(&mut a, 0.0, 1.0, |t| Some(face(t, 0.0)));
    run(&mut a, 1.0, 3.0, |t| Some(TrackFrame { time: t, ..TrackFrame::default() }));
    assert_eq!(a.pose().expressions[3], 1.0);
    // A name the avatar lacks is kept and does nothing.
    let s = AnimatorSettings { when_lost: WhenLost::Expression { name: ExpressionName::parse("sleepy") }, ..still() };
    assert!(Animator::new(&m, s, 1).is_ok());
}

#[test]
fn calibration_and_overrides_apply_in_the_step() {
    let mut m = humanoid();
    m.expressions[2].override_mouth = Override::Block;
    let mut cal = Calibration { rotation: Quat::from_yaw_pitch_roll(10.0, 0.0, 0.0), ..Calibration::default() };
    cal.weights.set(Blendshape::JawOpen, 0.2);
    let s = AnimatorSettings { mirror: false, calibration: cal, ..still() };
    let mut a = Animator::new(&m, s, 1).unwrap();
    run(&mut a, 0.0, 1.0, |t| Some(face(t, 30.0)));
    assert!(close(a.pose().head.to_yaw_pitch_roll().0, 20.0, 1e-3));
    assert!(close(a.pose().expressions[1], 0.75, 1e-5));
    // Smiling turns happy on, which blocks the mouth.
    run(&mut a, 1.0, 1.5, |t| {
        let mut f = face(t, 30.0);
        f.weights.set(Blendshape::MouthSmileLeft, 1.0);
        f.weights.set(Blendshape::MouthSmileRight, 1.0);
        Some(f)
    });
    assert_eq!(a.pose().expressions[1], 0.0);
    assert_eq!(a.pose().expressions[2], 1.0);
}

#[test]
fn a_calibration_from_unmirrored_frames_centers_a_mirrored_avatar() {
    let m = humanoid();
    let mut c = Calibrator::new(1.0).unwrap();
    let off_axis = |t: f64| TrackFrame {
        time: t,
        present: true,
        rotation: Quat::from_yaw_pitch_roll(15.0, 0.0, 0.0),
        translation: Vec3::new(0.03, 0.0, 0.0),
        ..TrackFrame::default()
    };
    for i in 0..40 {
        c.push(&off_axis(f64::from(i) / 30.0));
    }
    let s = AnimatorSettings { mirror: true, calibration: c.finish().unwrap(), ..still() };
    let mut a = Animator::new(&m, s, 1).unwrap();
    run(&mut a, 0.0, 1.0, |t| Some(off_axis(t)));
    assert!(a.pose().head.angle_to(Quat::IDENTITY) < 0.01, "{:?}", a.pose().head.to_yaw_pitch_roll());
    assert!(a.pose().lean.length() < 1e-6);
    run(&mut a, 1.0, 1.5, |t| Some(face(t, 25.0)));
    assert!(close(a.pose().head.to_yaw_pitch_roll().0, -10.0, 1e-3));
}

#[test]
fn look_expressions_follow_the_gaze() {
    let mut m = humanoid();
    m.look_at.kind = LookAtKind::Expression;
    m.look_at.horizontal_outer = RangeMap::new(30.0, 1.0);
    for name in ["lookLeft", "lookRight"] {
        m.expressions.push(Expression::new(ExpressionName::parse(name)));
    }
    let mut a = Animator::new(&m, AnimatorSettings { mirror: false, ..still() }, 1).unwrap();
    run(&mut a, 0.0, 1.0, |t| {
        let mut f = face(t, 0.0);
        f.weights.set(Blendshape::EyeLookOutLeft, 0.5);
        f.weights.set(Blendshape::EyeLookInRight, 0.5);
        Some(f)
    });
    let p = a.pose();
    assert!(close(p.expressions[3], 0.5, 1e-5) && p.expressions[4] == 0.0, "{:?}", p.expressions);
    assert_eq!(p.left_eye, Quat::IDENTITY);
}

#[test]
fn bad_settings_are_refused() {
    let m = humanoid();
    let bad = [
        AnimatorSettings { lost: LostFace { hold: -1.0, ..LostFace::default() }, ..AnimatorSettings::default() },
        AnimatorSettings {
            mapping: Mapping { rules: vec![Rule { gain: 9.0, ..Mapping::standard().rules[0].clone() }] },
            ..AnimatorSettings::default()
        },
        AnimatorSettings {
            calibration: Calibration { translation: Vec3::new(f32::NAN, 0.0, 0.0), ..Calibration::default() },
            ..AnimatorSettings::default()
        },
    ];
    for s in bad {
        assert!(Animator::new(&m, s, 1).is_err());
    }
}

#[test]
fn settings_read_back_what_they_write() {
    let s = AnimatorSettings { mirror: false, when_lost: WhenLost::HoldLast, ..AnimatorSettings::default() };
    let text = serde_json::to_string(&s).unwrap();
    let back: AnimatorSettings = serde_json::from_str(&text).unwrap();
    assert_eq!(back, s);
    // Missing fields take their defaults.
    let partial: AnimatorSettings = serde_json::from_str(r#"{"mirror": false}"#).unwrap();
    assert_eq!(partial, AnimatorSettings { mirror: false, ..AnimatorSettings::default() });
}
