//! The pose step: from tracked frames to the avatar's head rotation, lean, eye rotations and
//! expression weights, once per canvas frame.
//!
//! [`Animator::step`] mirrors and calibrates the newest tracked frame, keeps the last one while
//! the face is lost, mixes the tracked and idle poses by the lost-face factor, maps the face
//! shapes to the avatar's expressions, turns the gaze into eye rotations or look expressions,
//! and applies the expressions' overrides. Buffers are sized when the animator is made, so a
//! step does not allocate.

use serde::{Deserialize, Serialize};

use crate::AvatarError;
use crate::calibration::Calibration;
use crate::expression::{Expression, Preset, apply_overrides};
use crate::idle::{Idle, IdleMotion};
use crate::look_at::{LookAt, LookAtKind};
use crate::mapping::{BoundMapping, Mapping, gaze};
use crate::math::{Quat, Vec3};
use crate::model::Model;
use crate::presence::{LostFace, Presence, PresenceState, WhenLost};
use crate::vocab::TrackFrame;

/// The settings of the pose step.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AnimatorSettings {
    /// The avatar moves as the user's mirror image.
    pub mirror: bool,
    pub when_lost: WhenLost,
    pub lost: LostFace,
    pub idle: IdleMotion,
    pub mapping: Mapping,
    /// Taken from the tracker's frames as they arrive, before any mirroring.
    pub calibration: Calibration,
}

impl Default for AnimatorSettings {
    fn default() -> AnimatorSettings {
        AnimatorSettings {
            mirror: true,
            when_lost: WhenLost::default(),
            lost: LostFace::default(),
            idle: IdleMotion::default(),
            mapping: Mapping::standard(),
            calibration: Calibration::default(),
        }
    }
}

impl AnimatorSettings {
    pub fn validate(&self) -> Result<(), AvatarError> {
        self.lost.validate()?;
        self.mapping.validate()?;
        if !self.calibration.is_valid() {
            return Err(AvatarError::Invalid("the calibration has a value that is out of range".into()));
        }
        Ok(())
    }
}

/// The avatar's pose for one canvas frame.
#[derive(Clone, Debug, PartialEq)]
pub struct AvatarPose {
    /// The head's rotation from rest.
    pub head: Quat,
    /// The head's movement from the neutral position, in meters, for leaning the upper body.
    pub lean: Vec3,
    /// The eyes' local rotations, for an avatar whose gaze turns bones.
    pub left_eye: Quat,
    pub right_eye: Quat,
    /// One weight per expression of the model, in its order, overrides applied.
    pub expressions: Vec<f32>,
    /// 0 (breathed out) to 1 (breathed in).
    pub breath: f32,
    /// The share of the tracked pose, 0 to 1; the rest is the idle pose.
    pub tracked: f32,
    pub state: PresenceState,
}

/// The running pose step of one avatar.
#[derive(Clone, Debug)]
pub struct Animator {
    settings: AnimatorSettings,
    expressions: Vec<Expression>,
    mapping: BoundMapping,
    look_at: LookAt,
    /// The look expressions up, down, left, right.
    looks: [Option<usize>; 4],
    /// The expressions the idle blink drives.
    blinks: [Option<usize>; 2],
    lost_expression: Option<usize>,
    presence: Presence,
    idle: Idle,
    last: TrackFrame,
    pose: AvatarPose,
}

impl Animator {
    /// The pose step for `model` with `settings`. `seed` sets the idle blinks' timing. Settings
    /// that name an expression the model lacks are kept and have no effect.
    pub fn new(model: &Model, settings: AnimatorSettings, seed: u64) -> Result<Animator, AvatarError> {
        settings.validate()?;
        let expressions = model.expressions.clone();
        let mapping = settings.mapping.bind(&expressions)?;
        let find = |p: Preset| model.preset(p);
        // Idle blinks drive both one-eye blinks, else `blink`, else whichever one-eye blink there is.
        let blinks = match (find(Preset::BlinkLeft), find(Preset::BlinkRight), find(Preset::Blink)) {
            (Some(l), Some(r), _) => [Some(l), Some(r)],
            (_, _, Some(b)) => [Some(b), None],
            (l, r, None) => [l, r],
        };
        let lost_expression = match &settings.when_lost {
            WhenLost::Expression { name } => expressions.iter().position(|e| &e.name == name),
            _ => None,
        };
        let presence = Presence::new(settings.lost, &settings.when_lost);
        let idle = Idle::new(settings.idle, seed);
        let pose = AvatarPose {
            head: Quat::IDENTITY,
            lean: Vec3::ZERO,
            left_eye: Quat::IDENTITY,
            right_eye: Quat::IDENTITY,
            expressions: vec![0.0; expressions.len()],
            breath: 0.0,
            tracked: 0.0,
            state: presence.state(),
        };
        Ok(Animator {
            looks: [find(Preset::LookUp), find(Preset::LookDown), find(Preset::LookLeft), find(Preset::LookRight)],
            blinks,
            lost_expression,
            look_at: model.look_at,
            expressions,
            mapping,
            presence,
            idle,
            last: TrackFrame::default(),
            pose,
            settings,
        })
    }

    pub fn settings(&self) -> &AnimatorSettings {
        &self.settings
    }

    /// The last pose [`Animator::step`] made.
    pub fn pose(&self) -> &AvatarPose {
        &self.pose
    }

    /// The pose at `now` seconds. `frame` is the newest tracked frame, or `None` when the
    /// tracker has nothing new; the last frame with a face is used while none arrives. A frame
    /// without a face starts the lost-face behavior.
    pub fn step(&mut self, frame: Option<&TrackFrame>, now: f64) -> &AvatarPose {
        let mut face = matches!(self.presence.state(), PresenceState::Tracking | PresenceState::Returning);
        if let Some(f) = frame {
            face = f.present;
            if f.present {
                // The calibration was taken from the tracker's own frames, so it is applied
                // before the mirror.
                let f = self.settings.calibration.apply(f);
                self.last = if self.settings.mirror { f.mirrored() } else { f };
            }
        }
        let k = self.presence.step(now, face);
        let idle = self.idle.step(now);
        let rest = 1.0 - k;
        let p = &mut self.pose;
        p.tracked = k;
        p.state = self.presence.state();
        p.breath = idle.breath;
        let idle_head = Quat::from_yaw_pitch_roll(idle.yaw, idle.pitch, idle.roll);
        p.head = idle_head.nlerp(self.last.rotation, k);
        p.lean = self.last.translation * k;

        self.mapping.eval(&self.last.weights, &mut p.expressions);
        for w in p.expressions.iter_mut() {
            *w *= k;
        }
        for i in self.blinks.iter().flatten() {
            if let Some(w) = p.expressions.get_mut(*i) {
                *w += rest * idle.blink;
            }
        }
        // The tracked gaze goes through the range maps first; the mapped pose is then mixed
        // with the idle pose (eyes ahead, no look expression) by the tracked share.
        let (yaw, pitch) = gaze(&self.last.weights);
        match self.look_at.kind {
            LookAtKind::Bone => {
                let (l, r) = self.look_at.eye_rotations(yaw, pitch);
                p.left_eye = Quat::IDENTITY.nlerp(l, k);
                p.right_eye = Quat::IDENTITY.nlerp(r, k);
            }
            LookAtKind::Expression => {
                p.left_eye = Quat::IDENTITY;
                p.right_eye = Quat::IDENTITY;
                let w = self.look_at.expression_weights(yaw, pitch);
                for (slot, v) in self.looks.iter().zip([w.up, w.down, w.left, w.right]) {
                    if let Some(e) = slot.and_then(|i| p.expressions.get_mut(i)) {
                        *e = v * k;
                    }
                }
            }
        }
        // After the gaze, so a look expression chosen for a lost face is not overwritten.
        if let Some(w) = self.lost_expression.and_then(|i| p.expressions.get_mut(i)) {
            *w = w.max(rest);
        }
        apply_overrides(&self.expressions, &mut p.expressions);
        &self.pose
    }
}
