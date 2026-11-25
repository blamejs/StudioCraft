use crate::*;

/// The blinks in the first `seconds`, sampled at 1 ms: (start, length) of each.
fn blinks(idle: &mut Idle, seconds: f64) -> Vec<(f64, f64)> {
    let mut out = Vec::new();
    let mut start = None;
    for i in 0..(seconds * 1000.0) as i64 {
        let t = i as f64 / 1000.0;
        let closed = idle.step(t).blink > 0.0;
        match (closed, start) {
            (true, None) => start = Some(t),
            (false, Some(s)) => {
                out.push((s, t - s));
                start = None;
            }
            _ => {}
        }
    }
    out
}

#[test]
fn blinks_come_every_2_to_6_seconds_and_last_150_ms() {
    let mut idle = Idle::new(IdleMotion::default(), 7);
    let b = blinks(&mut idle, 60.0);
    assert!(b.len() >= 8, "{} blinks in a minute", b.len());
    for (_, len) in &b {
        assert!((len - BLINK_SECONDS).abs() <= 0.002, "a blink lasted {len}");
    }
    for pair in b.windows(2) {
        let gap = pair[1].0 - pair[0].0 - BLINK_SECONDS;
        assert!(gap >= BLINK_INTERVAL.0 - 0.002 && gap <= BLINK_INTERVAL.1 + 0.002, "gap {gap}");
    }
    assert!(b[0].0 <= BLINK_INTERVAL.1);
    // The eye closes fully partway through.
    let mut idle = Idle::new(IdleMotion::default(), 7);
    let peak = (0..60_000).map(|i| idle.step(f64::from(i) / 1000.0).blink).fold(0.0f32, f32::max);
    assert!(peak > 0.95);
}

#[test]
fn the_same_seed_gives_the_same_motion() {
    let (mut a, mut b, mut c) = (Idle::new(IdleMotion::default(), 1), Idle::new(IdleMotion::default(), 1), Idle::new(IdleMotion::default(), 2));
    let (ba, bb, bc) = (blinks(&mut a, 30.0), blinks(&mut b, 30.0), blinks(&mut c, 30.0));
    assert_eq!(ba, bb);
    assert_ne!(ba, bc);
}

#[test]
fn breathing_and_sway_stay_in_their_ranges() {
    let mut idle = Idle::new(IdleMotion::default(), 3);
    let (mut lo, mut hi) = (1.0f32, 0.0f32);
    for i in 0..2_000 {
        let p = idle.step(f64::from(i) / 100.0);
        lo = lo.min(p.breath);
        hi = hi.max(p.breath);
        assert!(p.yaw.abs() <= SWAY_DEGREES[0] && p.pitch.abs() <= SWAY_DEGREES[1] && p.roll.abs() <= SWAY_DEGREES[2]);
    }
    assert!(lo < 0.01 && hi > 0.99);
    assert!((idle.step(BREATH_SECONDS / 2.0).breath - 1.0).abs() < 1e-6);
}

#[test]
fn motions_that_are_off_stay_still() {
    let mut idle = Idle::new(IdleMotion { breathing: false, blinking: false, sway: false }, 3);
    for i in 0..10_000 {
        assert_eq!(idle.step(f64::from(i) / 100.0), IdlePose::default());
    }
    let mut idle = Idle::new(IdleMotion { breathing: true, blinking: false, sway: false }, 3);
    let p = idle.step(1.0);
    assert!(p.breath > 0.0 && p.blink == 0.0 && p.yaw == 0.0);
    assert_eq!(Idle::new(IdleMotion::default(), 3).step(f64::NAN), IdlePose::default());
}

#[test]
fn a_time_too_large_to_advance_returns() {
    let mut idle = Idle::new(IdleMotion::default(), 9);
    for now in [1e17, 1e17, 1e300, f64::MAX] {
        let p = idle.step(now);
        assert!((0.0..=1.0).contains(&p.blink) && p.yaw.is_finite(), "{now}: {p:?}");
    }
}

#[test]
fn a_clock_jump_starts_the_blinks_over() {
    let mut idle = Idle::new(IdleMotion::default(), 9);
    idle.step(0.0);
    // Far ahead, and back again: the next blink still comes within 6 seconds.
    for start in [10_000.0, 5.0] {
        let mut seen = false;
        for i in 0..6_200 {
            if idle.step(start + f64::from(i) / 1000.0).blink > 0.0 {
                seen = true;
                break;
            }
        }
        assert!(seen, "no blink after a jump to {start}");
    }
}
