//! Scene transitions: Cut, Fade, Fade through color and Swipe.
//!
//! - [`Kind`] is one transition. It gives its id and name, its WGSL blend ([`Kind::shader`]), the
//!   four floats the blend reads ([`Kind::uniform`]), its ease ([`Kind::ease`]), the gains of the
//!   old and new audio ([`Kind::audio_gains`]), and the same blend on the CPU ([`Kind::blend`]).
//! - [`Params`] holds a transition's settings: the color of Fade through color, the [`Edge`] Swipe
//!   enters from, and an [`Ease`] (each kind's own default when it is `None`). It carries a version,
//!   [`PARAMS_VERSION`], for apps that save it; [`Params::check`] refuses a version this crate
//!   cannot read.
//!
//! # Shader contract
//!
//! Each [`Kind::shader`] is the body of
//! `fn transition(a: vec4<f32>, b: vec4<f32>, uv: vec2<f32>, t: f32, p: vec4<f32>) -> vec4<f32>`.
//! `a` and `b` are the old and new scene's colors at `uv` (0 to 1, from the top left), `t` is the
//! eased progress, clamped to 0 to 1 by the host, and `p` is [`Kind::uniform`]. The host declares
//! `from_tex` and `to_tex` (the old and new scene as `texture_2d<f32>`) and `samp` (a `sampler`)
//! before the body, because Swipe samples `to_tex` itself.
//!
//! No `unsafe` code, and it builds for `wasm32-unknown-unknown`.

#![warn(missing_docs)]

use std::fmt;

pub use craft_ease::Easing;

/// The version of [`Params`] this crate writes and the newest it reads.
pub const PARAMS_VERSION: u32 = 1;

/// A transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Kind {
    /// Switches to the new scene at once, whatever the duration.
    #[cfg_attr(feature = "serde", serde(rename = "cut"))]
    Cut,
    /// Blends the old scene into the new one.
    #[cfg_attr(feature = "serde", serde(rename = "fade"))]
    Fade,
    /// Fades the old scene to a color, then the color to the new scene.
    #[cfg_attr(feature = "serde", serde(rename = "fade_color"))]
    FadeThroughColor,
    /// Slides the new scene in over the old one from an edge.
    #[cfg_attr(feature = "serde", serde(rename = "swipe"))]
    Swipe,
}

impl Kind {
    /// Every transition, in the order a picker lists them.
    pub const ALL: [Kind; 4] = [Kind::Cut, Kind::Fade, Kind::FadeThroughColor, Kind::Swipe];

    /// The stable id: `cut`, `fade`, `fade_color` or `swipe`.
    pub fn id(self) -> &'static str {
        match self {
            Kind::Cut => "cut",
            Kind::Fade => "fade",
            Kind::FadeThroughColor => "fade_color",
            Kind::Swipe => "swipe",
        }
    }

    /// The transition with the id `id`.
    pub fn from_id(id: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.id() == id)
    }

    /// The English name shown to the user.
    pub fn name(self) -> &'static str {
        match self {
            Kind::Cut => "Cut",
            Kind::Fade => "Fade",
            Kind::FadeThroughColor => "Fade through color",
            Kind::Swipe => "Swipe",
        }
    }

    /// Whether the transition switches at once and ignores its duration.
    pub fn instant(self) -> bool {
        self == Kind::Cut
    }

    /// Whether [`Params::ease`] changes the transition. Cut always moves linearly.
    pub fn eased(self) -> bool {
        self != Kind::Cut
    }

    /// The ease used when [`Params::ease`] is `None`: in and out for Swipe, linear for the others.
    pub fn default_ease(self) -> Ease {
        match self {
            Kind::Swipe => Ease::EaseInOut,
            _ => Ease::Linear,
        }
    }

    /// The WGSL body of `fn transition` (see the crate's shader contract).
    pub fn shader(self) -> &'static str {
        match self {
            Kind::Cut => "fn transition(a: vec4<f32>, b: vec4<f32>, uv: vec2<f32>, t: f32, p: vec4<f32>) -> vec4<f32> { return b; }",
            Kind::Fade => "fn transition(a: vec4<f32>, b: vec4<f32>, uv: vec2<f32>, t: f32, p: vec4<f32>) -> vec4<f32> { return mix(a, b, t); }",
            Kind::FadeThroughColor => {
                r"fn transition(a: vec4<f32>, b: vec4<f32>, uv: vec2<f32>, t: f32, p: vec4<f32>) -> vec4<f32> {
    let c = vec4<f32>(p.rgb, 1.0);
    if t < 0.5 { return mix(a, c, t * 2.0); }
    return mix(c, b, t * 2.0 - 1.0);
}"
            }
            Kind::Swipe => {
                r"fn transition(a: vec4<f32>, b: vec4<f32>, uv: vec2<f32>, t: f32, p: vec4<f32>) -> vec4<f32> {
    // p.xy: the edge the new scene enters from, as a unit step across the canvas.
    let q = uv + p.xy * (1.0 - t);
    let inside = all(q >= vec2<f32>(0.0)) && all(q <= vec2<f32>(1.0));
    let shifted = textureSample(to_tex, samp, q);
    return select(a, shifted, inside);
}"
            }
        }
    }

    /// The `p` the shader reads: the color as 0 to 1 with alpha 1 for Fade through color, the
    /// edge's [`Edge::step`] in `x` and `y` for Swipe, and zeros for the others.
    pub fn uniform(self, params: &Params) -> [f32; 4] {
        match self {
            Kind::FadeThroughColor => {
                let [r, g, b, _] = params.color;
                [f32::from(r) / 255.0, f32::from(g) / 255.0, f32::from(b) / 255.0, 1.0]
            }
            Kind::Swipe => {
                let [x, y] = params.edge.step();
                [x, y, 0.0, 0.0]
            }
            Kind::Cut | Kind::Fade => [0.0; 4],
        }
    }

    /// How progress through the duration becomes the shader's `t`.
    pub fn ease(self, params: &Params) -> Easing {
        if !self.eased() {
            return Easing::Linear;
        }
        params.ease.unwrap_or(self.default_ease()).easing()
    }

    /// The gains of the old and new scene's audio at `t`, the fraction of the duration that has
    /// passed (0 to 1, before the ease). Cut gives the new audio at once; the others cross-fade
    /// linearly. A `t` that is not a number gives the new audio.
    pub fn audio_gains(self, t: f32) -> (f32, f32) {
        if self == Kind::Cut {
            return (0.0, 1.0);
        }
        let t = if t.is_finite() { t.clamp(0.0, 1.0) } else { 1.0 };
        (1.0 - t, t)
    }

    /// The shader's blend on the CPU, for one pixel. `a` and `b` are the old and new colors at
    /// `uv`, `t` is the eased progress (clamped to 0 to 1; NaN is the end), `p` is
    /// [`Kind::uniform`], and `sample_new` returns the new scene's color at a point, which Swipe
    /// reads away from `uv`. A `uv` that is not finite gives the old color for Swipe.
    pub fn blend(self, a: [f32; 4], b: [f32; 4], uv: [f32; 2], t: f32, p: [f32; 4], sample_new: impl FnOnce([f32; 2]) -> [f32; 4]) -> [f32; 4] {
        let t = if t.is_nan() { 1.0 } else { t.clamp(0.0, 1.0) };
        match self {
            Kind::Cut => b,
            Kind::Fade => mix(a, b, t),
            Kind::FadeThroughColor => {
                let c = [p[0], p[1], p[2], 1.0];
                if t < 0.5 { mix(a, c, t * 2.0) } else { mix(c, b, t * 2.0 - 1.0) }
            }
            Kind::Swipe => {
                let q = [uv[0] + p[0] * (1.0 - t), uv[1] + p[1] * (1.0 - t)];
                let inside = q.iter().all(|v| (0.0..=1.0).contains(v));
                if inside { sample_new(q) } else { a }
            }
        }
    }
}

/// WGSL's `mix`: `x * (1 - t) + y * t` for each component.
fn mix(x: [f32; 4], y: [f32; 4], t: f32) -> [f32; 4] {
    std::array::from_fn(|i| x[i] * (1.0 - t) + y[i] * t)
}

/// The edge Swipe's new scene enters from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize), serde(rename_all = "snake_case"))]
pub enum Edge {
    /// From the left edge.
    #[default]
    Left,
    /// From the right edge.
    Right,
    /// From the top edge.
    Top,
    /// From the bottom edge.
    Bottom,
}

impl Edge {
    /// Every edge, in the order a picker lists them.
    pub const ALL: [Edge; 4] = [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom];

    /// The stable id: `left`, `right`, `top` or `bottom`.
    pub fn id(self) -> &'static str {
        match self {
            Edge::Left => "left",
            Edge::Right => "right",
            Edge::Top => "top",
            Edge::Bottom => "bottom",
        }
    }

    /// The edge with the id `id`.
    pub fn from_id(id: &str) -> Option<Edge> {
        Edge::ALL.into_iter().find(|e| e.id() == id)
    }

    /// The English label shown to the user.
    pub fn label(self) -> &'static str {
        match self {
            Edge::Left => "From the left",
            Edge::Right => "From the right",
            Edge::Top => "From the top",
            Edge::Bottom => "From the bottom",
        }
    }

    /// The offset, in canvas widths and heights, at which a point of the new scene is sampled at
    /// the start: the new scene's sample point moves from `uv + step` to `uv` as `t` goes from 0
    /// to 1.
    pub fn step(self) -> [f32; 2] {
        match self {
            Edge::Left => [1.0, 0.0],
            Edge::Right => [-1.0, 0.0],
            Edge::Top => [0.0, 1.0],
            Edge::Bottom => [0.0, -1.0],
        }
    }
}

/// The eases a transition offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize), serde(rename_all = "snake_case"))]
pub enum Ease {
    /// Constant speed.
    Linear,
    /// Starts slowly (Quad In).
    EaseIn,
    /// Ends slowly (Quad Out).
    EaseOut,
    /// Starts and ends slowly: the smoothstep 3t² − 2t³, as a cubic Bezier.
    EaseInOut,
}

impl Ease {
    /// Every ease, in the order a picker lists them.
    pub const ALL: [Ease; 4] = [Ease::Linear, Ease::EaseIn, Ease::EaseOut, Ease::EaseInOut];

    /// The stable id: `linear`, `ease_in`, `ease_out` or `ease_in_out`.
    pub fn id(self) -> &'static str {
        match self {
            Ease::Linear => "linear",
            Ease::EaseIn => "ease_in",
            Ease::EaseOut => "ease_out",
            Ease::EaseInOut => "ease_in_out",
        }
    }

    /// The ease with the id `id`.
    pub fn from_id(id: &str) -> Option<Ease> {
        Ease::ALL.into_iter().find(|e| e.id() == id)
    }

    /// The English label shown to the user.
    pub fn label(self) -> &'static str {
        match self {
            Ease::Linear => "Linear",
            Ease::EaseIn => "Ease in",
            Ease::EaseOut => "Ease out",
            Ease::EaseInOut => "Ease in and out",
        }
    }

    /// The curve. [`Ease::EaseInOut`] is the cubic Bezier (1/3, 0, 2/3, 1), which equals
    /// 3t² − 2t³; it is not `craft-ease`'s Quad In Out.
    pub fn easing(self) -> Easing {
        match self {
            Ease::Linear => Easing::Linear,
            Ease::EaseIn => Easing::QuadIn,
            Ease::EaseOut => Easing::QuadOut,
            Ease::EaseInOut => Easing::CubicBezier(1.0 / 3.0, 0.0, 2.0 / 3.0, 1.0),
        }
    }
}

/// A transition's settings. Each kind reads only the fields it uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Params {
    /// The version these settings were written with. A reader calls [`Params::check`] after
    /// reading them.
    pub version: u32,
    /// Fade through color's color, as RGBA; its alpha is not used.
    #[cfg_attr(feature = "serde", serde(default = "opaque_black"))]
    pub color: [u8; 4],
    /// The edge Swipe enters from.
    #[cfg_attr(feature = "serde", serde(default))]
    pub edge: Edge,
    /// The ease; `None` is the kind's [`Kind::default_ease`].
    #[cfg_attr(feature = "serde", serde(default))]
    pub ease: Option<Ease>,
}

#[cfg(feature = "serde")]
fn opaque_black() -> [u8; 4] {
    [0, 0, 0, 255]
}

impl Default for Params {
    /// Version [`PARAMS_VERSION`], opaque black, from the left, and each kind's default ease.
    fn default() -> Self {
        Params { version: PARAMS_VERSION, color: [0, 0, 0, 255], edge: Edge::Left, ease: None }
    }
}

impl Params {
    /// Whether this crate can read these settings: an error for version 0 or a version newer
    /// than [`PARAMS_VERSION`].
    pub fn check(&self) -> Result<(), ParamsError> {
        match self.version {
            0 => Err(ParamsError::NoVersion),
            v if v > PARAMS_VERSION => Err(ParamsError::Newer(v)),
            _ => Ok(()),
        }
    }
}

/// Why [`Params::check`] refused a set of settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamsError {
    /// The version is 0, which no writer uses.
    NoVersion,
    /// The settings were written by a newer version than this crate reads.
    Newer(u32),
}

impl fmt::Display for ParamsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParamsError::NoVersion => write!(f, "the transition settings have no version"),
            ParamsError::Newer(v) => write!(f, "the transition settings are version {v}; this build reads up to version {PARAMS_VERSION}"),
        }
    }
}

impl std::error::Error for ParamsError {}

#[cfg(test)]
mod tests;
