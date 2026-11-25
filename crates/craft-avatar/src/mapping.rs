//! Mappings from the 52 tracked face shapes to an avatar's expressions, and the gaze from the eye
//! shapes.
//!
//! A [`Mapping`] is a list of [`Rule`]s, each naming an expression and a [`Formula`] over the
//! tracked weights, with a threshold and a gain. [`Mapping::standard`] is StudioCraft's own
//! default table. [`Mapping::bind`] resolves the rules against one avatar's expressions once, so
//! evaluating a frame does no name lookups and does not allocate.

use serde::{Deserialize, Serialize};

use crate::AvatarError;
use crate::expression::{Expression, ExpressionName, Preset};
use crate::vocab::{Blendshape, Weights, clamp01};

/// Most rules in a mapping.
pub const MAX_RULES: usize = 256;
/// Most shapes in one formula.
pub const MAX_FORMULA_SHAPES: usize = 8;
/// Highest gain a rule may have.
pub const MAX_GAIN: f32 = 4.0;
/// How far the eyes turn, in degrees, when an eye-direction shape is at 1. StudioCraft's own
/// value.
pub const GAZE_DEGREES: f32 = 30.0;

/// How a formula combines its shapes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Combine {
    #[default]
    Average,
    Min,
    Max,
}

/// A value from the tracked weights: the average, smallest or largest of a few shapes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Formula {
    pub combine: Combine,
    #[serde(with = "shape_names")]
    pub shapes: Vec<Blendshape>,
}

impl Formula {
    pub fn single(s: Blendshape) -> Formula {
        Formula { combine: Combine::Average, shapes: vec![s] }
    }

    pub fn of(combine: Combine, shapes: &[Blendshape]) -> Formula {
        Formula { combine, shapes: shapes.to_vec() }
    }

    /// The formula's value for `w`, within 0 to 1; an empty formula gives 0.
    pub fn eval(&self, w: &Weights) -> f32 {
        let mut it = self.shapes.iter().map(|s| w.get(*s));
        let Some(first) = it.next() else {
            return 0.0;
        };
        let v = match self.combine {
            Combine::Average => {
                let (sum, n) = it.fold((first, 1.0f32), |(s, n), x| (s + x, n + 1.0));
                sum / n
            }
            Combine::Min => it.fold(first, f32::min),
            Combine::Max => it.fold(first, f32::max),
        };
        clamp01(v)
    }
}

mod shape_names {
    use serde::{Deserializer, Serializer};

    use crate::vocab::Blendshape;

    pub fn serialize<S: Serializer>(shapes: &[Blendshape], s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq(shapes.iter().map(|b| b.name()))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Blendshape>, D::Error> {
        let names: Vec<String> = super::bounded::read(d, super::MAX_FORMULA_SHAPES, "face shapes in a formula")?;
        names.iter().map(|n| Blendshape::from_name(n).ok_or_else(|| serde::de::Error::custom(format!("unknown face shape {n}")))).collect()
    }
}

mod rule_list {
    use serde::{Deserializer, Serialize, Serializer};

    use super::Rule;

    pub fn serialize<S: Serializer>(rules: &[Rule], s: S) -> Result<S::Ok, S::Error> {
        rules.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Rule>, D::Error> {
        super::bounded::read(d, super::MAX_RULES, "mapping rules")
    }
}

/// Lists read from settings stop at their limit while being read, so an oversized list is
/// refused before it is held in memory.
mod bounded {
    use std::fmt;
    use std::marker::PhantomData;

    use serde::de::{Error, SeqAccess, Visitor};
    use serde::{Deserialize, Deserializer};

    struct Bounded<T> {
        limit: usize,
        what: &'static str,
        item: PhantomData<T>,
    }

    impl<'de, T: Deserialize<'de>> Visitor<'de> for Bounded<T> {
        type Value = Vec<T>;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            write!(f, "a list of at most {} {}", self.limit, self.what)
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<T>, A::Error> {
            let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(self.limit));
            while let Some(v) = seq.next_element()? {
                if out.len() == self.limit {
                    return Err(A::Error::custom(format!("more than {} {}", self.limit, self.what)));
                }
                out.push(v);
            }
            Ok(out)
        }
    }

    pub fn read<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D, limit: usize, what: &'static str) -> Result<Vec<T>, D::Error> {
        d.deserialize_seq(Bounded { limit, what, item: PhantomData })
    }
}

/// How one expression follows the tracked face.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    pub expression: ExpressionName,
    pub formula: Formula,
    /// Formula values at or below this give 0; above it the value is spread over 0 to 1.
    #[serde(default)]
    pub threshold: f32,
    /// The spread value is multiplied by this, then limited to 1.
    #[serde(default = "one")]
    pub gain: f32,
}

fn one() -> f32 {
    1.0
}

impl Rule {
    pub fn new(expression: ExpressionName, formula: Formula) -> Rule {
        Rule { expression, formula, threshold: 0.0, gain: 1.0 }
    }

    /// The rule's output for a formula value `v`.
    pub fn shape(&self, v: f32) -> f32 {
        if v <= self.threshold {
            return 0.0;
        }
        // A valid rule's threshold is below 1, so the span is above 0. For a rule that skipped
        // validation, a span of 0 gives infinity or NaN, which `clamp01` turns into 1 or 0.
        clamp01((v - self.threshold) / (1.0 - self.threshold) * self.gain)
    }

    fn validate(&self) -> Result<(), AvatarError> {
        let name = self.expression.as_str();
        if !self.expression.is_canonical() {
            return Err(AvatarError::Invalid(format!("the rule for {name} has a custom name that is a preset's name")));
        }
        if !(0.0..1.0).contains(&self.threshold) {
            return Err(AvatarError::Invalid(format!("the rule for {name} has a threshold outside 0 to 1")));
        }
        if !(0.0..=MAX_GAIN).contains(&self.gain) {
            return Err(AvatarError::Invalid(format!("the rule for {name} has a gain outside 0 to 4")));
        }
        if self.formula.shapes.is_empty() || self.formula.shapes.len() > MAX_FORMULA_SHAPES {
            return Err(AvatarError::Invalid(format!("the rule for {name} must name 1 to 8 face shapes")));
        }
        Ok(())
    }
}

/// The rules for every expression that follows the face.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Mapping {
    #[serde(with = "rule_list")]
    pub rules: Vec<Rule>,
}

impl Default for Mapping {
    fn default() -> Mapping {
        Mapping::standard()
    }
}

impl Mapping {
    /// StudioCraft's default table:
    ///
    /// | Expression | Formula |
    /// | --- | --- |
    /// | `blinkLeft`, `blinkRight` | `eyeBlinkLeft`, `eyeBlinkRight` |
    /// | `blink` | the smaller of the two, only for an avatar without both one-eye blinks |
    /// | `aa` | `jawOpen` |
    /// | `ih` | the average of `mouthStretchLeft` and `mouthStretchRight` |
    /// | `ou` | `mouthPucker` |
    /// | `ee` | the average of `mouthUpperUpLeft` and `mouthUpperUpRight` |
    /// | `oh` | `mouthFunnel` |
    /// | `happy` | the average of `mouthSmileLeft` and `mouthSmileRight` |
    /// | `sad` | the average of `mouthFrownLeft` and `mouthFrownRight` |
    /// | `angry` | the average of `browDownLeft` and `browDownRight` |
    /// | `surprised` | the average of `eyeWideLeft` and `eyeWideRight` |
    ///
    /// `happy`, `sad`, `angry` and `surprised` start at 0.2 and the others at 0, each with gain 1.
    /// The gaze comes from the eye-direction shapes ([`gaze`]), not from rules.
    pub fn standard() -> Mapping {
        use Blendshape as B;
        let p = |p: Preset| ExpressionName::Preset(p);
        let avg = |a: B, b: B| Formula::of(Combine::Average, &[a, b]);
        let mut rules = vec![
            Rule::new(p(Preset::BlinkLeft), Formula::single(B::EyeBlinkLeft)),
            Rule::new(p(Preset::BlinkRight), Formula::single(B::EyeBlinkRight)),
            Rule::new(p(Preset::Blink), Formula::of(Combine::Min, &[B::EyeBlinkLeft, B::EyeBlinkRight])),
            Rule::new(p(Preset::Aa), Formula::single(B::JawOpen)),
            Rule::new(p(Preset::Ih), avg(B::MouthStretchLeft, B::MouthStretchRight)),
            Rule::new(p(Preset::Ou), Formula::single(B::MouthPucker)),
            Rule::new(p(Preset::Ee), avg(B::MouthUpperUpLeft, B::MouthUpperUpRight)),
            Rule::new(p(Preset::Oh), Formula::single(B::MouthFunnel)),
        ];
        for (e, a, b) in [
            (Preset::Happy, B::MouthSmileLeft, B::MouthSmileRight),
            (Preset::Sad, B::MouthFrownLeft, B::MouthFrownRight),
            (Preset::Angry, B::BrowDownLeft, B::BrowDownRight),
            (Preset::Surprised, B::EyeWideLeft, B::EyeWideRight),
        ] {
            rules.push(Rule { threshold: 0.2, ..Rule::new(p(e), avg(a, b)) });
        }
        Mapping { rules }
    }

    /// Checks the rule count and every rule's values and shapes.
    pub fn validate(&self) -> Result<(), AvatarError> {
        if self.rules.len() > MAX_RULES {
            return Err(AvatarError::TooLarge { what: "mapping rules", limit: MAX_RULES });
        }
        self.rules.iter().try_for_each(Rule::validate)
    }

    /// The rules resolved against `expressions`. A rule for an expression the avatar lacks is
    /// skipped (it stays in the mapping). When the avatar has both `blinkLeft` and `blinkRight`,
    /// the `blink` rule is skipped so the eyes do not close twice. When several rules name one
    /// expression, the last one is used.
    pub fn bind(&self, expressions: &[Expression]) -> Result<BoundMapping, AvatarError> {
        self.validate()?;
        let has = |p: Preset| expressions.iter().any(|e| e.name == ExpressionName::Preset(p));
        let one_eye = has(Preset::BlinkLeft) && has(Preset::BlinkRight);
        let mut slots: Vec<(usize, Rule)> = Vec::new();
        for r in &self.rules {
            if one_eye && r.expression == ExpressionName::Preset(Preset::Blink) {
                continue;
            }
            let Some(i) = expressions.iter().position(|e| e.name == r.expression) else {
                continue;
            };
            match slots.iter_mut().find(|(j, _)| *j == i) {
                Some(slot) => slot.1 = r.clone(),
                None => slots.push((i, r.clone())),
            }
        }
        Ok(BoundMapping { slots, len: expressions.len() })
    }
}

/// A mapping resolved against one avatar's expressions.
#[derive(Clone, Debug, PartialEq)]
pub struct BoundMapping {
    slots: Vec<(usize, Rule)>,
    len: usize,
}

impl BoundMapping {
    /// How many expressions the avatar has; `eval` writes that many weights.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether expression `i` has a rule.
    pub fn drives(&self, i: usize) -> bool {
        self.slots.iter().any(|(j, _)| *j == i)
    }

    /// Writes one weight per expression into `out`: each rule's output for its expression, and
    /// 0 for expressions without a rule. `out` shorter than the expression count is filled as
    /// far as it goes. It does not allocate.
    pub fn eval(&self, w: &Weights, out: &mut [f32]) {
        for v in out.iter_mut() {
            *v = 0.0;
        }
        for (i, r) in &self.slots {
            if let Some(v) = out.get_mut(*i) {
                *v = r.shape(r.formula.eval(w));
            }
        }
    }
}

/// The gaze from the eye-direction shapes: yaw and pitch in degrees, positive toward the
/// subject's left and down. Each eye looking out or in adds to the yaw (the left eye looking out
/// and the right eye looking in both look left), and the two eyes are averaged; up and down
/// likewise. A shape at 1 on both eyes gives [`GAZE_DEGREES`].
pub fn gaze(w: &Weights) -> (f32, f32) {
    use Blendshape as B;
    let left = (w.get(B::EyeLookOutLeft) + w.get(B::EyeLookInRight) - w.get(B::EyeLookInLeft) - w.get(B::EyeLookOutRight)) * 0.5;
    let down = (w.get(B::EyeLookDownLeft) + w.get(B::EyeLookDownRight) - w.get(B::EyeLookUpLeft) - w.get(B::EyeLookUpRight)) * 0.5;
    (left * GAZE_DEGREES, down * GAZE_DEGREES)
}
