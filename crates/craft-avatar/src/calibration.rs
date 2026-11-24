//! Calibration: the user's relaxed face, looking at the camera, taken as the neutral pose.
//!
//! A [`Calibrator`] averages the head rotation, the head position and every shape weight over
//! the frames it is given. [`Calibrator::finish`] refuses a neutral with an eye closed or the
//! mouth open. [`Calibration::apply`] then measures each frame from the neutral: the head's
//! rotation and position relative to it, and each weight spread from its neutral value to 1.

use serde::{Deserialize, Serialize};

use crate::math::{Quat, Vec3};
use crate::vocab::{Blendshape, TrackFrame, Weights, clamp01};

/// Above this average `eyeBlink` weight the neutral is refused.
pub const MAX_NEUTRAL_BLINK: f32 = 0.5;
/// Above this average `jawOpen` weight the neutral is refused.
pub const MAX_NEUTRAL_JAW: f32 = 0.3;
/// Fewest frames with a face a calibration needs.
pub const MIN_FRAMES: u32 = 5;
/// The default calibration time, and the range allowed, in seconds.
pub const DEFAULT_SECONDS: f64 = 2.0;
pub const SECONDS: (f64, f64) = (1.0, 5.0);

/// Why a calibration was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CalibrationError {
    #[error("No face was found. Look at the camera, then try again.")]
    NoFace,
    #[error("Look at the camera with your eyes open and mouth closed, then try again.")]
    EyesClosed,
    #[error("Look at the camera with your eyes open and mouth closed, then try again.")]
    MouthOpen,
    #[error("The calibration time must be 1 to 5 seconds.")]
    BadTime,
}

/// The neutral pose.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Calibration {
    pub rotation: Quat,
    pub translation: Vec3,
    pub weights: Weights,
}

impl Default for Calibration {
    /// No calibration: frames pass through unchanged.
    fn default() -> Calibration {
        Calibration { rotation: Quat::IDENTITY, translation: Vec3::ZERO, weights: Weights::default() }
    }
}

impl Calibration {
    /// Whether the values are finite, the rotation is of unit length and the weights are within
    /// 0 to 1 (a saved calibration is checked with this before use).
    pub fn is_valid(&self) -> bool {
        self.rotation.is_unit() && self.translation.is_finite() && self.weights.0.iter().all(|w| (0.0..=1.0).contains(w))
    }

    /// `f` measured from the neutral pose. The rotation is the one that turns the neutral head
    /// into the frame's head, the translation is the offset from the neutral position, and a
    /// weight `w` with neutral value `n` becomes `(w − n) / (1 − n)`, within 0 to 1. A frame
    /// without a face is returned unchanged.
    pub fn apply(&self, f: &TrackFrame) -> TrackFrame {
        if !f.present {
            return *f;
        }
        let f = f.sanitized();
        let mut weights = Weights::default();
        for (i, out) in weights.0.iter_mut().enumerate() {
            let (w, n) = (f.weights.0[i], self.weights.0[i]);
            // `w` is at most 1, so `n` is below 1 whenever `w` is above it.
            *out = if w > n { clamp01((w - n) / (1.0 - n)) } else { 0.0 };
        }
        TrackFrame {
            time: f.time,
            present: true,
            weights,
            rotation: (f.rotation * self.rotation.conjugate()).normalized(),
            translation: f.translation - self.translation,
        }
    }
}

/// Collects frames for a calibration.
#[derive(Clone, Debug)]
pub struct Calibrator {
    seconds: f64,
    start: Option<f64>,
    frames: u32,
    rotation: [f64; 4],
    first: Quat,
    translation: [f64; 3],
    weights: [f64; 52],
}

impl Calibrator {
    /// A calibrator that collects frames for `seconds` (1 to 5) from the first frame it gets.
    pub fn new(seconds: f64) -> Result<Calibrator, CalibrationError> {
        if !(SECONDS.0..=SECONDS.1).contains(&seconds) {
            return Err(CalibrationError::BadTime);
        }
        Ok(Calibrator { seconds, start: None, frames: 0, rotation: [0.0; 4], first: Quat::IDENTITY, translation: [0.0; 3], weights: [0.0; 52] })
    }

    /// Adds a frame. The calibration time starts at the first frame with a finite time, with a
    /// face or without one. Frames without a face, and frames after the calibration time, are
    /// not counted. Rotations are averaged on the side of the first frame's rotation, so `q` and
    /// `−q` (the same rotation) add up.
    pub fn push(&mut self, f: &TrackFrame) {
        if !f.time.is_finite() {
            return;
        }
        let start = *self.start.get_or_insert(f.time);
        if !f.present || f.time < start || self.done(f.time) {
            return;
        }
        let f = f.sanitized();
        if self.frames == 0 {
            self.first = f.rotation;
        }
        let q = if f.rotation.dot(self.first) < 0.0 { Quat::new(-f.rotation.x, -f.rotation.y, -f.rotation.z, -f.rotation.w) } else { f.rotation };
        for (acc, v) in self.rotation.iter_mut().zip([q.x, q.y, q.z, q.w]) {
            *acc += f64::from(v);
        }
        for (acc, v) in self.translation.iter_mut().zip(f.translation.to_array()) {
            *acc += f64::from(v);
        }
        for (acc, v) in self.weights.iter_mut().zip(f.weights.0) {
            *acc += f64::from(v);
        }
        self.frames += 1;
    }

    /// Whether the calibration time has passed at `now`.
    pub fn done(&self, now: f64) -> bool {
        self.start.is_some_and(|s| now - s >= self.seconds)
    }

    /// Frames counted so far.
    pub fn frames(&self) -> u32 {
        self.frames
    }

    /// The neutral pose from the frames collected. Refused with fewer than [`MIN_FRAMES`]
    /// frames, with an eye closed (either eye's average `eyeBlink` above [`MAX_NEUTRAL_BLINK`]) or with the
    /// mouth open (average `jawOpen` above [`MAX_NEUTRAL_JAW`]).
    pub fn finish(&self) -> Result<Calibration, CalibrationError> {
        if self.frames < MIN_FRAMES {
            return Err(CalibrationError::NoFace);
        }
        let n = f64::from(self.frames);
        let mut weights = Weights::default();
        for (out, sum) in weights.0.iter_mut().zip(self.weights) {
            *out = clamp01((sum / n) as f32);
        }
        let blink = weights.get(Blendshape::EyeBlinkLeft).max(weights.get(Blendshape::EyeBlinkRight));
        if blink > MAX_NEUTRAL_BLINK {
            return Err(CalibrationError::EyesClosed);
        }
        if weights.get(Blendshape::JawOpen) > MAX_NEUTRAL_JAW {
            return Err(CalibrationError::MouthOpen);
        }
        let [x, y, z, w] = self.rotation.map(|v| (v / n) as f32);
        let [tx, ty, tz] = self.translation.map(|v| (v / n) as f32);
        Ok(Calibration { rotation: Quat::new(x, y, z, w).normalized(), translation: Vec3::new(tx, ty, tz), weights })
    }
}
