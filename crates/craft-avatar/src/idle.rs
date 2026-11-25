//! Idle motion: breathing, blinking and a gentle sway of the head, for while no face is tracked.
//!
//! The rates and sizes are StudioCraft's own values. Blinks come at intervals of 2 to 6 seconds
//! from a seeded generator, so a given seed and clock always give the same motion.

use serde::{Deserialize, Serialize};

/// One breath, in seconds.
pub const BREATH_SECONDS: f64 = 4.0;
/// How long a blink lasts, in seconds: closing for the first 40 %, opening for the rest.
pub const BLINK_SECONDS: f64 = 0.15;
/// The shortest and longest time between blinks, in seconds.
pub const BLINK_INTERVAL: (f64, f64) = (2.0, 6.0);
/// The sway's largest yaw, pitch and roll, in degrees, and their periods in seconds.
pub const SWAY_DEGREES: [f32; 3] = [3.0, 1.5, 2.0];
pub const SWAY_SECONDS: [f64; 3] = [7.3, 9.1, 11.3];

/// Which idle motions are on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct IdleMotion {
    pub breathing: bool,
    pub blinking: bool,
    pub sway: bool,
}

impl Default for IdleMotion {
    fn default() -> IdleMotion {
        IdleMotion { breathing: true, blinking: true, sway: true }
    }
}

/// The idle pose at one moment.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct IdlePose {
    /// 0 (breathed out) to 1 (breathed in).
    pub breath: f32,
    /// 0 (open) to 1 (closed).
    pub blink: f32,
    /// Head yaw, pitch and roll, in degrees.
    pub yaw: f32,
    pub pitch: f32,
    pub roll: f32,
}

/// The idle motion generator.
#[derive(Clone, Debug)]
pub struct Idle {
    motion: IdleMotion,
    rng: u64,
    next_blink: Option<f64>,
}

impl Idle {
    /// A generator with the motions in `motion`, blinking at intervals drawn from `seed`.
    pub fn new(motion: IdleMotion, seed: u64) -> Idle {
        // A zero state would stay zero.
        Idle { motion, rng: seed ^ 0x9E37_79B9_7F4A_7C15, next_blink: None }
    }

    fn interval(&mut self) -> f64 {
        // xorshift64*.
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        let r = (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64;
        BLINK_INTERVAL.0 + r * (BLINK_INTERVAL.1 - BLINK_INTERVAL.0)
    }

    /// The pose at `now` seconds. A time that is not finite gives the rest pose.
    pub fn step(&mut self, now: f64) -> IdlePose {
        let mut pose = IdlePose::default();
        if !now.is_finite() {
            return pose;
        }
        let tau = std::f64::consts::TAU;
        if self.motion.breathing {
            pose.breath = (0.5 - 0.5 * (tau * now.rem_euclid(BREATH_SECONDS) / BREATH_SECONDS).cos()) as f32;
        }
        if self.motion.sway {
            let s = |i: usize, phase: f64| SWAY_DEGREES[i] * (tau * now.rem_euclid(SWAY_SECONDS[i]) / SWAY_SECONDS[i] + phase).sin() as f32;
            pose.yaw = s(0, 0.0);
            pose.pitch = s(1, 1.3);
            pose.roll = s(2, 2.1);
        }
        if self.motion.blinking {
            pose.blink = self.blink(now);
        }
        pose
    }

    fn blink(&mut self, now: f64) -> f32 {
        let mut start = match self.next_blink {
            Some(t) => t,
            None => {
                let t = now + self.interval();
                self.next_blink = Some(t);
                t
            }
        };
        // A clock that jumped far ahead or back starts the blinks over from now.
        if now - start > 60.0 || start - now > BLINK_INTERVAL.1 {
            start = now + self.interval();
        }
        // At most 60 s behind, so 32 blinks catch up. A time so large that adding a few seconds
        // does not change it stops here instead of looping forever.
        let mut steps = 0;
        while now >= start + BLINK_SECONDS {
            if steps == 32 {
                start = now + self.interval();
                break;
            }
            start += BLINK_SECONDS + self.interval();
            steps += 1;
        }
        self.next_blink = Some(start);
        let t = now - start;
        if t < 0.0 {
            return 0.0;
        }
        let close = BLINK_SECONDS * 0.4;
        let v = if t < close { t / close } else { 1.0 - (t - close) / (BLINK_SECONDS - close) };
        v.clamp(0.0, 1.0) as f32
    }
}
