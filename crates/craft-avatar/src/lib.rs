//! Avatars driven by face tracking.
//!
//! * [`Model`] is a whole avatar as plain data: a node tree with the humanoid bones mapped onto it
//!   ([`HumanBone`]), meshes with skins and morph targets, materials and textures,
//!   [`Expression`]s, the gaze settings ([`LookAt`]), [`SpringBones`] and the license terms
//!   ([`Meta`]). [`Model::validate`] checks every index, count and value against [`Limits`].
//! * [`Blendshape`], [`Weights`] and [`TrackFrame`] are the tracking vocabulary: 52 face shapes,
//!   the head's rotation and position, and whether a face was found.
//! * [`Mapping`] turns the face shapes into the avatar's expressions, and [`gaze`] turns the
//!   eye shapes into a direction of sight.
//! * [`Calibrator`] takes the user's neutral face, and [`Calibration`] measures each frame from
//!   it.
//! * [`Presence`] eases between the tracked pose and the idle pose when the face is lost and
//!   found, and [`Idle`] makes the idle breathing, blinking and sway.
//! * [`SpringSim`] swings the spring chains.
//! * [`Animator`] runs all of it once per canvas frame into an [`AvatarPose`], without
//!   allocating.
//!
//! The crate depends only on `craft-ease`, `serde` and `thiserror`, has no operating-system or
//! GPU code, and builds for `wasm32-unknown-unknown`.

mod animator;
mod calibration;
mod expression;
mod idle;
mod look_at;
mod mapping;
mod math;
mod model;
mod presence;
mod spring;
mod vocab;

pub use animator::{Animator, AnimatorSettings, AvatarPose};
pub use calibration::{Calibration, CalibrationError, Calibrator, DEFAULT_SECONDS, MAX_NEUTRAL_BLINK, MAX_NEUTRAL_JAW, MIN_FRAMES, SECONDS};
pub use expression::{
    ColorBind, ColorTarget, Expression, ExpressionName, Group, MAX_NAME_BYTES, MorphBind, Override, Preset, TextureBind, apply_overrides,
};
pub use idle::{BLINK_INTERVAL, BLINK_SECONDS, BREATH_SECONDS, Idle, IdleMotion, IdlePose, SWAY_DEGREES, SWAY_SECONDS};
pub use look_at::{LookAt, LookAtKind, LookWeights, RangeMap};
pub use mapping::{BoundMapping, Combine, Formula, GAZE_DEGREES, MAX_FORMULA_SHAPES, MAX_GAIN, MAX_RULES, Mapping, Rule, gaze};
pub use math::{Quat, Transform, Vec3};
pub use model::{
    AlphaMode, AvatarPermission, CommercialUsage, FirstPerson, HumanBone, Humanoid, Limits, Material, Mesh, Meta, Model, Modification, MorphTarget,
    Node, Primitive, Skin, Texture, Toon,
};
pub use presence::{LostFace, MAX_LOST_SECONDS, Presence, PresenceState, WhenLost};
pub use spring::{Collider, ColliderGroup, ColliderShape, MAX_STEPS, STEP_SECONDS, Spring, SpringBones, SpringJoint, SpringSim};
pub use vocab::{Blendshape, MAX_WEIGHT_ENTRIES, TrackFrame, Weights};

/// Why an avatar or its settings were refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AvatarError {
    #[error("The avatar is not valid: {0}.")]
    Invalid(String),
    #[error("The avatar has more {what} than allowed ({limit}).")]
    TooLarge { what: &'static str, limit: usize },
}

#[cfg(test)]
#[path = "tests/mod.rs"]
mod tests;
