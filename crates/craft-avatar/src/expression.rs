//! Expressions: named sets of morph-target weights, material colors and texture offsets that
//! turn on together, and the rules by which one expression holds back the mouth, blink or gaze
//! expressions.
//!
//! The names, binds and override rules follow the published VRM 1.0 expressions specification
//! (https://github.com/vrm-c/vrm-specification/blob/master/specification/VRMC_vrm-1.0/expressions.md).

use serde::{Deserialize, Serialize};

/// The preset expression names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Preset {
    Happy,
    Angry,
    Sad,
    Relaxed,
    Surprised,
    Aa,
    Ih,
    Ou,
    Ee,
    Oh,
    Blink,
    BlinkLeft,
    BlinkRight,
    LookUp,
    LookDown,
    LookLeft,
    LookRight,
    Neutral,
}

impl Preset {
    pub const ALL: [Preset; 18] = [
        Preset::Happy,
        Preset::Angry,
        Preset::Sad,
        Preset::Relaxed,
        Preset::Surprised,
        Preset::Aa,
        Preset::Ih,
        Preset::Ou,
        Preset::Ee,
        Preset::Oh,
        Preset::Blink,
        Preset::BlinkLeft,
        Preset::BlinkRight,
        Preset::LookUp,
        Preset::LookDown,
        Preset::LookLeft,
        Preset::LookRight,
        Preset::Neutral,
    ];

    /// The name as files spell it, for example `blinkLeft`.
    pub fn name(self) -> &'static str {
        match self {
            Preset::Happy => "happy",
            Preset::Angry => "angry",
            Preset::Sad => "sad",
            Preset::Relaxed => "relaxed",
            Preset::Surprised => "surprised",
            Preset::Aa => "aa",
            Preset::Ih => "ih",
            Preset::Ou => "ou",
            Preset::Ee => "ee",
            Preset::Oh => "oh",
            Preset::Blink => "blink",
            Preset::BlinkLeft => "blinkLeft",
            Preset::BlinkRight => "blinkRight",
            Preset::LookUp => "lookUp",
            Preset::LookDown => "lookDown",
            Preset::LookLeft => "lookLeft",
            Preset::LookRight => "lookRight",
            Preset::Neutral => "neutral",
        }
    }

    pub fn from_name(name: &str) -> Option<Preset> {
        Preset::ALL.iter().copied().find(|p| p.name() == name)
    }

    /// The kind of procedural expression this is, if any: the expressions that other
    /// expressions can hold back.
    pub fn group(self) -> Option<Group> {
        match self {
            Preset::Aa | Preset::Ih | Preset::Ou | Preset::Ee | Preset::Oh => Some(Group::Mouth),
            Preset::Blink | Preset::BlinkLeft | Preset::BlinkRight => Some(Group::Blink),
            Preset::LookUp | Preset::LookDown | Preset::LookLeft | Preset::LookRight => Some(Group::LookAt),
            _ => None,
        }
    }
}

/// The procedural expression groups an override acts on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Group {
    /// `aa`, `ih`, `ou`, `ee`, `oh`.
    Mouth,
    /// `blink`, `blinkLeft`, `blinkRight`.
    Blink,
    /// `lookUp`, `lookDown`, `lookLeft`, `lookRight`.
    LookAt,
}

/// An expression's name: a preset or a custom name.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ExpressionName {
    Preset(Preset),
    Custom(String),
}

impl ExpressionName {
    /// A name read from text: a preset's spelling gives the preset, anything else a custom name.
    pub fn parse(name: &str) -> ExpressionName {
        match Preset::from_name(name) {
            Some(p) => ExpressionName::Preset(p),
            None => ExpressionName::Custom(name.to_string()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            ExpressionName::Preset(p) => p.name(),
            ExpressionName::Custom(s) => s,
        }
    }

    pub fn preset(&self) -> Option<Preset> {
        match self {
            ExpressionName::Preset(p) => Some(*p),
            ExpressionName::Custom(_) => None,
        }
    }

    pub fn group(&self) -> Option<Group> {
        self.preset().and_then(Preset::group)
    }

    /// Whether the name reads back as itself: false for a custom name spelled like a preset,
    /// which [`ExpressionName::parse`] turns into the preset. VRM 1.0 does not allow such names.
    pub fn is_canonical(&self) -> bool {
        match self {
            ExpressionName::Preset(_) => true,
            ExpressionName::Custom(s) => Preset::from_name(s).is_none(),
        }
    }
}

impl Serialize for ExpressionName {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ExpressionName {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<ExpressionName, D::Error> {
        let s = String::deserialize(d)?;
        if s.is_empty() || s.len() > MAX_NAME_BYTES {
            return Err(serde::de::Error::custom("an expression name must have 1 to 64 bytes"));
        }
        Ok(ExpressionName::parse(&s))
    }
}

/// Longest expression name, in bytes.
pub const MAX_NAME_BYTES: usize = 64;

/// What an expression does to one group of procedural expressions while its own weight is not
/// zero.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Override {
    /// Nothing.
    #[default]
    None,
    /// The group's weights become 0.
    Block,
    /// The group's weights are scaled down by this expression's weight.
    Blend,
}

/// A morph target that an expression sets: target `target` of the mesh on node `node`, at
/// `weight` when the expression is fully on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MorphBind {
    pub node: usize,
    pub target: usize,
    pub weight: f32,
}

/// Which material value a [`ColorBind`] changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorTarget {
    Color,
    EmissionColor,
    ShadeColor,
    MatcapColor,
    RimColor,
    OutlineColor,
}

/// A material color an expression moves toward `value` as it turns on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColorBind {
    pub material: usize,
    pub target: ColorTarget,
    pub value: [f32; 4],
}

/// A texture scale and offset an expression moves toward as it turns on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextureBind {
    pub material: usize,
    pub scale: [f32; 2],
    pub offset: [f32; 2],
}

/// One expression of an avatar.
#[derive(Clone, Debug, PartialEq)]
pub struct Expression {
    pub name: ExpressionName,
    /// The expression is either fully on (above 0.5) or off.
    pub is_binary: bool,
    pub morphs: Vec<MorphBind>,
    pub colors: Vec<ColorBind>,
    pub textures: Vec<TextureBind>,
    pub override_mouth: Override,
    pub override_blink: Override,
    pub override_look_at: Override,
}

impl Expression {
    /// An expression with no binds and no overrides.
    pub fn new(name: ExpressionName) -> Expression {
        Expression {
            name,
            is_binary: false,
            morphs: Vec::new(),
            colors: Vec::new(),
            textures: Vec::new(),
            override_mouth: Override::None,
            override_blink: Override::None,
            override_look_at: Override::None,
        }
    }

    fn override_for(&self, g: Group) -> Override {
        match g {
            Group::Mouth => self.override_mouth,
            Group::Blink => self.override_blink,
            Group::LookAt => self.override_look_at,
        }
    }
}

/// Applies the binary rule and the overrides to `weights`, one weight per expression in
/// `expressions`, in place. Weights are first brought into 0 to 1.
///
/// - A binary expression is 1 above 0.5 and 0 otherwise.
/// - For each group, every expression outside the group whose (binary) weight is above 0 and
///   whose override for the group is `block` sets the group's factor to 0. The others with
///   `blend` add their weights, and the factor is 1 minus that sum, at least 0.
/// - Each expression in the group is multiplied by the factor. A binary one is set to 0 when
///   the factor is below 1.
///
/// All three factors come from the clamped and binary weights, before any group is held back,
/// so an expression that another group's factor turns off still holds back its own targets.
///
/// Custom expressions are never held back; they may hold back the preset groups. `weights`
/// shorter than `expressions` is handled for the expressions it covers. It does not allocate.
pub fn apply_overrides(expressions: &[Expression], weights: &mut [f32]) {
    let n = expressions.len().min(weights.len());
    for (e, w) in expressions.iter().zip(weights.iter_mut()).take(n) {
        let v = crate::vocab::clamp01(*w);
        *w = if e.is_binary { if v > 0.5 { 1.0 } else { 0.0 } } else { v };
    }
    // Every factor is taken from the weights above before any group is held back.
    let groups = [Group::Mouth, Group::Blink, Group::LookAt];
    let factors = groups.map(|g| {
        let mut blocked = false;
        let mut blend = 0.0f32;
        for (e, &w) in expressions.iter().zip(weights.iter()).take(n) {
            if e.name.group() == Some(g) || w <= 0.0 {
                continue;
            }
            match e.override_for(g) {
                Override::None => {}
                Override::Block => blocked = true,
                Override::Blend => blend += w,
            }
        }
        if blocked { 0.0 } else { 1.0 - blend.clamp(0.0, 1.0) }
    });
    for (g, factor) in groups.into_iter().zip(factors) {
        if factor >= 1.0 {
            continue;
        }
        for (e, w) in expressions.iter().zip(weights.iter_mut()).take(n) {
            if e.name.group() == Some(g) {
                *w = if e.is_binary { 0.0 } else { *w * factor };
            }
        }
    }
}
