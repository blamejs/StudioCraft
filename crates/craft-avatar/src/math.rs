//! Small 3D math: vectors, unit quaternions and node transforms, in glTF's axes (right-handed,
//! +Y up, +Z forward). An avatar faces +Z, so its own left is +X.

use std::ops::{Add, AddAssign, Mul, Neg, Sub};

use serde::{Deserialize, Serialize};

/// A 3D vector.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Vec3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl Vec3 {
    pub const ZERO: Vec3 = Vec3 { x: 0.0, y: 0.0, z: 0.0 };
    pub const ONE: Vec3 = Vec3 { x: 1.0, y: 1.0, z: 1.0 };
    pub const X: Vec3 = Vec3 { x: 1.0, y: 0.0, z: 0.0 };
    pub const Y: Vec3 = Vec3 { x: 0.0, y: 1.0, z: 0.0 };
    pub const Z: Vec3 = Vec3 { x: 0.0, y: 0.0, z: 1.0 };

    pub const fn new(x: f32, y: f32, z: f32) -> Vec3 {
        Vec3 { x, y, z }
    }

    pub fn from_array(a: [f32; 3]) -> Vec3 {
        Vec3 { x: a[0], y: a[1], z: a[2] }
    }

    pub fn to_array(self) -> [f32; 3] {
        [self.x, self.y, self.z]
    }

    pub fn dot(self, o: Vec3) -> f32 {
        self.x * o.x + self.y * o.y + self.z * o.z
    }

    pub fn cross(self, o: Vec3) -> Vec3 {
        Vec3::new(self.y * o.z - self.z * o.y, self.z * o.x - self.x * o.z, self.x * o.y - self.y * o.x)
    }

    pub fn length_squared(self) -> f32 {
        self.dot(self)
    }

    pub fn length(self) -> f32 {
        self.length_squared().sqrt()
    }

    /// The vector scaled to length 1, or `None` when it is too short or not finite.
    pub fn normalized(self) -> Option<Vec3> {
        let l = self.length();
        if l.is_finite() && l > 1e-12 { Some(self * (1.0 / l)) } else { None }
    }

    /// Each component multiplied by the other vector's.
    pub fn scale(self, o: Vec3) -> Vec3 {
        Vec3::new(self.x * o.x, self.y * o.y, self.z * o.z)
    }

    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite() && self.z.is_finite()
    }

    pub fn lerp(self, o: Vec3, t: f32) -> Vec3 {
        self + (o - self) * t
    }
}

impl Add for Vec3 {
    type Output = Vec3;
    fn add(self, o: Vec3) -> Vec3 {
        Vec3::new(self.x + o.x, self.y + o.y, self.z + o.z)
    }
}

impl AddAssign for Vec3 {
    fn add_assign(&mut self, o: Vec3) {
        *self = *self + o;
    }
}

impl Sub for Vec3 {
    type Output = Vec3;
    fn sub(self, o: Vec3) -> Vec3 {
        Vec3::new(self.x - o.x, self.y - o.y, self.z - o.z)
    }
}

impl Mul<f32> for Vec3 {
    type Output = Vec3;
    fn mul(self, s: f32) -> Vec3 {
        Vec3::new(self.x * s, self.y * s, self.z * s)
    }
}

impl Neg for Vec3 {
    type Output = Vec3;
    fn neg(self) -> Vec3 {
        Vec3::new(-self.x, -self.y, -self.z)
    }
}

/// A rotation as a unit quaternion.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Quat {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub w: f32,
}

impl Default for Quat {
    fn default() -> Quat {
        Quat::IDENTITY
    }
}

impl Quat {
    pub const IDENTITY: Quat = Quat { x: 0.0, y: 0.0, z: 0.0, w: 1.0 };

    pub const fn new(x: f32, y: f32, z: f32, w: f32) -> Quat {
        Quat { x, y, z, w }
    }

    /// A rotation of `radians` about `axis`. A zero or non-finite axis gives the identity.
    pub fn from_axis_angle(axis: Vec3, radians: f32) -> Quat {
        match axis.normalized() {
            Some(a) if radians.is_finite() => {
                let (s, c) = (radians * 0.5).sin_cos();
                Quat::new(a.x * s, a.y * s, a.z * s, c)
            }
            _ => Quat::IDENTITY,
        }
    }

    /// Yaw about +Y, pitch about +X and roll about +Z, in degrees, as `Ry(yaw) · Rx(pitch) ·
    /// Rz(roll)`. A positive yaw turns +Z toward +X (the avatar's left), a positive pitch turns
    /// +Z toward −Y (down), and a positive roll turns +X toward +Y.
    pub fn from_yaw_pitch_roll(yaw: f32, pitch: f32, roll: f32) -> Quat {
        let y = Quat::from_axis_angle(Vec3::Y, yaw.to_radians());
        let p = Quat::from_axis_angle(Vec3::X, pitch.to_radians());
        let r = Quat::from_axis_angle(Vec3::Z, roll.to_radians());
        y * p * r
    }

    /// The yaw, pitch and roll of [`Quat::from_yaw_pitch_roll`], in degrees. Pitch is within ±90.
    pub fn to_yaw_pitch_roll(self) -> (f32, f32, f32) {
        let q = self.normalized();
        let (x, y, z, w) = (q.x, q.y, q.z, q.w);
        let sp = (2.0 * (w * x - y * z)).clamp(-1.0, 1.0);
        let pitch = sp.asin();
        let yaw = (2.0 * (w * y + x * z)).atan2(1.0 - 2.0 * (x * x + y * y));
        let roll = (2.0 * (w * z + x * y)).atan2(1.0 - 2.0 * (x * x + z * z));
        (yaw.to_degrees(), pitch.to_degrees(), roll.to_degrees())
    }

    /// The shortest rotation that turns direction `from` into direction `to`. Directions that
    /// are zero or not finite give the identity.
    pub fn from_rotation_arc(from: Vec3, to: Vec3) -> Quat {
        let (Some(a), Some(b)) = (from.normalized(), to.normalized()) else {
            return Quat::IDENTITY;
        };
        let d = a.dot(b);
        if d < -0.999_999 {
            // Opposite directions: half a turn about any axis at right angles to `a`.
            let side = if a.x.abs() < 0.9 { Vec3::X } else { Vec3::Y };
            return Quat::from_axis_angle(a.cross(side), std::f32::consts::PI);
        }
        let c = a.cross(b);
        Quat::new(c.x, c.y, c.z, 1.0 + d).normalized()
    }

    pub fn dot(self, o: Quat) -> f32 {
        self.x * o.x + self.y * o.y + self.z * o.z + self.w * o.w
    }

    /// Whether the quaternion is finite and of length 1, within 0.001.
    pub fn is_unit(self) -> bool {
        (self.dot(self).sqrt() - 1.0).abs() <= 1e-3
    }

    /// The quaternion scaled to length 1; one that cannot be gives the identity.
    pub fn normalized(self) -> Quat {
        let l = self.dot(self).sqrt();
        if l.is_finite() && l > 1e-12 {
            let s = 1.0 / l;
            Quat::new(self.x * s, self.y * s, self.z * s, self.w * s)
        } else {
            Quat::IDENTITY
        }
    }

    /// The inverse rotation.
    pub fn conjugate(self) -> Quat {
        Quat::new(-self.x, -self.y, -self.z, self.w)
    }

    /// `v` rotated.
    pub fn rotate(self, v: Vec3) -> Vec3 {
        let u = Vec3::new(self.x, self.y, self.z);
        let t = u.cross(v) * 2.0;
        v + t * self.w + u.cross(t)
    }

    /// Normalized linear interpolation along the shorter way: `self` at 0, `o` at 1.
    pub fn nlerp(self, o: Quat, t: f32) -> Quat {
        let o = if self.dot(o) < 0.0 { Quat::new(-o.x, -o.y, -o.z, -o.w) } else { o };
        Quat::new(self.x + (o.x - self.x) * t, self.y + (o.y - self.y) * t, self.z + (o.z - self.z) * t, self.w + (o.w - self.w) * t).normalized()
    }

    /// The rotation seen in a mirror at the plane x = 0: yaw and roll change sign, pitch stays.
    pub fn mirrored_x(self) -> Quat {
        Quat::new(self.x, -self.y, -self.z, self.w)
    }

    /// The angle between two rotations, in degrees.
    pub fn angle_to(self, o: Quat) -> f32 {
        let d = self.normalized().dot(o.normalized()).abs().min(1.0);
        (2.0 * d.acos()).to_degrees()
    }

    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite() && self.z.is_finite() && self.w.is_finite()
    }
}

impl Mul for Quat {
    type Output = Quat;
    fn mul(self, o: Quat) -> Quat {
        Quat::new(
            self.w * o.x + self.x * o.w + self.y * o.z - self.z * o.y,
            self.w * o.y - self.x * o.z + self.y * o.w + self.z * o.x,
            self.w * o.z + self.x * o.y - self.y * o.x + self.z * o.w,
            self.w * o.w - self.x * o.x - self.y * o.y - self.z * o.z,
        )
    }
}

/// A node's translation, rotation and scale, applied scale first.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Transform {
    pub translation: Vec3,
    pub rotation: Quat,
    pub scale: Vec3,
}

impl Default for Transform {
    fn default() -> Transform {
        Transform::IDENTITY
    }
}

impl Transform {
    pub const IDENTITY: Transform = Transform { translation: Vec3::ZERO, rotation: Quat::IDENTITY, scale: Vec3::ONE };

    /// This transform followed by its parent's: the child's transform in the parent's space.
    /// Scale composes per axis, which is exact while scales are uniform.
    pub fn then(self, parent: &Transform) -> Transform {
        Transform {
            translation: parent.translation + parent.rotation.rotate(self.translation.scale(parent.scale)),
            rotation: (parent.rotation * self.rotation).normalized(),
            scale: parent.scale.scale(self.scale),
        }
    }

    /// A point in this transform's space carried out to the space it is in.
    pub fn apply(&self, p: Vec3) -> Vec3 {
        self.translation + self.rotation.rotate(p.scale(self.scale))
    }

    pub fn is_finite(&self) -> bool {
        self.translation.is_finite() && self.rotation.is_finite() && self.scale.is_finite()
    }
}
