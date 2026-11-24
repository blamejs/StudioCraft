use super::close;
use crate::*;

fn tracking() -> Presence {
    let mut p = Presence::new(LostFace::default(), &WhenLost::HoldThenIdle);
    p.step(0.0, true);
    p.step(1.0, true);
    p
}

#[test]
fn a_lost_face_holds_then_eases_to_idle() {
    let mut p = tracking();
    assert_eq!((p.factor(), p.state()), (1.0, PresenceState::Tracking));
    // Lost at 10 s: held for 0.5 s, then eased to 0 over 1 s.
    assert_eq!(p.step(10.0, false), 1.0);
    assert_eq!(p.state(), PresenceState::Holding);
    assert_eq!(p.step(10.49, false), 1.0);
    let half = p.step(11.0, false);
    assert!(close(half, 0.5, 1e-6), "{half}");
    assert_eq!(p.state(), PresenceState::Lost);
    let late = p.step(11.4, false);
    assert!(late > 0.0 && late < half);
    assert_eq!(p.step(11.5, false), 0.0);
    assert_eq!(p.step(30.0, false), 0.0);
    assert_eq!(p.state(), PresenceState::Lost);
}

#[test]
fn a_found_face_eases_back() {
    let mut p = tracking();
    p.step(10.0, false);
    p.step(12.0, false);
    assert_eq!(p.factor(), 0.0);
    assert_eq!(p.step(20.0, true), 0.0);
    assert_eq!(p.state(), PresenceState::Returning);
    assert!(close(p.step(20.125, true), 0.5, 1e-6));
    assert_eq!(p.step(20.25, true), 1.0);
    assert_eq!(p.state(), PresenceState::Tracking);
}

#[test]
fn a_face_found_after_the_hold_ran_out_does_not_depend_on_the_steps_between() {
    // Lost at 10 s: the hold ends at 10.5 and the fade at 11.5. A face found at 12 starts from
    // the idle pose whether or not the caller stepped while it was lost.
    let mut direct = tracking();
    direct.step(10.0, false);
    let mut stepped = tracking();
    stepped.step(10.0, false);
    stepped.step(12.0, false);
    assert_eq!(direct.step(12.0, true), stepped.step(12.0, true));
    assert_eq!(direct.factor(), 0.0);
    assert_eq!(direct.state(), PresenceState::Returning);
    // Found during the fade: it returns from the faded value.
    let mut mid = tracking();
    mid.step(10.0, false);
    let found = mid.step(11.0, true);
    assert!(close(found, 0.5, 1e-6), "{found}");
    // Found within the hold: still fully tracked.
    let mut held = tracking();
    held.step(10.0, false);
    assert_eq!(held.step(10.3, true), 1.0);
    assert_eq!(held.state(), PresenceState::Tracking);
}

#[test]
fn a_face_found_during_the_fade_returns_from_where_it_was() {
    let mut p = tracking();
    p.step(10.0, false);
    let mid = p.step(11.0, false);
    assert_eq!(p.step(11.0, true), mid);
    let up = p.step(11.125, true);
    assert!(up > mid && up < 1.0);
    // Lost again while returning: the pose holds where it is.
    let held = p.step(11.2, false);
    assert_eq!(p.state(), PresenceState::Holding);
    assert_eq!(p.step(11.6, false), held);
    assert_eq!(p.step(20.0, false), 0.0);
}

#[test]
fn one_long_step_goes_straight_to_idle() {
    let mut p = tracking();
    p.step(10.0, false);
    assert_eq!(p.step(100.0, false), 0.0);
    assert_eq!(p.state(), PresenceState::Lost);
}

#[test]
fn hold_last_keeps_the_pose() {
    let mut p = Presence::new(LostFace::default(), &WhenLost::HoldLast);
    p.step(0.0, true);
    p.step(1.0, true);
    p.step(2.0, false);
    assert_eq!(p.step(600.0, false), 1.0);
    assert_eq!(p.state(), PresenceState::Holding);
    assert_eq!(p.step(601.0, true), 1.0);
    assert_eq!(p.state(), PresenceState::Tracking);
}

#[test]
fn the_clock_never_runs_backward() {
    let mut p = tracking();
    p.step(10.0, false);
    let a = p.step(11.0, false);
    assert_eq!(p.step(5.0, false), a);
    assert_eq!(p.step(f64::NAN, false), a);
    assert_eq!(p.step(f64::INFINITY, false), a);
    assert!(p.step(11.2, false) < a);
}

#[test]
fn zero_times_switch_at_once() {
    let timing = LostFace { hold: 0.0, fade_out: 0.0, fade_in: 0.0 };
    let mut p = Presence::new(timing, &WhenLost::HoldThenIdle);
    assert_eq!(p.step(0.0, true), 1.0);
    assert_eq!(p.step(0.0, false), 0.0);
    assert_eq!(p.step(0.0, true), 1.0);
}

#[test]
fn timings_out_of_range_are_refused() {
    assert!(LostFace::default().validate().is_ok());
    for bad in [
        LostFace { hold: -0.1, ..LostFace::default() },
        LostFace { fade_out: 10.5, ..LostFace::default() },
        LostFace { fade_in: f64::NAN, ..LostFace::default() },
    ] {
        assert!(bad.validate().is_err(), "{bad:?}");
    }
    let s: WhenLost = serde_json::from_str(r#"{"mode":"expression","name":"sad"}"#).unwrap();
    assert_eq!(s, WhenLost::Expression { name: ExpressionName::Preset(Preset::Sad) });
    assert_eq!(serde_json::to_string(&WhenLost::HoldLast).unwrap(), r#"{"mode":"holdLast"}"#);
}
