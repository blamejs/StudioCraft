//! Spring bones: chains of nodes (hair, ears, cloth) that swing with the head's movement, fall
//! under gravity, return toward their rest direction and stay outside sphere and capsule
//! colliders.
//!
//! The settings and the step follow the published VRM 1.0 spring bone specification
//! (https://github.com/vrm-c/vrm-specification/blob/master/specification/VRMC_springBone-1.0/README.md):
//! each joint's tail keeps moving by its last movement scaled by `1 − drag`, is pulled toward its
//! rest direction by `stiffness` and down by `gravity_power`, is kept at its rest length from the
//! joint, and is pushed out of colliders. [`SpringSim`] runs that step at a fixed 60 steps a
//! second whatever the caller's frame rate, so the motion is the same at 30, 60 or 144 fps, and
//! keeps its state between calls. Positions are in the model's space.

use crate::AvatarError;
use crate::math::{Quat, Transform, Vec3};
use crate::model::{Limits, Model, world_of};

/// The step length, in seconds.
pub const STEP_SECONDS: f64 = 1.0 / 60.0;
/// Most steps one call runs; a longer gap is dropped.
pub const MAX_STEPS: u32 = 8;

/// A collider's shape, in its node's space.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ColliderShape {
    Sphere { offset: Vec3, radius: f32 },
    Capsule { offset: Vec3, tail: Vec3, radius: f32 },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Collider {
    pub node: usize,
    pub shape: ColliderShape,
}

/// Colliders that chains name together.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ColliderGroup {
    pub name: String,
    pub colliders: Vec<usize>,
}

/// One joint of a chain and how its part swings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpringJoint {
    pub node: usize,
    /// The radius of the joint's tail for collisions.
    pub hit_radius: f32,
    /// How strongly the tail returns to its rest direction.
    pub stiffness: f32,
    pub gravity_power: f32,
    /// The direction of gravity, in the model's space.
    pub gravity_dir: Vec3,
    /// 0 keeps all movement, 1 stops it every step.
    pub drag: f32,
}

impl SpringJoint {
    /// A joint with the VRM defaults: stiffness 1, drag 0.5, no gravity, no radius.
    pub fn new(node: usize) -> SpringJoint {
        SpringJoint { node, hit_radius: 0.0, stiffness: 1.0, gravity_power: 0.0, gravity_dir: Vec3::new(0.0, -1.0, 0.0), drag: 0.5 }
    }
}

/// A chain: each joint is an ancestor of the next. The last joint is the end of the chain and
/// only its position is used.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Spring {
    pub name: String,
    pub joints: Vec<SpringJoint>,
    pub collider_groups: Vec<usize>,
}

/// Every spring chain and collider of a model.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SpringBones {
    pub colliders: Vec<Collider>,
    pub groups: Vec<ColliderGroup>,
    pub springs: Vec<Spring>,
}

fn bad(what: String) -> AvatarError {
    AvatarError::Invalid(what)
}

fn is_ancestor(model: &Model, ancestor: usize, node: usize) -> bool {
    let mut at = model.nodes.get(node).and_then(|n| n.parent);
    while let Some(p) = at {
        if p == ancestor {
            return true;
        }
        at = model.nodes.get(p).and_then(|n| n.parent);
    }
    false
}

impl SpringBones {
    pub(crate) fn validate(&self, model: &Model, limits: &Limits) -> Result<(), AvatarError> {
        if self.springs.len() > limits.springs {
            return Err(AvatarError::TooLarge { what: "spring chains", limit: limits.springs });
        }
        if self.colliders.len() > limits.colliders {
            return Err(AvatarError::TooLarge { what: "spring colliders", limit: limits.colliders });
        }
        if self.groups.len() > limits.collider_groups {
            return Err(AvatarError::TooLarge { what: "collider groups", limit: limits.collider_groups });
        }
        for (i, c) in self.colliders.iter().enumerate() {
            let ok = match c.shape {
                ColliderShape::Sphere { offset, radius } => offset.is_finite() && radius.is_finite() && radius >= 0.0,
                ColliderShape::Capsule { offset, tail, radius } => offset.is_finite() && tail.is_finite() && radius.is_finite() && radius >= 0.0,
            };
            if c.node >= model.nodes.len() || !ok {
                return Err(bad(format!("spring collider {i} is not valid")));
            }
        }
        for (i, g) in self.groups.iter().enumerate() {
            if g.colliders.len() > limits.colliders || g.colliders.iter().any(|&c| c >= self.colliders.len()) {
                return Err(bad(format!("collider group {i} names a collider that does not exist")));
            }
        }
        let mut total = 0usize;
        let mut seen: Vec<usize> = Vec::new();
        for (i, s) in self.springs.iter().enumerate() {
            total = total.saturating_add(s.joints.len());
            if total > limits.spring_joints {
                return Err(AvatarError::TooLarge { what: "spring joints", limit: limits.spring_joints });
            }
            if s.collider_groups.len() > limits.collider_groups || s.collider_groups.iter().any(|&g| g >= self.groups.len()) {
                return Err(bad(format!("spring {i} names a collider group that does not exist")));
            }
            for (j, joint) in s.joints.iter().enumerate() {
                let finite = joint.gravity_dir.is_finite()
                    && [joint.hit_radius, joint.stiffness, joint.gravity_power, joint.drag].iter().all(|v| v.is_finite() && *v >= 0.0);
                if joint.node >= model.nodes.len() || !finite || joint.drag > 1.0 {
                    return Err(bad(format!("spring {i} joint {j} is not valid")));
                }
                if j > 0 && !is_ancestor(model, s.joints[j - 1].node, joint.node) {
                    return Err(bad(format!("spring {i} joint {j} is not below the joint before it")));
                }
                // The end joint of a chain may also start nothing, so only joints that move a
                // tail are checked against other chains.
                if j + 1 < s.joints.len() {
                    if seen.contains(&joint.node) {
                        return Err(bad(format!("spring {i} moves a node that another chain moves")));
                    }
                    seen.push(joint.node);
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct JointState {
    node: usize,
    tail_node: usize,
    props: SpringJoint,
    /// The tail's direction from the joint at rest, in the joint's space.
    axis: Vec3,
    length: f32,
    rest: Quat,
    prev: Vec3,
    current: Vec3,
}

/// The running spring-bone simulation of one model.
#[derive(Clone, Debug)]
pub struct SpringSim {
    joints: Vec<JointState>,
    /// For each chain: its joint range and its colliders.
    chains: Vec<(usize, usize, Vec<usize>)>,
    pending: f64,
}

impl SpringSim {
    /// The simulation of `model`'s springs, with every tail at rest. The model must be valid.
    /// Joints whose tail lies on the joint have no direction and do not move.
    pub fn new(model: &Model) -> SpringSim {
        let world = model.rest_world();
        let mut joints = Vec::new();
        let mut chains = Vec::new();
        for s in &model.springs.springs {
            let start = joints.len();
            for pair in s.joints.windows(2) {
                let (a, b) = (pair[0], pair[1]);
                let (Some(wa), Some(wb)) = (world.get(a.node), world.get(b.node)) else {
                    continue;
                };
                let d = wb.translation - wa.translation;
                let Some(rest) = model.nodes.get(a.node).map(|n| n.rest.rotation) else {
                    continue;
                };
                let axis = wa.rotation.conjugate().rotate(d).normalized().unwrap_or(Vec3::ZERO);
                joints.push(JointState {
                    node: a.node,
                    tail_node: b.node,
                    props: a,
                    axis,
                    length: d.length(),
                    rest,
                    prev: wb.translation,
                    current: wb.translation,
                });
            }
            let mut colliders: Vec<usize> = Vec::new();
            for g in &s.collider_groups {
                for c in model.springs.groups.get(*g).map_or(&[][..], |g| &g.colliders[..]) {
                    if !colliders.contains(c) {
                        colliders.push(*c);
                    }
                }
            }
            chains.push((start, joints.len(), colliders));
        }
        SpringSim { joints, chains, pending: 0.0 }
    }

    /// Puts every tail where the pose `local` places it, at rest.
    pub fn reset(&mut self, model: &Model, local: &[Transform]) {
        for j in &mut self.joints {
            if let Some(w) = world_of(model, local, j.tail_node) {
                j.prev = w.translation;
                j.current = w.translation;
            }
        }
        self.pending = 0.0;
    }

    /// Advances `dt` seconds and writes each joint's new rotation into `local`, the local
    /// transforms of every node (the pose the rest of the avatar is in). Runs whole steps of
    /// [`STEP_SECONDS`] and keeps the remainder for the next call; at most [`MAX_STEPS`] steps
    /// run, and a longer gap is dropped. A `dt` that is negative or not finite runs nothing.
    /// It does not allocate.
    pub fn step(&mut self, model: &Model, local: &mut [Transform], dt: f64) {
        if !dt.is_finite() || dt <= 0.0 {
            return;
        }
        self.pending = (self.pending + dt).min(STEP_SECONDS * f64::from(MAX_STEPS));
        // A step that is short of a whole step only by rounding still runs.
        while self.pending >= STEP_SECONDS - 1e-9 {
            self.pending -= STEP_SECONDS;
            self.substep(model, local, STEP_SECONDS as f32);
        }
    }

    fn substep(&mut self, model: &Model, local: &mut [Transform], dt: f32) {
        for (start, end, colliders) in &self.chains {
            for j in self.joints.get_mut(*start..*end).into_iter().flatten() {
                if j.length <= 1e-6 {
                    continue;
                }
                let Some(head) = world_of(model, local, j.node) else {
                    continue;
                };
                let parent_rot =
                    model.nodes.get(j.node).and_then(|n| n.parent).and_then(|p| world_of(model, local, p)).map_or(Quat::IDENTITY, |t| t.rotation);
                let p = j.props;
                let rest_dir = (parent_rot * j.rest).rotate(j.axis);
                let gravity = p.gravity_dir.normalized().unwrap_or(Vec3::ZERO);
                let mut next = j.current + (j.current - j.prev) * (1.0 - p.drag) + rest_dir * (p.stiffness * dt) + gravity * (p.gravity_power * dt);
                let keep_length = |v: Vec3| head.translation + (v - head.translation).normalized().unwrap_or(rest_dir) * j.length;
                next = keep_length(next);
                for c in colliders {
                    let Some(c) = model.springs.colliders.get(*c) else {
                        continue;
                    };
                    let Some(cw) = world_of(model, local, c.node) else {
                        continue;
                    };
                    let (center, radius) = match c.shape {
                        ColliderShape::Sphere { offset, radius } => (cw.apply(offset), radius),
                        ColliderShape::Capsule { offset, tail, radius } => {
                            let (a, b) = (cw.apply(offset), cw.apply(tail));
                            let ab = b - a;
                            let len2 = ab.length_squared();
                            let t = if len2 > 1e-12 { ((next - a).dot(ab) / len2).clamp(0.0, 1.0) } else { 0.0 };
                            (a + ab * t, radius)
                        }
                    };
                    // A collider stays round: under an uneven scale its radius takes the largest axis.
                    let scale = cw.scale.x.abs().max(cw.scale.y.abs()).max(cw.scale.z.abs());
                    let reach = radius * scale + p.hit_radius;
                    let delta = next - center;
                    let distance = delta.length() - reach;
                    if distance < 0.0 {
                        // A tail at the collider's center is pushed out sideways to its part.
                        let bone = (next - head.translation).normalized().unwrap_or(rest_dir);
                        let side = bone.cross(Vec3::Y).normalized().or(bone.cross(Vec3::X).normalized()).unwrap_or(Vec3::X);
                        let dir = delta.normalized().unwrap_or(side);
                        next = keep_length(next - dir * distance);
                        if (next - center).length() < reach {
                            next = outside_on_reach(head.translation, j.length, center, reach, next, side);
                        }
                    }
                }
                j.prev = j.current;
                j.current = next;
                // Turn the joint so its rest axis points at the new tail.
                let to = (parent_rot * j.rest).conjugate().rotate(next - head.translation);
                if let Some(t) = local.get_mut(j.node) {
                    t.rotation = (j.rest * Quat::from_rotation_arc(j.axis, to)).normalized();
                }
            }
        }
    }

    /// Where each moving joint's tail is now, in the model's space, in chain order.
    pub fn tails(&self) -> impl Iterator<Item = Vec3> + '_ {
        self.joints.iter().map(|j| j.current)
    }
}

/// The point `length` from `head` and `reach` from `center`, on the side of `toward`. When no
/// such point exists (the collider holds every point the tail can reach, or touches none of
/// them), it returns `toward`.
fn outside_on_reach(head: Vec3, length: f32, center: Vec3, reach: f32, toward: Vec3, side: Vec3) -> Vec3 {
    let to_center = center - head;
    let d = to_center.length();
    let Some(u) = to_center.normalized() else {
        return toward;
    };
    // The angle between the part and the collider's center, from the law of cosines.
    let cos = (length * length + d * d - reach * reach) / (2.0 * length * d);
    if !(-1.0..=1.0).contains(&cos) {
        return toward;
    }
    let t = toward - head;
    let v = (t - u * t.dot(u)).normalized().or((side - u * side.dot(u)).normalized()).unwrap_or(side);
    let sin = (1.0 - cos * cos).max(0.0).sqrt();
    head + (u * cos + v * sin) * length
}
