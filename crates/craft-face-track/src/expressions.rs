//! Expression weights measured from the face mesh.
//!
//! The weights use the common vocabulary of 52 face shapes that avatar rigs are built with
//! ([`Blendshape`]), each from 0 (neutral) to 1 (fully formed). "Left" and "right" are the
//! subject's own: with an unmirrored camera, the subject's left eye is on the right of the
//! picture.
//!
//! The weights are geometric measurements, not the output of a learned model. Every point is
//! first carried into the canonical face's frame with the head pose fit, so turning or tilting
//! the head does not change them. Each measurement is then compared with the same measurement on
//! the bundle's canonical (neutral) face, and mapped to 0 to 1 over a range StudioCraft chose. The
//! eyes, eye direction, brows, jaw and mouth corners, mouth width and mouth position are measured;
//! the tongue weight is the face mesh's own tongue score. The other shapes stay at 0.

use crate::landmarker::MESH_POINTS;
use crate::pose::Fit;

macro_rules! shapes {
    ($($v:ident = $n:literal),* $(,)?) => {
        /// One of the 52 face shapes.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum Blendshape { $($v),* }

        impl Blendshape {
            /// Every shape, in the vocabulary's usual order.
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
    /// The shapes the tracker measures; the others stay at 0.
    pub const MEASURED: [Blendshape; 25] = [
        Blendshape::EyeBlinkLeft,
        Blendshape::EyeBlinkRight,
        Blendshape::EyeWideLeft,
        Blendshape::EyeWideRight,
        Blendshape::EyeLookUpLeft,
        Blendshape::EyeLookUpRight,
        Blendshape::EyeLookDownLeft,
        Blendshape::EyeLookDownRight,
        Blendshape::EyeLookInLeft,
        Blendshape::EyeLookInRight,
        Blendshape::EyeLookOutLeft,
        Blendshape::EyeLookOutRight,
        Blendshape::BrowInnerUp,
        Blendshape::BrowDownLeft,
        Blendshape::BrowDownRight,
        Blendshape::BrowOuterUpLeft,
        Blendshape::BrowOuterUpRight,
        Blendshape::JawOpen,
        Blendshape::MouthSmileLeft,
        Blendshape::MouthSmileRight,
        Blendshape::MouthFrownLeft,
        Blendshape::MouthFrownRight,
        Blendshape::MouthPucker,
        Blendshape::MouthLeft,
        Blendshape::MouthRight,
    ];

    /// The shape's position in [`Blendshape::ALL`].
    pub fn index(self) -> usize {
        self as usize
    }
}

/// The weights of all 52 shapes, 0 to 1.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Expressions {
    pub weights: [f32; 52],
}

impl Default for Expressions {
    fn default() -> Expressions {
        Expressions { weights: [0.0; 52] }
    }
}

impl Expressions {
    pub fn get(&self, s: Blendshape) -> f32 {
        self.weights.get(s.index()).copied().unwrap_or(0.0)
    }

    pub fn set(&mut self, s: Blendshape, w: f32) {
        if let Some(v) = self.weights.get_mut(s.index()) {
            *v = if w.is_finite() { w.clamp(0.0, 1.0) } else { 0.0 };
        }
    }

    /// Every shape with its weight.
    pub fn iter(&self) -> impl Iterator<Item = (Blendshape, f32)> + '_ {
        Blendshape::ALL.into_iter().zip(self.weights)
    }
}

/// Mesh points of one eye: inner corner, outer corner, upper lid, lower lid and the iris center.
#[derive(Clone, Copy)]
struct Eye {
    inner: usize,
    outer: usize,
    top: usize,
    bottom: usize,
    iris: usize,
}

/// Mesh points of one brow: inner, middle and outer.
#[derive(Clone, Copy)]
struct Brow {
    inner: usize,
    middle: usize,
    outer: usize,
}

/// The subject's left side is on the right of the picture.
const EYE_LEFT: Eye = Eye { inner: 362, outer: 263, top: 386, bottom: 374, iris: 473 };
const EYE_RIGHT: Eye = Eye { inner: 133, outer: 33, top: 159, bottom: 145, iris: 468 };
const BROW_LEFT: Brow = Brow { inner: 336, middle: 334, outer: 300 };
const BROW_RIGHT: Brow = Brow { inner: 107, middle: 105, outer: 70 };
const MOUTH_LEFT: usize = 291;
const MOUTH_RIGHT: usize = 61;
const LIP_TOP: usize = 13;
const LIP_BOTTOM: usize = 14;
const NOSE_TIP: usize = 1;
const FOREHEAD: usize = 10;
const CHIN: usize = 152;

/// The ranges, in the canonical face's frame, over which each measurement goes from 0 to 1.
/// These are StudioCraft's own values.
mod range {
    /// Eye opening as a fraction of the neutral opening: fully closed at 0.25, open from 0.8.
    pub const BLINK: [f64; 2] = [0.8, 0.25];
    /// Eye opening from 1.15 times neutral to 1.6 times.
    pub const WIDE: [f64; 2] = [1.15, 1.6];
    /// Iris offset from the eye's middle, as a fraction of the eye's width.
    pub const LOOK_SIDE: f64 = 0.12;
    pub const LOOK_UP_DOWN: f64 = 0.08;
    /// Brow movement as a fraction of the face's height.
    pub const BROW_UP: f64 = 0.06;
    pub const BROW_DOWN: f64 = 0.04;
    /// Lip gap as a fraction of the face's height.
    pub const JAW_OPEN: f64 = 0.2;
    /// Mouth corner rise or drop as a fraction of the face's height.
    pub const SMILE: f64 = 0.05;
    pub const FROWN: f64 = 0.04;
    /// Mouth narrowing as a fraction of the neutral width.
    pub const PUCKER: f64 = 0.25;
    /// Mouth shift sideways as a fraction of the face's height.
    pub const SIDE: f64 = 0.06;
}

/// Measurements of one face, in centimeters of the canonical frame (x to the subject's left, y up).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Measures {
    /// Eye opening over eye width, left then right.
    open: [f64; 2],
    /// Brow height above the eye's corners: inner, middle, outer, for the left then the right.
    brow: [[f64; 3]; 2],
    face_height: f64,
    lip_gap: f64,
    /// Mouth corners' height above the upper lip's middle, left then right (the upper lip moves
    /// little when the jaw opens).
    corner: [f64; 2],
    mouth_width: f64,
    /// The mouth's middle sideways from the nose tip.
    mouth_x: f64,
}

fn dist(a: [f64; 3], b: [f64; 3]) -> f64 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

fn mid(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [(a[0] + b[0]) / 2.0, (a[1] + b[1]) / 2.0, (a[2] + b[2]) / 2.0]
}

impl Measures {
    /// The measurements of a face whose points are `at(i)` (canonical frame).
    fn of(at: impl Fn(usize) -> Option<[f64; 3]>) -> Option<Measures> {
        let eye = |e: Eye| -> Option<f64> {
            let w = dist(at(e.inner)?, at(e.outer)?);
            let h = dist(at(e.top)?, at(e.bottom)?);
            (w > 0.0).then_some(h / w)
        };
        let brow = |b: Brow, e: Eye| -> Option<[f64; 3]> {
            let base = mid(at(e.inner)?, at(e.outer)?)[1];
            Some([at(b.inner)?[1] - base, at(b.middle)?[1] - base, at(b.outer)?[1] - base])
        };
        let (top, bottom) = (at(LIP_TOP)?, at(LIP_BOTTOM)?);
        let (ml, mr) = (at(MOUTH_LEFT)?, at(MOUTH_RIGHT)?);
        let m = Measures {
            open: [eye(EYE_LEFT)?, eye(EYE_RIGHT)?],
            brow: [brow(BROW_LEFT, EYE_LEFT)?, brow(BROW_RIGHT, EYE_RIGHT)?],
            face_height: dist(at(FOREHEAD)?, at(CHIN)?),
            lip_gap: dist(top, bottom),
            corner: [ml[1] - top[1], mr[1] - top[1]],
            mouth_width: dist(ml, mr),
            mouth_x: mid(ml, mr)[0] - at(NOSE_TIP)?[0],
        };
        let finite = m
            .open
            .iter()
            .chain(m.brow.iter().flatten())
            .chain(&m.corner)
            .chain(&[m.face_height, m.lip_gap, m.mouth_width, m.mouth_x])
            .all(|v| v.is_finite());
        (finite && m.face_height > 0.0 && m.mouth_width > 0.0).then_some(m)
    }

    /// The canonical (neutral) face's measurements.
    pub(crate) fn neutral(canonical: &[[f32; 3]]) -> Option<Measures> {
        Measures::of(|i| canonical.get(i).map(|p| [p[0] as f64, p[1] as f64, p[2] as f64]))
    }
}

/// `v` mapped from `range[0]` (0) to `range[1]` (1), clamped.
fn ramp(v: f64, [from, to]: [f64; 2]) -> f32 {
    let t = (v - from) / (to - from);
    if t.is_finite() { t.clamp(0.0, 1.0) as f32 } else { 0.0 }
}

/// Measure the expressions of `points` (picture pixels, all [`MESH_POINTS`]) posed by `fit`,
/// against the `neutral` face, into `out`. `tongue` is the face mesh's tongue score.
pub(crate) fn measure(points: &[[f32; 3]], fit: &Fit, neutral: &Measures, tongue: f32, out: &mut Expressions) {
    *out = Expressions::default();
    out.set(Blendshape::TongueOut, tongue);
    if points.len() < MESH_POINTS {
        return;
    }
    let at = |i: usize| points.get(i).map(|p| fit.to_canonical(*p));
    let Some(m) = Measures::of(at) else { return };
    let h = neutral.face_height;
    for (side, (blink, wide)) in
        [(Blendshape::EyeBlinkLeft, Blendshape::EyeWideLeft), (Blendshape::EyeBlinkRight, Blendshape::EyeWideRight)].into_iter().enumerate()
    {
        let open = m.open[side] / neutral.open[side];
        out.set(blink, ramp(open, range::BLINK));
        out.set(wide, ramp(open, range::WIDE));
    }
    // Eye direction: how far the iris sits from the middle of the lids, along the line from the
    // inner to the outer corner and up or down across it, as fractions of the eye's width,
    // measured in the face's plane. The canonical face has no iris points: a neutral iris is taken
    // to sit at the lids' middle.
    let shapes = [
        (EYE_LEFT, [Blendshape::EyeLookInLeft, Blendshape::EyeLookOutLeft, Blendshape::EyeLookUpLeft, Blendshape::EyeLookDownLeft]),
        (EYE_RIGHT, [Blendshape::EyeLookInRight, Blendshape::EyeLookOutRight, Blendshape::EyeLookUpRight, Blendshape::EyeLookDownRight]),
    ];
    for (e, [look_in, look_out, up, down]) in shapes {
        let (Some(inner), Some(outer), Some(top), Some(bottom), Some(iris)) = (at(e.inner), at(e.outer), at(e.top), at(e.bottom), at(e.iris)) else {
            continue;
        };
        let axis = [outer[0] - inner[0], outer[1] - inner[1]];
        let w = axis[0].hypot(axis[1]);
        if w.is_nan() || w <= 0.0 {
            continue;
        }
        let (along, across) = ([axis[0] / w, axis[1] / w], [-axis[1] / w, axis[0] / w]);
        let lids = mid(top, bottom);
        let rel = [iris[0] - lids[0], iris[1] - lids[1]];
        let u = (rel[0] * along[0] + rel[1] * along[1]) / w;
        // `across` points up for both eyes once it is turned to have a positive y.
        let up_dir = if across[1] < 0.0 { [-across[0], -across[1]] } else { across };
        let v = (rel[0] * up_dir[0] + rel[1] * up_dir[1]) / w;
        out.set(look_out, ramp(u, [0.0, range::LOOK_SIDE]));
        out.set(look_in, ramp(-u, [0.0, range::LOOK_SIDE]));
        out.set(up, ramp(v, [0.0, range::LOOK_UP_DOWN]));
        out.set(down, ramp(-v, [0.0, range::LOOK_UP_DOWN]));
    }
    let rise = |side: usize, k: usize| (m.brow[side][k] - neutral.brow[side][k]) / h;
    out.set(Blendshape::BrowInnerUp, ramp((rise(0, 0) + rise(1, 0)) / 2.0, [0.0, range::BROW_UP]));
    out.set(Blendshape::BrowOuterUpLeft, ramp(rise(0, 2), [0.0, range::BROW_UP]));
    out.set(Blendshape::BrowOuterUpRight, ramp(rise(1, 2), [0.0, range::BROW_UP]));
    out.set(Blendshape::BrowDownLeft, ramp(-rise(0, 1), [0.0, range::BROW_DOWN]));
    out.set(Blendshape::BrowDownRight, ramp(-rise(1, 1), [0.0, range::BROW_DOWN]));
    out.set(Blendshape::JawOpen, ramp((m.lip_gap - neutral.lip_gap) / h, [0.0, range::JAW_OPEN]));
    for (side, (smile, frown)) in
        [(Blendshape::MouthSmileLeft, Blendshape::MouthFrownLeft), (Blendshape::MouthSmileRight, Blendshape::MouthFrownRight)].into_iter().enumerate()
    {
        let lift = (m.corner[side] - neutral.corner[side]) / h;
        out.set(smile, ramp(lift, [0.0, range::SMILE]));
        out.set(frown, ramp(-lift, [0.0, range::FROWN]));
    }
    out.set(Blendshape::MouthPucker, ramp(1.0 - m.mouth_width / neutral.mouth_width, [0.0, range::PUCKER]));
    let shift = (m.mouth_x - neutral.mouth_x) / h;
    out.set(Blendshape::MouthLeft, ramp(shift, [0.0, range::SIDE]));
    out.set(Blendshape::MouthRight, ramp(-shift, [0.0, range::SIDE]));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pose::fit;
    use crate::pose::tests::posed;

    #[test]
    fn the_vocabulary_has_52_distinct_names() {
        let mut names: Vec<&str> = Blendshape::ALL.iter().map(|s| s.name()).collect();
        assert_eq!(names.len(), 52);
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), 52);
        for (i, s) in Blendshape::ALL.iter().enumerate() {
            assert_eq!(s.index(), i);
        }
        assert_eq!(Blendshape::EyeBlinkLeft.name(), "eyeBlinkLeft");
        assert_eq!(Blendshape::TongueOut.index(), 51);
    }

    #[test]
    fn weights_are_clamped() {
        let mut e = Expressions::default();
        e.set(Blendshape::JawOpen, 3.0);
        e.set(Blendshape::MouthLeft, -1.0);
        e.set(Blendshape::MouthRight, f32::NAN);
        assert_eq!(e.get(Blendshape::JawOpen), 1.0);
        assert_eq!(e.get(Blendshape::MouthLeft), 0.0);
        assert_eq!(e.get(Blendshape::MouthRight), 0.0);
        assert_eq!(e.iter().count(), 52);
    }

    /// A stand-in canonical face: every point the measurements use placed like a face (cm, y up,
    /// x to the subject's left), the rest on a sphere so the pose fit has spread.
    fn face() -> Vec<[f32; 3]> {
        let mut c: Vec<[f32; 3]> = (0..MESH_POINTS)
            .map(|i| {
                let a = i as f32 * 0.37;
                [7.0 * a.cos() * (a * 0.5).sin(), 9.0 * (a * 0.5).cos(), 3.0 + 2.0 * a.sin()]
            })
            .collect();
        let mut put = |i: usize, p: [f32; 3]| c[i] = p;
        for (s, e, b) in [(1.0, EYE_LEFT, BROW_LEFT), (-1.0, EYE_RIGHT, BROW_RIGHT)] {
            put(e.inner, [s * 1.5, 3.0, 4.0]);
            put(e.outer, [s * 4.5, 3.0, 3.5]);
            put(e.top, [s * 3.0, 3.5, 4.0]);
            put(e.bottom, [s * 3.0, 2.5, 4.0]);
            put(e.iris, [s * 3.0, 3.0, 4.2]);
            put(b.inner, [s * 1.5, 4.5, 4.5]);
            put(b.middle, [s * 3.0, 5.0, 4.5]);
            put(b.outer, [s * 4.5, 4.6, 4.0]);
        }
        put(NOSE_TIP, [0.0, 0.0, 7.0]);
        put(FOREHEAD, [0.0, 8.0, 5.0]);
        put(CHIN, [0.0, -9.0, 4.0]);
        put(LIP_TOP, [0.0, -4.0, 5.5]);
        put(LIP_BOTTOM, [0.0, -4.0, 5.5]);
        put(MOUTH_LEFT, [2.5, -4.0, 4.5]);
        put(MOUTH_RIGHT, [-2.5, -4.0, 4.5]);
        c
    }

    /// The weights of `points_cm` posed in a picture, fitted on the points away from the eyes,
    /// brows and mouth (as the bundle's fit weights favor the face's still points).
    fn fit_weights(points_cm: &[[f32; 3]], canonical: &[[f32; 3]], angles: [f64; 3]) -> Expressions {
        let moving: Vec<usize> = [EYE_LEFT, EYE_RIGHT]
            .iter()
            .flat_map(|e| [e.inner, e.outer, e.top, e.bottom, e.iris])
            .chain([BROW_LEFT, BROW_RIGHT].iter().flat_map(|b| [b.inner, b.middle, b.outer]))
            .chain([LIP_TOP, LIP_BOTTOM, MOUTH_LEFT, MOUTH_RIGHT])
            .collect();
        let basis: Vec<(usize, f32)> = (0..468).filter(|i| !moving.contains(i)).map(|i| (i, 1.0)).collect();
        let pts = posed(points_cm, angles, 10.0, [300.0, -200.0, 0.0]);
        let f = fit(canonical, &basis, &pts).unwrap();
        let neutral = Measures::neutral(canonical).unwrap();
        let mut e = Expressions::default();
        measure(&pts, &f, &neutral, 0.3, &mut e);
        e
    }

    #[test]
    fn a_neutral_face_measures_neutral_at_any_pose() {
        let c = face();
        for angles in [[0.0, 0.0, 0.0], [30.0, -15.0, 20.0], [-40.0, 25.0, -10.0]] {
            let e = fit_weights(&c, &c, angles);
            for s in Blendshape::ALL {
                let want = if s == Blendshape::TongueOut { 0.3 } else { 0.0 };
                assert!((e.get(s) - want).abs() < 1e-3, "{} at {angles:?}: {}", s.name(), e.get(s));
            }
        }
    }

    #[test]
    fn closed_eyes_open_jaw_and_a_smile_are_measured() {
        let c = face();
        let mut f = c.clone();
        // The subject's left eye closed, the right one wide open.
        f[EYE_LEFT.top][1] = 3.05;
        f[EYE_LEFT.bottom][1] = 2.95;
        f[EYE_RIGHT.top][1] = 3.8;
        f[EYE_RIGHT.bottom][1] = 2.2;
        // Jaw open by 3.4 cm of a 17 cm face.
        f[LIP_BOTTOM][1] = -7.4;
        // Both corners raised by 1.7 cm.
        f[MOUTH_LEFT][1] = -4.0 + 1.7;
        f[MOUTH_RIGHT][1] = -4.0 + 1.7;
        let e = fit_weights(&f, &c, [20.0, 10.0, -5.0]);
        assert!(e.get(Blendshape::EyeBlinkLeft) > 0.99, "{}", e.get(Blendshape::EyeBlinkLeft));
        assert_eq!(e.get(Blendshape::EyeBlinkRight), 0.0);
        assert!(e.get(Blendshape::EyeWideRight) > 0.99);
        assert!(e.get(Blendshape::JawOpen) > 0.99, "{}", e.get(Blendshape::JawOpen));
        assert!(e.get(Blendshape::MouthSmileLeft) > 0.5 && e.get(Blendshape::MouthFrownLeft) == 0.0);
    }

    #[test]
    fn the_subjects_left_is_the_pictures_right() {
        let c = face();
        let mut f = c.clone();
        // The mouth moved toward the subject's left (positive x in the canonical frame).
        for i in [LIP_TOP, LIP_BOTTOM, MOUTH_LEFT, MOUTH_RIGHT] {
            f[i][0] += 1.0;
        }
        // The irises looking toward the subject's left: out for the left eye, in for the right.
        f[EYE_LEFT.iris][0] += 0.5;
        f[EYE_RIGHT.iris][0] += 0.5;
        let e = fit_weights(&f, &c, [0.0; 3]);
        assert!(e.get(Blendshape::MouthLeft) > 0.9 && e.get(Blendshape::MouthRight) == 0.0);
        assert!(e.get(Blendshape::EyeLookOutLeft) > 0.9 && e.get(Blendshape::EyeLookInRight) > 0.9);
        assert_eq!(e.get(Blendshape::EyeLookInLeft), 0.0);
        // In the picture, the subject's left eye is the one further right.
        let pts = posed(&c, [0.0; 3], 10.0, [300.0, -200.0, 0.0]);
        assert!(pts[EYE_LEFT.outer][0] > pts[EYE_RIGHT.outer][0]);
    }

    #[test]
    fn short_or_broken_meshes_measure_nothing() {
        let c = face();
        let basis: Vec<(usize, f32)> = (0..468).map(|i| (i, 1.0)).collect();
        let pts = posed(&c, [0.0; 3], 10.0, [0.0; 3]);
        let f = fit(&c, &basis, &pts).unwrap();
        let neutral = Measures::neutral(&c).unwrap();
        let mut e = Expressions::default();
        e.set(Blendshape::JawOpen, 1.0);
        measure(&pts[..100], &f, &neutral, 0.0, &mut e);
        assert_eq!(e, Expressions::default());
        let mut bad = pts.clone();
        bad[LIP_TOP] = [f32::NAN; 3];
        measure(&bad, &f, &neutral, 0.0, &mut e);
        assert_eq!(e, Expressions::default());
        assert!(Measures::neutral(&c[..10]).is_none());
    }
}
