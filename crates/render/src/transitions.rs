//! Built-in scene transitions.

use studiocraft_model::{ChoiceOption, Field, FieldKind, Settings, SettingsSchema};
use studiocraft_plugin::{Easing, Plugin, Transition, easing_field, easing_from};

/// The Easing setting's key and default for Fade and Fade through color.
const FADE_EASING: (&str, &str) = ("easing", "linear");
/// The Easing setting's key and default for Swipe: a smoothstep, as it has always moved. Its
/// key is its own, so a Fade's easing does not carry over when the transition changes.
const SWIPE_EASING: (&str, &str) = ("swipe_easing", "ease_in_out");

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
        SettingsSchema::new(vec![easing_field(FADE_EASING.0, FADE_EASING.1)])
    }
}

impl Transition for Fade {
    fn shader(&self) -> &'static str {
        "fn transition(a: vec4<f32>, b: vec4<f32>, uv: vec2<f32>, t: f32, p: vec4<f32>) -> vec4<f32> { return mix(a, b, t); }"
    }
    fn easing(&self, settings: &Settings) -> Easing {
        easing_from(settings, FADE_EASING.0, FADE_EASING.1)
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
        SettingsSchema::new(vec![Field::new("color", "Color", FieldKind::Color, "#000000"), easing_field(FADE_EASING.0, FADE_EASING.1)])
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
    fn easing(&self, settings: &Settings) -> Easing {
        easing_from(settings, FADE_EASING.0, FADE_EASING.1)
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
        SettingsSchema::new(vec![
            Field::new("direction", "Direction", FieldKind::Choice { options }, "left"),
            easing_field(SWIPE_EASING.0, SWIPE_EASING.1),
        ])
    }
}

impl Transition for Swipe {
    fn shader(&self) -> &'static str {
        r"fn transition(a: vec4<f32>, b: vec4<f32>, uv: vec2<f32>, t: f32, p: vec4<f32>) -> vec4<f32> {
    // p.xy: the edge the new scene enters from, as a unit step across the canvas.
    let q = uv + p.xy * (1.0 - t);
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
    fn easing(&self, settings: &Settings) -> Easing {
        easing_from(settings, SWIPE_EASING.0, SWIPE_EASING.1)
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

    #[test]
    fn easing_follows_settings_with_each_transitions_default() {
        let none = Settings::default();
        assert_eq!(Fade.easing(&none), Easing::Linear);
        assert_eq!(FadeThroughColor.easing(&none), Easing::Linear);
        assert_eq!(Cut.easing(&none), Easing::Linear);
        let s = Settings::default().with("easing", "ease_out");
        assert_eq!(Fade.easing(&s), Easing::QuadOut);
        assert_eq!(FadeThroughColor.easing(&Settings::default().with("easing", "ease_in")), Easing::QuadIn);
        // Swipe keeps its own choice, so a Fade's easing left in the settings does not move it.
        assert_eq!(Swipe.easing(&s), Swipe.easing(&none));
        assert_eq!(Swipe.easing(&s.clone().with("swipe_easing", "ease_in")), Easing::QuadIn);
        // A value from an older or edited file that is not a choice takes the default.
        let odd = Settings::default().with("easing", "wobble").with("swipe_easing", "wobble");
        assert_eq!(Fade.easing(&odd), Easing::Linear);
        assert_eq!(Swipe.easing(&odd), Swipe.easing(&none));
        // Each eased transition offers the setting, with its default.
        for (schema, key, default) in
            [(Fade.schema(), "easing", "linear"), (FadeThroughColor.schema(), "easing", "linear"), (Swipe.schema(), "swipe_easing", "ease_in_out")]
        {
            let f = schema.fields.iter().find(|f| f.key == key).unwrap();
            assert_eq!(f.label, "Easing");
            assert_eq!(f.default.as_str(), Some(default));
            let FieldKind::Choice { options } = &f.kind else { panic!("{:?}", f.kind) };
            assert_eq!(options.iter().map(|o| o.value.as_str().unwrap_or("")).collect::<Vec<_>>(), ["linear", "ease_in", "ease_out", "ease_in_out"]);
        }
        assert!(Cut.schema().fields.is_empty());
    }

    /// Swipe used to apply 3t² − 2t³ in its shader; its default ease now gives the shader the
    /// same progress.
    #[test]
    fn swipe_moves_as_it_did_before_by_default() {
        let e = Swipe.easing(&Settings::default());
        for k in 0..=100 {
            let t = k as f64 / 100.0;
            let old = t * t * (3.0 - 2.0 * t);
            assert!((e.apply(t) - old).abs() < 1e-9, "{t}: {} vs {old}", e.apply(t));
        }
        assert!(!Swipe.shader().contains("3.0 - 2.0"));
    }
}
