//! One Euro filters (G. Casiez, N. Roussel and D. Vogel, "1 € Filter: A Simple Speed-based
//! Low-pass Filter for Noisy Input in Interactive Systems", CHI 2012): a low-pass filter whose
//! cutoff rises with the signal's speed, so a still face holds still and a moving one keeps up.
//!
//! Times are seconds from the caller's clock. A sample at the same time as the previous one, or
//! earlier, returns the previous output; a sample that is not a number is skipped.

use std::f64::consts::PI;

/// The settings of a One Euro filter.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OneEuroParams {
    /// The cutoff frequency (Hz) of a still signal: lower is smoother and lags more.
    pub min_cutoff: f64,
    /// How fast the cutoff rises with speed (per unit of the signal per second).
    pub beta: f64,
    /// The cutoff frequency (Hz) of the speed estimate.
    pub d_cutoff: f64,
}

impl OneEuroParams {
    /// No smoothing: the filter returns its input.
    pub const OFF: OneEuroParams = OneEuroParams { min_cutoff: f64::INFINITY, beta: 0.0, d_cutoff: 1.0 };
}

/// The smoothing factor of a low-pass step of `dt` seconds at `cutoff` Hz.
fn alpha(cutoff: f64, dt: f64) -> f64 {
    if cutoff.is_infinite() {
        return 1.0;
    }
    let tau = 1.0 / (2.0 * PI * cutoff.max(1e-6));
    1.0 / (1.0 + tau / dt)
}

/// A One Euro filter of one value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OneEuro {
    params: OneEuroParams,
    last: Option<(f64, f64, f64)>,
}

impl OneEuro {
    pub fn new(params: OneEuroParams) -> OneEuro {
        OneEuro { params, last: None }
    }

    /// Forget the past: the next sample passes as it is.
    pub fn reset(&mut self) {
        self.last = None;
    }

    pub fn set_params(&mut self, params: OneEuroParams) {
        self.params = params;
    }

    /// The filtered value of `x` at `time`.
    pub fn filter(&mut self, x: f64, time: f64) -> f64 {
        let Some((t, prev, dprev)) = self.last else {
            if x.is_finite() && time.is_finite() {
                self.last = Some((time, x, 0.0));
            }
            return x;
        };
        let dt = time - t;
        if !x.is_finite() || !dt.is_finite() || dt <= 0.0 {
            return prev;
        }
        let p = self.params;
        let dx = (x - prev) / dt;
        let d = dprev + alpha(p.d_cutoff, dt) * (dx - dprev);
        let cutoff = p.min_cutoff + p.beta * d.abs();
        let y = prev + alpha(cutoff, dt) * (x - prev);
        self.last = Some((time, y, d));
        y
    }
}

type Quat = [f64; 4];

fn qmul(a: Quat, b: Quat) -> Quat {
    [
        a[0] * b[0] - a[1] * b[1] - a[2] * b[2] - a[3] * b[3],
        a[0] * b[1] + a[1] * b[0] + a[2] * b[3] - a[3] * b[2],
        a[0] * b[2] - a[1] * b[3] + a[2] * b[0] + a[3] * b[1],
        a[0] * b[3] + a[1] * b[2] - a[2] * b[1] + a[3] * b[0],
    ]
}

fn conj(q: Quat) -> Quat {
    [q[0], -q[1], -q[2], -q[3]]
}

fn normalized(q: Quat) -> Option<Quat> {
    let n = q.iter().map(|v| v * v).sum::<f64>().sqrt();
    (n.is_finite() && n > 1e-12).then(|| q.map(|v| v / n))
}

/// The rotation vector (axis times angle, radians) of a unit quaternion.
fn log(q: Quat) -> [f64; 3] {
    let q = if q[0] < 0.0 { q.map(|v| -v) } else { q };
    let s = (q[1] * q[1] + q[2] * q[2] + q[3] * q[3]).sqrt();
    if s < 1e-12 {
        return [2.0 * q[1], 2.0 * q[2], 2.0 * q[3]];
    }
    let angle = 2.0 * s.atan2(q[0]);
    [q[1] / s * angle, q[2] / s * angle, q[3] / s * angle]
}

/// The unit quaternion of a rotation vector.
fn exp(v: [f64; 3]) -> Quat {
    let angle = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    if angle < 1e-12 {
        return [1.0, v[0] / 2.0, v[1] / 2.0, v[2] / 2.0];
    }
    let (s, c) = (angle / 2.0).sin_cos();
    [c, v[0] / angle * s, v[1] / angle * s, v[2] / angle * s]
}

/// A One Euro filter of a rotation (unit quaternion `[w, x, y, z]`). Each step moves the output
/// toward the input along the shortest arc, by the same factor a [`OneEuro`] would use on the
/// angle between them; the speed is the angular speed in radians per second.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RotationFilter {
    params: OneEuroParams,
    last: Option<(f64, Quat, f64)>,
}

impl RotationFilter {
    pub fn new(params: OneEuroParams) -> RotationFilter {
        RotationFilter { params, last: None }
    }

    pub fn reset(&mut self) {
        self.last = None;
    }

    pub fn set_params(&mut self, params: OneEuroParams) {
        self.params = params;
    }

    /// The filtered rotation of `q` at `time`.
    pub fn filter(&mut self, q: [f32; 4], time: f64) -> [f32; 4] {
        let Some(q) = normalized(q.map(f64::from)) else { return self.last.map_or(q, |l| l.1.map(|v| v as f32)) };
        let Some((t, prev, dprev)) = self.last else {
            if time.is_finite() {
                self.last = Some((time, q, 0.0));
            }
            return q.map(|v| v as f32);
        };
        let dt = time - t;
        if !dt.is_finite() || dt <= 0.0 {
            return prev.map(|v| v as f32);
        }
        let delta = log(qmul(conj(prev), q));
        let angle = (delta[0] * delta[0] + delta[1] * delta[1] + delta[2] * delta[2]).sqrt();
        let p = self.params;
        let d = dprev + alpha(p.d_cutoff, dt) * (angle / dt - dprev);
        let a = alpha(p.min_cutoff + p.beta * d.abs(), dt);
        let y = normalized(qmul(prev, exp(delta.map(|v| v * a)))).unwrap_or(q);
        self.last = Some((time, y, d));
        y.map(|v| v as f32)
    }
}

/// How much a tracker smooths what it reports.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Smoothing {
    /// No smoothing.
    Off,
    /// Little lag, some jitter.
    Low,
    #[default]
    Medium,
    /// Steady, more lag.
    High,
}

impl Smoothing {
    /// The filter for mesh points (pixels).
    pub fn points(self) -> OneEuroParams {
        self.pick([4.0, 0.08], [2.0, 0.04], [1.0, 0.02])
    }

    /// The filter for the head's rotation (radians).
    pub fn rotation(self) -> OneEuroParams {
        self.pick([4.0, 1.0], [2.0, 0.6], [1.0, 0.4])
    }

    /// The filter for the head's position (pixels) and scale.
    pub fn position(self) -> OneEuroParams {
        self.points()
    }

    /// The filter for expression weights (0 to 1).
    pub fn expressions(self) -> OneEuroParams {
        self.pick([6.0, 2.0], [3.0, 1.0], [1.5, 0.5])
    }

    fn pick(self, low: [f64; 2], medium: [f64; 2], high: [f64; 2]) -> OneEuroParams {
        let [min_cutoff, beta] = match self {
            Smoothing::Off => return OneEuroParams::OFF,
            Smoothing::Low => low,
            Smoothing::Medium => medium,
            Smoothing::High => high,
        };
        OneEuroParams { min_cutoff, beta, d_cutoff: 1.0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: OneEuroParams = OneEuroParams { min_cutoff: 1.0, beta: 0.0, d_cutoff: 1.0 };

    #[test]
    fn a_still_signal_settles_and_a_step_is_followed() {
        let mut f = OneEuro::new(P);
        assert_eq!(f.filter(5.0, 0.0), 5.0);
        let mut y = 0.0;
        for k in 1..=300 {
            y = f.filter(10.0, k as f64 / 30.0);
        }
        assert!((y - 10.0).abs() < 1e-6, "{y}");
        // One step at 30 fps with a 1 Hz cutoff moves about 17 % of the way.
        let mut g = OneEuro::new(P);
        g.filter(0.0, 0.0);
        let first = g.filter(1.0, 1.0 / 30.0);
        let want = alpha(1.0, 1.0 / 30.0);
        assert!((first - want).abs() < 1e-12 && (0.15..0.2).contains(&first), "{first}");
    }

    #[test]
    fn speed_raises_the_cutoff() {
        let fast = OneEuroParams { beta: 1.0, ..P };
        let (mut slow_f, mut fast_f) = (OneEuro::new(P), OneEuro::new(fast));
        let (mut a, mut b) = (0.0, 0.0);
        for k in 0..30 {
            let t = k as f64 / 30.0;
            a = slow_f.filter(100.0 * t, t);
            b = fast_f.filter(100.0 * t, t);
        }
        // The input is at 96.7; the filter with beta lags far less.
        let input = 100.0 * 29.0 / 30.0;
        assert!(input - b < (input - a) / 10.0, "{a} {b}");
    }

    #[test]
    fn equal_times_and_non_numbers_hold_the_last_value() {
        let mut f = OneEuro::new(P);
        f.filter(1.0, 0.0);
        let y = f.filter(2.0, 0.1);
        assert_eq!(f.filter(50.0, 0.1), y);
        assert_eq!(f.filter(50.0, 0.05), y);
        assert_eq!(f.filter(f64::NAN, 0.2), y);
        assert_eq!(f.filter(f64::INFINITY, 0.3), y);
        assert_eq!(f.filter(3.0, f64::NAN), y);
        // The held samples left the speed estimate a number: the next sample still moves the
        // output most of the way at a 1 Hz cutoff over 0.3 s.
        let z = f.filter(3.0, 0.4);
        assert!(z - y > 0.5 * (3.0 - y), "{y} {z}");
        let mut fresh = OneEuro::new(P);
        assert!(fresh.filter(f64::NAN, 0.0).is_nan());
        assert_eq!(fresh.filter(4.0, 0.0), 4.0);
        let mut off = OneEuro::new(OneEuroParams::OFF);
        off.filter(0.0, 0.0);
        assert_eq!(off.filter(7.0, 0.01), 7.0);
        f.reset();
        assert_eq!(f.filter(-9.0, 1.0), -9.0);
    }

    fn about_z(deg: f64) -> [f32; 4] {
        let (s, c) = (deg.to_radians() / 2.0).sin_cos();
        [c as f32, 0.0, 0.0, s as f32]
    }

    fn angle_z(q: [f32; 4]) -> f64 {
        2.0 * (q[3] as f64).atan2(q[0] as f64).to_degrees()
    }

    #[test]
    fn rotations_are_filtered_along_the_shortest_arc() {
        let mut f = RotationFilter::new(P);
        f.filter(about_z(0.0), 0.0);
        let y = f.filter(about_z(30.0), 1.0 / 30.0);
        let want = 30.0 * alpha(1.0, 1.0 / 30.0);
        assert!((angle_z(y) - want).abs() < 1e-3, "{} {want}", angle_z(y));
        // The same rotation written with the opposite sign is no step at all.
        let mut g = RotationFilter::new(P);
        g.filter(about_z(10.0), 0.0);
        let neg = about_z(10.0).map(|v| -v);
        let y = g.filter(neg, 0.1);
        assert!((angle_z(y) - 10.0).abs() < 1e-3 || (angle_z(y.map(|v| -v)) - 10.0).abs() < 1e-3);
        for k in 2..200 {
            g.filter(about_z(170.0), k as f64 / 30.0);
        }
        let y = g.filter(about_z(-170.0), 200.0 / 30.0);
        // From 170 degrees to -170 is 20 degrees across 180, not 340 back.
        let a = angle_z(y);
        assert!(!(-160.0..160.0).contains(&a), "{a}");
    }

    #[test]
    fn bad_rotations_and_times_hold_the_last_rotation() {
        let mut f = RotationFilter::new(P);
        let first = f.filter(about_z(20.0), 0.0);
        assert_eq!(f.filter([0.0; 4], 0.1), first);
        assert_eq!(f.filter([f32::NAN, 0.0, 0.0, 1.0], 0.2), first);
        assert_eq!(f.filter(about_z(40.0), 0.0), first);
        let y = f.filter([2.0, 0.0, 0.0, 0.0], 0.1);
        assert!((y.iter().map(|v| v * v).sum::<f32>() - 1.0).abs() < 1e-5);
    }

    #[test]
    fn smoothing_levels_order_their_cutoffs() {
        let c = [Smoothing::Low, Smoothing::Medium, Smoothing::High].map(|s| s.points().min_cutoff);
        assert!(c[0] > c[1] && c[1] > c[2]);
        assert_eq!(Smoothing::Off.rotation(), OneEuroParams::OFF);
        assert_eq!(Smoothing::default(), Smoothing::Medium);
    }
}
