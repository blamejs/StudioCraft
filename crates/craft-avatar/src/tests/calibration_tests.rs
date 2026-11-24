use super::close;
use crate::*;

fn frame(t: f64, yaw: f32, blink: f32, jaw: f32) -> TrackFrame {
    let mut f = TrackFrame {
        time: t,
        present: true,
        rotation: Quat::from_yaw_pitch_roll(yaw, 5.0, 0.0),
        translation: Vec3::new(0.01, 0.02, 0.0),
        ..TrackFrame::default()
    };
    f.weights.set(Blendshape::EyeBlinkLeft, blink);
    f.weights.set(Blendshape::EyeBlinkRight, blink);
    f.weights.set(Blendshape::JawOpen, jaw);
    f.weights.set(Blendshape::MouthSmileLeft, 0.2);
    f
}

fn collect(c: &mut Calibrator, make: impl Fn(f64) -> TrackFrame) {
    for i in 0..=70 {
        c.push(&make(f64::from(i) / 30.0));
    }
}

#[test]
fn an_off_axis_neutral_is_taken_and_measured_from() {
    let mut c = Calibrator::new(DEFAULT_SECONDS).unwrap();
    // The camera sits to one side: the relaxed head reads 15 degrees of yaw, with jitter.
    collect(&mut c, |t| frame(t, 15.0 + if (t * 30.0) as i64 % 2 == 0 { 0.5 } else { -0.5 }, 0.1, 0.05));
    assert!(c.done(2.0));
    assert_eq!(c.frames(), 60, "only the first 2 seconds count");
    let cal = c.finish().unwrap();
    let (y, p, _) = cal.rotation.to_yaw_pitch_roll();
    assert!(close(y, 15.0, 0.05) && close(p, 5.0, 0.05), "{y} {p}");
    assert!(close(cal.weights.get(Blendshape::EyeBlinkLeft), 0.1, 1e-5));

    // At the neutral pose the head reads straight ahead and the weights read 0.
    let at_neutral = cal.apply(&frame(3.0, 15.0, 0.1, 0.05));
    assert!(at_neutral.rotation.angle_to(Quat::IDENTITY) < 0.01);
    assert!(close(at_neutral.translation.length(), 0.0, 1e-6));
    assert_eq!(at_neutral.weights.get(Blendshape::EyeBlinkLeft), 0.0);
    assert_eq!(at_neutral.weights.get(Blendshape::MouthSmileLeft), 0.0);

    // Turning 10 degrees further reads as 10 degrees; a weight is spread from its neutral to 1.
    let turned = cal.apply(&frame(3.0, 25.0, 0.55, 1.0));
    let (y, p, r) = turned.rotation.to_yaw_pitch_roll();
    assert!(close(y, 10.0, 0.05) && close(p, 0.0, 0.05) && close(r, 0.0, 0.05), "{y} {p} {r}");
    assert!(close(turned.weights.get(Blendshape::EyeBlinkLeft), 0.5, 1e-4));
    assert_eq!(turned.weights.get(Blendshape::JawOpen), 1.0);
    let lost = TrackFrame { present: false, ..frame(3.0, 25.0, 0.5, 1.0) };
    assert_eq!(cal.apply(&lost), lost);
}

#[test]
fn a_neutral_with_closed_eyes_or_an_open_mouth_is_refused() {
    let mut c = Calibrator::new(2.0).unwrap();
    collect(&mut c, |t| frame(t, 0.0, 0.6, 0.0));
    assert_eq!(c.finish(), Err(CalibrationError::EyesClosed));
    // One eye shut and the other open is refused too.
    let mut c = Calibrator::new(2.0).unwrap();
    collect(&mut c, |t| {
        let mut f = frame(t, 0.0, 0.0, 0.0);
        f.weights.set(Blendshape::EyeBlinkLeft, 1.0);
        f
    });
    assert_eq!(c.finish(), Err(CalibrationError::EyesClosed));
    let mut c = Calibrator::new(2.0).unwrap();
    collect(&mut c, |t| frame(t, 0.0, 0.0, 0.35));
    assert_eq!(c.finish(), Err(CalibrationError::MouthOpen));
    assert_eq!(CalibrationError::MouthOpen.to_string(), "Look at the camera with your eyes open and mouth closed, then try again.");
    // Just inside both limits is accepted.
    let mut c = Calibrator::new(2.0).unwrap();
    collect(&mut c, |t| frame(t, 0.0, MAX_NEUTRAL_BLINK, MAX_NEUTRAL_JAW));
    assert!(c.finish().is_ok());
}

#[test]
fn too_few_faces_or_a_bad_time_is_refused() {
    let mut c = Calibrator::new(2.0).unwrap();
    for i in 0..60 {
        let mut f = frame(f64::from(i) / 30.0, 0.0, 0.0, 0.0);
        f.present = i % 15 == 0;
        c.push(&f);
    }
    assert_eq!(c.frames(), 4);
    assert_eq!(c.finish(), Err(CalibrationError::NoFace));
    assert_eq!(Calibrator::new(0.5).err(), Some(CalibrationError::BadTime));
    assert_eq!(Calibrator::new(f64::NAN).err(), Some(CalibrationError::BadTime));
    assert!(Calibrator::new(5.0).is_ok());
}

#[test]
fn the_calibration_time_starts_at_the_first_frame_even_without_a_face() {
    let mut c = Calibrator::new(1.0).unwrap();
    for i in 0..40 {
        c.push(&TrackFrame { time: f64::from(i) / 30.0, ..TrackFrame::default() });
    }
    assert!(c.done(40.0 / 30.0));
    assert_eq!(c.frames(), 0);
    assert_eq!(c.finish(), Err(CalibrationError::NoFace));
    // Frames with a face that arrive after the time has run out are not counted.
    c.push(&frame(1.5, 0.0, 0.0, 0.0));
    assert_eq!(c.frames(), 0);
}

#[test]
fn a_neutral_near_full_weight_still_reaches_full_weight() {
    let mut cal = Calibration::default();
    cal.weights.set(Blendshape::MouthSmileLeft, 0.98);
    cal.weights.set(Blendshape::MouthSmileRight, 1.0);
    assert!(cal.is_valid());
    let mut f = TrackFrame { present: true, ..TrackFrame::default() };
    f.weights.set(Blendshape::MouthSmileLeft, 1.0);
    f.weights.set(Blendshape::MouthSmileRight, 1.0);
    let full = cal.apply(&f);
    assert_eq!(full.weights.get(Blendshape::MouthSmileLeft), 1.0);
    // A neutral already at 1 leaves nothing above it.
    assert_eq!(full.weights.get(Blendshape::MouthSmileRight), 0.0);
    f.weights.set(Blendshape::MouthSmileLeft, 0.99);
    assert!(close(cal.apply(&f).weights.get(Blendshape::MouthSmileLeft), 0.5, 1e-3));
}

#[test]
fn opposite_signs_of_one_rotation_average_to_it() {
    let mut c = Calibrator::new(1.0).unwrap();
    let q = Quat::from_yaw_pitch_roll(30.0, 0.0, 0.0);
    for i in 0..10 {
        let mut f = frame(f64::from(i) * 0.05, 0.0, 0.0, 0.0);
        f.rotation = if i % 2 == 0 { q } else { Quat::new(-q.x, -q.y, -q.z, -q.w) };
        c.push(&f);
    }
    assert!(c.finish().unwrap().rotation.angle_to(q) < 0.01);
}

#[test]
fn a_calibration_reads_back_what_it_writes() {
    let mut c = Calibrator::new(1.0).unwrap();
    collect(&mut c, |t| frame(t, 12.0, 0.2, 0.1));
    let cal = c.finish().unwrap();
    let back: Calibration = serde_json::from_str(&serde_json::to_string(&cal).unwrap()).unwrap();
    assert_eq!(back, cal);
    assert!(back.is_valid());
    let broken = Calibration { rotation: Quat::new(f32::NAN, 0.0, 0.0, 1.0), ..cal };
    assert!(!broken.is_valid());
    let zero = Calibration { rotation: Quat::new(0.0, 0.0, 0.0, 0.0), ..cal };
    assert!(!zero.is_valid());
    let long = Calibration { rotation: Quat::new(0.0, 0.0, 0.0, 2.0), ..cal };
    assert!(!long.is_valid());
}
