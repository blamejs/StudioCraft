//! Gaze: turning a direction of sight (yaw and pitch) into eye bone rotations or into the
//! `lookUp`, `lookDown`, `lookLeft` and `lookRight` expression weights.
//!
//! The range maps and the choice of map for each eye and direction follow the published VRM 1.0
//! look-at specification
//! (https://github.com/vrm-c/vrm-specification/blob/master/specification/VRMC_vrm-1.0/lookAt.md).
//! A positive yaw looks toward the avatar's left (+X) and a positive pitch looks down.

use crate::AvatarError;
use crate::math::{Quat, Vec3};

/// Scales the size of a yaw or pitch into a bone angle or an expression weight.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RangeMap {
    /// The yaw or pitch, in degrees, at which the output reaches `output_scale`.
    pub input_max: f32,
    /// The largest bone angle in degrees, or the largest expression weight.
    pub output_scale: f32,
}

impl RangeMap {
    pub const fn new(input_max: f32, output_scale: f32) -> RangeMap {
        RangeMap { input_max, output_scale }
    }

    /// The output for the size of `value`, in degrees: `min(|value|, input_max) / input_max ·
    /// output_scale`. With `input_max` at 0, any value other than 0 gives `output_scale`. A
    /// value that is not a number gives 0.
    pub fn map(&self, value: f32) -> f32 {
        let v = value.abs();
        if v.is_nan() {
            return 0.0;
        }
        if self.input_max <= 0.0 {
            return if v > 0.0 { self.output_scale } else { 0.0 };
        }
        v.min(self.input_max) / self.input_max * self.output_scale
    }
}

/// What the gaze moves.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LookAtKind {
    /// The `leftEye` and `rightEye` bones turn.
    #[default]
    Bone,
    /// The four look expressions are weighted.
    Expression,
}

/// The gaze settings of an avatar.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LookAt {
    pub kind: LookAtKind,
    /// The point between the eyes, relative to the head bone.
    pub offset_from_head: Vec3,
    /// Toward the nose.
    pub horizontal_inner: RangeMap,
    /// Away from the nose; the look-left and look-right expressions use it too.
    pub horizontal_outer: RangeMap,
    pub vertical_down: RangeMap,
    pub vertical_up: RangeMap,
}

impl Default for LookAt {
    /// Bones, 10 degrees of eye turn at 90 degrees of gaze, the eyes 6 cm above the head bone.
    fn default() -> LookAt {
        let m = RangeMap::new(90.0, 10.0);
        LookAt {
            kind: LookAtKind::Bone,
            offset_from_head: Vec3::new(0.0, 0.06, 0.0),
            horizontal_inner: m,
            horizontal_outer: m,
            vertical_down: m,
            vertical_up: m,
        }
    }
}

/// The look expression weights.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LookWeights {
    pub up: f32,
    pub down: f32,
    pub left: f32,
    pub right: f32,
}

impl LookAt {
    pub(crate) fn validate(&self) -> Result<(), AvatarError> {
        let maps = [self.horizontal_inner, self.horizontal_outer, self.vertical_down, self.vertical_up];
        let bad = |m: &RangeMap| !m.input_max.is_finite() || !m.output_scale.is_finite() || m.input_max < 0.0 || m.output_scale < 0.0;
        if !self.offset_from_head.is_finite() || maps.iter().any(bad) {
            return Err(AvatarError::Invalid("the look-at settings have a value that is negative or not finite".into()));
        }
        Ok(())
    }

    /// The local rotations of the left and right eye bones for a gaze of `yaw` and `pitch`
    /// degrees, each made as yaw about +Y then pitch about +X.
    ///
    /// Looking left uses the outer map for the left eye and the inner map for the right eye;
    /// looking right is the other way around. Both eyes use the down map below the horizon and
    /// the up map above it.
    pub fn eye_rotations(&self, yaw: f32, pitch: f32) -> (Quat, Quat) {
        let (ly, ry) = if yaw > 0.0 {
            (self.horizontal_outer.map(yaw), self.horizontal_inner.map(yaw))
        } else {
            (-self.horizontal_inner.map(yaw), -self.horizontal_outer.map(yaw))
        };
        let p = if pitch > 0.0 { self.vertical_down.map(pitch) } else { -self.vertical_up.map(pitch) };
        (Quat::from_yaw_pitch_roll(ly, p, 0.0), Quat::from_yaw_pitch_roll(ry, p, 0.0))
    }

    /// The look expression weights for a gaze of `yaw` and `pitch` degrees, each within 0 to 1.
    pub fn expression_weights(&self, yaw: f32, pitch: f32) -> LookWeights {
        let h = crate::vocab::clamp01(self.horizontal_outer.map(yaw));
        let mut w = LookWeights::default();
        if yaw > 0.0 {
            w.left = h;
        } else if yaw < 0.0 {
            w.right = h;
        }
        if pitch > 0.0 {
            w.down = crate::vocab::clamp01(self.vertical_down.map(pitch));
        } else if pitch < 0.0 {
            w.up = crate::vocab::clamp01(self.vertical_up.map(pitch));
        }
        w
    }
}
