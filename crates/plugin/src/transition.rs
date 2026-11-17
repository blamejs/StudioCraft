//! Scene transitions.

use craft_ease::Easing;
use studiocraft_model::{ChoiceOption, Field, FieldKind, Settings};

use crate::Plugin;

/// The easing choices a transition offers, as `(value, label, ease)`.
const EASINGS: [(&str, &str, Easing); 4] = [
    ("linear", "Linear", Easing::Linear),
    ("ease_in", "Ease in", Easing::QuadIn),
    ("ease_out", "Ease out", Easing::QuadOut),
    // 3t² − 2t³ (smoothstep) as a cubic Bezier.
    ("ease_in_out", "Ease in and out", Easing::CubicBezier(1.0 / 3.0, 0.0, 2.0 / 3.0, 1.0)),
];

/// The Easing setting under `key`: how the transition's progress speeds up and slows down,
/// starting at `default` (a value of [`EASINGS`]). Settings carry over when the transition
/// changes, so transitions that use the same `key` must use the same `default`.
pub fn easing_field(key: &str, default: &str) -> Field {
    let options = EASINGS.iter().map(|(v, l, _)| ChoiceOption::new(*v, *l)).collect();
    Field::new(key, "Easing", FieldKind::Choice { options }, default)
}

/// The ease the Easing setting under `key` in `settings` names; `default`'s for a missing or
/// unknown value.
pub fn easing_from(settings: &Settings, key: &str, default: &str) -> Easing {
    let pick = |v: &str| EASINGS.iter().find(|(k, _, _)| *k == v).map(|(_, _, e)| e.clone());
    settings.str(key).and_then(pick).or_else(|| pick(default)).unwrap_or_default()
}

/// A transition type: a GPU blend from the old scene to the new one.
///
/// `shader` defines
/// `fn transition(a: vec4<f32>, b: vec4<f32>, uv: vec2<f32>, t: f32, p: vec4<f32>) -> vec4<f32>`:
/// the old (`a`) and new (`b`) scene's colors at `uv`, the progress `t` from 0 to 1 (after
/// [`Transition::easing`]) and the parameters from [`Transition::params`]. The shader may also
/// sample the scenes itself through `from_tex`, `to_tex` and `samp`.
pub trait Transition: Plugin {
    /// A transition that switches at once, ignoring the duration.
    fn instant(&self) -> bool {
        false
    }
    fn shader(&self) -> &'static str;
    fn params(&self, settings: &Settings) -> [f32; 4] {
        let _ = settings;
        [0.0; 4]
    }
    /// How progress through the transition's duration becomes the `t` its shader receives.
    fn easing(&self, settings: &Settings) -> Easing {
        let _ = settings;
        Easing::Linear
    }
    /// Gains of the old and new scene's audio at progress `t`.
    fn audio_gains(&self, t: f32) -> (f32, f32) {
        let t = if t.is_finite() { t.clamp(0.0, 1.0) } else { 1.0 };
        (1.0 - t, t)
    }
}
