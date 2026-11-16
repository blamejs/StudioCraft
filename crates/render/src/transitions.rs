//! Built-in scene transitions.

use studiocraft_model::{ChoiceOption, Field, FieldKind, Settings, SettingsSchema};
use studiocraft_plugin::{Plugin, Transition};

/// Switches at once.
pub struct Cut;

impl Plugin for Cut {
    fn id(&self) -> &'static str {
        "cut"
    }
    fn name(&self) -> &'static str {
        "Cut"
    }
    fn schema(&self) -> SettingsSchema {
        SettingsSchema::default()
    }
}

impl Transition for Cut {
    fn instant(&self) -> bool {
        true
    }
    fn shader(&self) -> &'static str {
        "fn transition(a: vec4<f32>, b: vec4<f32>, uv: vec2<f32>, t: f32, p: vec4<f32>) -> vec4<f32> { return b; }"
    }
    fn audio_gains(&self, _t: f32) -> (f32, f32) {
        (0.0, 1.0)
    }
}

/// Blends the old scene into the new one.
pub struct Fade;

impl Plugin for Fade {
    fn id(&self) -> &'static str {
        "fade"
    }
    fn name(&self) -> &'static str {
        "Fade"
    }
    fn schema(&self) -> SettingsSchema {
        SettingsSchema::default()
    }
}

impl Transition for Fade {
    fn shader(&self) -> &'static str {
        "fn transition(a: vec4<f32>, b: vec4<f32>, uv: vec2<f32>, t: f32, p: vec4<f32>) -> vec4<f32> { return mix(a, b, t); }"
    }
}

/// Fades the old scene to a color, then the color to the new scene.
pub struct FadeThroughColor;

impl Plugin for FadeThroughColor {
    fn id(&self) -> &'static str {
        "fade_color"
    }
    fn name(&self) -> &'static str {
        "Fade through color"
    }
    fn schema(&self) -> SettingsSchema {
        SettingsSchema::new(vec![Field::new("color", "Color", FieldKind::Color, "#000000")])
    }
}

impl Transition for FadeThroughColor {
    fn shader(&self) -> &'static str {
        r"fn transition(a: vec4<f32>, b: vec4<f32>, uv: vec2<f32>, t: f32, p: vec4<f32>) -> vec4<f32> {
    let c = vec4<f32>(p.rgb, 1.0);
    if t < 0.5 { return mix(a, c, t * 2.0); }
    return mix(c, b, t * 2.0 - 1.0);
}"
    }
    fn params(&self, settings: &Settings) -> [f32; 4] {
        let [r, g, b, _] = settings.color("color").unwrap_or([0, 0, 0, 255]);
        [f32::from(r) / 255.0, f32::from(g) / 255.0, f32::from(b) / 255.0, 1.0]
    }
}

/// The new scene slides in over the old one from an edge.
pub struct Swipe;

impl Plugin for Swipe {
    fn id(&self) -> &'static str {
        "swipe"
    }
    fn name(&self) -> &'static str {
        "Swipe"
    }
    fn schema(&self) -> SettingsSchema {
        let edges = [("left", "From the left"), ("right", "From the right"), ("top", "From the top"), ("bottom", "From the bottom")];
        let options = edges.iter().map(|(v, l)| ChoiceOption::new(*v, *l)).collect();
        SettingsSchema::new(vec![Field::new("direction", "Direction", FieldKind::Choice { options }, "left")])
    }
}

impl Transition for Swipe {
    fn shader(&self) -> &'static str {
        r"fn transition(a: vec4<f32>, b: vec4<f32>, uv: vec2<f32>, t: f32, p: vec4<f32>) -> vec4<f32> {
    // p.xy: the edge the new scene enters from, as a unit step across the canvas.
    let e = t * t * (3.0 - 2.0 * t);
    let q = uv + p.xy * (1.0 - e);
    let inside = all(q >= vec2<f32>(0.0)) && all(q <= vec2<f32>(1.0));
    let shifted = textureSample(to_tex, samp, q);
    return select(a, shifted, inside);
}"
    }
    fn params(&self, settings: &Settings) -> [f32; 4] {
        match settings.str("direction").unwrap_or("left") {
            "right" => [-1.0, 0.0, 0.0, 0.0],
            "top" => [0.0, 1.0, 0.0, 0.0],
            "bottom" => [0.0, -1.0, 0.0, 0.0],
            _ => [1.0, 0.0, 0.0, 0.0],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_follow_settings() {
        let s = Settings::default().with("color", "#ff0000");
        assert_eq!(FadeThroughColor.params(&s), [1.0, 0.0, 0.0, 1.0]);
        let s = Settings::default().with("direction", "bottom");
        assert_eq!(Swipe.params(&s), [0.0, -1.0, 0.0, 0.0]);
        assert_eq!(Cut.audio_gains(0.2), (0.0, 1.0));
        assert_eq!(Fade.audio_gains(0.25), (0.75, 0.25));
    }
}
