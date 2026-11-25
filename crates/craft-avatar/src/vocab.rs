//! The tracking vocabulary: 52 face shapes ([`Blendshape`]), their weights ([`Weights`]) and one
//! tracked frame with the head's pose ([`TrackFrame`]).
//!
//! The 52 shapes are the common set that face-tracked avatar rigs are built with, spelled as the
//! rigs spell them (`eyeBlinkLeft`, `jawOpen`). Each weight runs from 0 (neutral) to 1 (fully
//! formed). "Left" and "right" are the subject's own sides.

use serde::de::{MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::math::{Quat, Vec3};

macro_rules! shapes {
    ($($v:ident = $n:literal),* $(,)?) => {
        /// One of the 52 face shapes.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum Blendshape { $($v),* }

        impl Blendshape {
            /// Every shape, in the vocabulary's usual order. [`Blendshape::index`] is the
            /// position in this list.
            pub const ALL: [Blendshape; 52] = [$(Blendshape::$v),*];

            /// The shape's name as avatar rigs spell it, for example `eyeBlinkLeft`.
            pub fn name(self) -> &'static str {
                match self { $(Blendshape::$v => $n),* }
            }
        }
    };
}

shapes! {
    EyeBlinkLeft = "eyeBlinkLeft", EyeLookDownLeft = "eyeLookDownLeft", EyeLookInLeft = "eyeLookInLeft",
    EyeLookOutLeft = "eyeLookOutLeft", EyeLookUpLeft = "eyeLookUpLeft", EyeSquintLeft = "eyeSquintLeft",
    EyeWideLeft = "eyeWideLeft", EyeBlinkRight = "eyeBlinkRight", EyeLookDownRight = "eyeLookDownRight",
    EyeLookInRight = "eyeLookInRight", EyeLookOutRight = "eyeLookOutRight", EyeLookUpRight = "eyeLookUpRight",
    EyeSquintRight = "eyeSquintRight", EyeWideRight = "eyeWideRight", JawForward = "jawForward", JawLeft = "jawLeft",
    JawRight = "jawRight", JawOpen = "jawOpen", MouthClose = "mouthClose", MouthFunnel = "mouthFunnel",
    MouthPucker = "mouthPucker", MouthLeft = "mouthLeft", MouthRight = "mouthRight", MouthSmileLeft = "mouthSmileLeft",
    MouthSmileRight = "mouthSmileRight", MouthFrownLeft = "mouthFrownLeft", MouthFrownRight = "mouthFrownRight",
    MouthDimpleLeft = "mouthDimpleLeft", MouthDimpleRight = "mouthDimpleRight", MouthStretchLeft = "mouthStretchLeft",
    MouthStretchRight = "mouthStretchRight", MouthRollLower = "mouthRollLower", MouthRollUpper = "mouthRollUpper",
    MouthShrugLower = "mouthShrugLower", MouthShrugUpper = "mouthShrugUpper", MouthPressLeft = "mouthPressLeft",
    MouthPressRight = "mouthPressRight", MouthLowerDownLeft = "mouthLowerDownLeft", MouthLowerDownRight = "mouthLowerDownRight",
    MouthUpperUpLeft = "mouthUpperUpLeft", MouthUpperUpRight = "mouthUpperUpRight", BrowDownLeft = "browDownLeft",
    BrowDownRight = "browDownRight", BrowInnerUp = "browInnerUp", BrowOuterUpLeft = "browOuterUpLeft",
    BrowOuterUpRight = "browOuterUpRight", CheekPuff = "cheekPuff", CheekSquintLeft = "cheekSquintLeft",
    CheekSquintRight = "cheekSquintRight", NoseSneerLeft = "noseSneerLeft", NoseSneerRight = "noseSneerRight",
    TongueOut = "tongueOut",
}

impl Blendshape {
    /// The shape's position in [`Blendshape::ALL`].
    pub fn index(self) -> usize {
        self as usize
    }

    /// The shape named `name`, matched exactly.
    pub fn from_name(name: &str) -> Option<Blendshape> {
        Blendshape::ALL.iter().copied().find(|s| s.name() == name)
    }

    /// The same shape on the other side (`eyeBlinkLeft` and `eyeBlinkRight`, `mouthLeft` and
    /// `mouthRight`); a shape with no side is itself.
    pub fn mirrored(self) -> Blendshape {
        let name = self.name();
        let (stem, other) = match (name.strip_suffix("Left"), name.strip_suffix("Right")) {
            (Some(stem), _) => (stem, "Right"),
            (None, Some(stem)) => (stem, "Left"),
            (None, None) => return self,
        };
        Blendshape::ALL.iter().copied().find(|s| s.name().strip_suffix(other) == Some(stem)).unwrap_or(self)
    }
}

/// The index of each shape's mirror, built once without allocating.
fn mirror_table() -> &'static [usize; 52] {
    static TABLE: std::sync::OnceLock<[usize; 52]> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        let mut t = [0; 52];
        for s in Blendshape::ALL {
            t[s.index()] = s.mirrored().index();
        }
        t
    })
}

/// One weight per [`Blendshape`], each within 0 to 1.
///
/// Serialized as a map from shape name to weight. Reading skips names outside the vocabulary
/// and refuses a weight that is not a number; a missing name reads as 0.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Weights(pub [f32; 52]);

impl Default for Weights {
    fn default() -> Weights {
        Weights([0.0; 52])
    }
}

impl Weights {
    pub fn get(&self, s: Blendshape) -> f32 {
        self.0[s.index()]
    }

    pub fn set(&mut self, s: Blendshape, w: f32) {
        self.0[s.index()] = clamp01(w);
    }

    /// Every weight brought into 0 to 1; a weight that is not a number becomes 0.
    pub fn sanitized(mut self) -> Weights {
        for w in &mut self.0 {
            *w = clamp01(*w);
        }
        self
    }

    /// The weights with each side's shape swapped with the other side's.
    pub fn mirrored(&self) -> Weights {
        let t = mirror_table();
        let mut out = [0.0; 52];
        for (i, w) in out.iter_mut().enumerate() {
            *w = self.0[t[i]];
        }
        Weights(out)
    }
}

/// `w` within 0 to 1; NaN gives 0.
pub(crate) fn clamp01(w: f32) -> f32 {
    if w.is_nan() { 0.0 } else { w.clamp(0.0, 1.0) }
}

impl Serialize for Weights {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(Some(52))?;
        for shape in Blendshape::ALL {
            m.serialize_entry(shape.name(), &self.get(shape))?;
        }
        m.end()
    }
}

impl<'de> Deserialize<'de> for Weights {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Weights, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Weights;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a map from face shape names to weights")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Weights, A::Error> {
                let mut w = Weights::default();
                let mut entries = 0usize;
                while let Some(name) = map.next_key::<String>()? {
                    entries += 1;
                    if entries > MAX_WEIGHT_ENTRIES {
                        return Err(serde::de::Error::custom("too many face shape weights"));
                    }
                    let value: f32 = map.next_value()?;
                    if !value.is_finite() {
                        return Err(serde::de::Error::custom(format!("the weight of {name} is not a number")));
                    }
                    if let Some(s) = Blendshape::from_name(&name) {
                        w.set(s, value);
                    }
                }
                Ok(w)
            }
        }
        d.deserialize_map(V)
    }
}

/// Most entries a weight map may hold when read, unknown names included.
pub const MAX_WEIGHT_ENTRIES: usize = 256;

/// What the face tracker measured in one camera picture.
///
/// The pose is in the avatar's axes (+Y up, +Z out of the face toward the camera, +X the
/// subject's left): a positive yaw turns the face toward the subject's left, a positive pitch
/// tilts it down, and a positive roll tips the top of the head toward the subject's right.
/// `translation` is in meters from where the tracker first found the face, or from the
/// calibrated neutral position.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TrackFrame {
    /// When the picture was taken, in seconds on the caller's clock.
    pub time: f64,
    /// Whether a face was found. When it is false the other fields are not used.
    pub present: bool,
    pub weights: Weights,
    pub rotation: Quat,
    pub translation: Vec3,
}

impl Default for TrackFrame {
    fn default() -> TrackFrame {
        TrackFrame { time: 0.0, present: false, weights: Weights::default(), rotation: Quat::IDENTITY, translation: Vec3::ZERO }
    }
}

impl TrackFrame {
    /// The frame as a mirror shows it: each side's shapes swap, yaw and roll change sign, and
    /// sideways movement reverses.
    pub fn mirrored(&self) -> TrackFrame {
        TrackFrame {
            time: self.time,
            present: self.present,
            weights: self.weights.mirrored(),
            rotation: self.rotation.mirrored_x(),
            translation: Vec3::new(-self.translation.x, self.translation.y, self.translation.z),
        }
    }

    /// The frame with weights brought into 0 to 1, and a rotation or translation that is not
    /// finite replaced by the identity or zero.
    pub fn sanitized(&self) -> TrackFrame {
        TrackFrame {
            time: if self.time.is_finite() { self.time } else { 0.0 },
            present: self.present,
            weights: self.weights.sanitized(),
            rotation: if self.rotation.is_finite() { self.rotation.normalized() } else { Quat::IDENTITY },
            translation: if self.translation.is_finite() { self.translation } else { Vec3::ZERO },
        }
    }
}
