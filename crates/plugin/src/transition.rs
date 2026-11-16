//! Scene transitions.

use studiocraft_model::Settings;

use crate::Plugin;

/// A transition type: a GPU blend from the old scene to the new one.
///
/// `shader` defines
/// `fn transition(a: vec4<f32>, b: vec4<f32>, uv: vec2<f32>, t: f32, p: vec4<f32>) -> vec4<f32>`:
/// the old (`a`) and new (`b`) scene's colors at `uv`, the progress `t` from 0 to 1 and the
/// parameters from [`Transition::params`]. The shader may also sample the scenes itself through
/// `from_tex`, `to_tex` and `samp`.
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
    /// Gains of the old and new scene's audio at progress `t`.
    fn audio_gains(&self, t: f32) -> (f32, f32) {
        let t = if t.is_finite() { t.clamp(0.0, 1.0) } else { 1.0 };
        (1.0 - t, t)
    }
}
