use super::*;
use studiocraft_plugin::easing_from;

/// The built-in transitions as they were before they moved to `craft-transitions`, to check that
/// the plug-ins give the same shaders, parameters, eases, audio gains and settings forms.
pub(crate) mod before {
    use super::*;

    pub const CUT: &str = "fn transition(a: vec4<f32>, b: vec4<f32>, uv: vec2<f32>, t: f32, p: vec4<f32>) -> vec4<f32> { return b; }";
    pub const FADE: &str = "fn transition(a: vec4<f32>, b: vec4<f32>, uv: vec2<f32>, t: f32, p: vec4<f32>) -> vec4<f32> { return mix(a, b, t); }";
    pub const FADE_COLOR: &str = r"fn transition(a: vec4<f32>, b: vec4<f32>, uv: vec2<f32>, t: f32, p: vec4<f32>) -> vec4<f32> {
    let c = vec4<f32>(p.rgb, 1.0);
    if t < 0.5 { return mix(a, c, t * 2.0); }
    return mix(c, b, t * 2.0 - 1.0);
}";
    pub const SWIPE: &str = r"fn transition(a: vec4<f32>, b: vec4<f32>, uv: vec2<f32>, t: f32, p: vec4<f32>) -> vec4<f32> {
    // p.xy: the edge the new scene enters from, as a unit step across the canvas.
    let q = uv + p.xy * (1.0 - t);
    let inside = all(q >= vec2<f32>(0.0)) && all(q <= vec2<f32>(1.0));
    let shifted = textureSample(to_tex, samp, q);
    return select(a, shifted, inside);
}";

    pub fn shader(id: &str) -> &'static str {
        match id {
            "cut" => CUT,
            "fade" => FADE,
            "fade_color" => FADE_COLOR,
            _ => SWIPE,
        }
    }

    pub fn params(id: &str, settings: &Settings) -> [f32; 4] {
        match id {
            "fade_color" => {
                let [r, g, b, _] = settings.color("color").unwrap_or([0, 0, 0, 255]);
                [f32::from(r) / 255.0, f32::from(g) / 255.0, f32::from(b) / 255.0, 1.0]
            }
            "swipe" => match settings.str("direction").unwrap_or("left") {
                "right" => [-1.0, 0.0, 0.0, 0.0],
                "top" => [0.0, 1.0, 0.0, 0.0],
                "bottom" => [0.0, -1.0, 0.0, 0.0],
                _ => [1.0, 0.0, 0.0, 0.0],
            },
            _ => [0.0; 4],
        }
    }

    pub fn easing(id: &str, settings: &Settings) -> Easing {
        match id {
            "fade" | "fade_color" => easing_from(settings, "fade_easing", "linear"),
            "swipe" => easing_from(settings, "swipe_easing", "ease_in_out"),
            _ => Easing::Linear,
        }
    }

    pub fn audio_gains(id: &str, t: f32) -> (f32, f32) {
        if id == "cut" {
            return (0.0, 1.0);
        }
        let t = if t.is_finite() { t.clamp(0.0, 1.0) } else { 1.0 };
        (1.0 - t, t)
    }

    pub fn schema(id: &str) -> SettingsSchema {
        match id {
            "fade" => SettingsSchema::new(vec![easing_field("fade_easing", "linear")]),
            "fade_color" => {
                SettingsSchema::new(vec![Field::new("color", "Color", FieldKind::Color, "#000000"), easing_field("fade_easing", "linear")])
            }
            "swipe" => {
                let edges = [("left", "From the left"), ("right", "From the right"), ("top", "From the top"), ("bottom", "From the bottom")];
                let options = edges.iter().map(|(v, l)| ChoiceOption::new(*v, *l)).collect();
                SettingsSchema::new(vec![
                    Field::new("direction", "Direction", FieldKind::Choice { options }, "left"),
                    easing_field("swipe_easing", "ease_in_out"),
                ])
            }
            _ => SettingsSchema::default(),
        }
    }
}

/// The four built-ins, in registration order.
pub(crate) fn built_ins() -> [&'static dyn Transition; 4] {
    [&Fade, &Cut, &FadeThroughColor, &Swipe]
}

/// Settings that exercise every branch: missing, valid, unknown and wrongly typed values, and the
/// easing keys of the other transitions.
fn settings_grid() -> Vec<Settings> {
    let mut grid = vec![Settings::default()];
    for color in ["#ff0000", "#12345678", "#00ff0080", "red", "#12", ""] {
        grid.push(Settings::default().with("color", color));
    }
    for direction in ["left", "right", "top", "bottom", "diagonal", "Left", ""] {
        grid.push(Settings::default().with("direction", direction));
    }
    for key in ["fade_easing", "swipe_easing", "easing"] {
        for value in ["linear", "ease_in", "ease_out", "ease_in_out", "wobble", ""] {
            grid.push(Settings::default().with(key, value));
        }
    }
    grid.push(Settings::default().with("color", 7).with("direction", true).with("fade_easing", 1.5).with("swipe_easing", 0));
    grid.push(Settings::default().with("color", "#0000ff").with("direction", "top").with("fade_easing", "ease_out").with("swipe_easing", "ease_in"));
    grid
}

#[test]
fn the_plug_ins_match_the_transitions_before_the_crate() {
    for t in built_ins() {
        let id = t.id();
        assert_eq!(t.shader(), before::shader(id), "{id}");
        assert_eq!(t.instant(), id == "cut", "{id}");
        assert_eq!(t.schema(), before::schema(id), "{id}");
        for s in settings_grid() {
            assert_eq!(t.params(&s).map(f32::to_bits), before::params(id, &s).map(f32::to_bits), "{id} {s:?}");
            assert_eq!(t.easing(&s), before::easing(id, &s), "{id} {s:?}");
        }
        for k in -4..=24 {
            let x = k as f32 / 20.0;
            assert_eq!(t.audio_gains(x), before::audio_gains(id, x), "{id} {x}");
        }
        for odd in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert_eq!(t.audio_gains(odd), before::audio_gains(id, odd), "{id} {odd}");
        }
    }
    assert_eq!(built_ins().map(|t| t.name()), ["Fade", "Cut", "Fade through color", "Swipe"]);
}

/// The plug-in crate's Easing choices and the shared crate's eases are the same list.
#[test]
fn the_easing_setting_offers_the_shared_eases() {
    let FieldKind::Choice { options } = easing_field("k", "linear").kind else { panic!("not a choice") };
    let offered: Vec<_> = options.iter().map(|o| (o.value.as_str().unwrap_or("").to_owned(), o.label.clone())).collect();
    let shared: Vec<_> = Ease::ALL.iter().map(|e| (e.id().to_owned(), e.label().to_owned())).collect();
    assert_eq!(offered, shared);
    for e in Ease::ALL {
        assert_eq!(easing_from(&Settings::default().with("k", e.id()), "k", "linear"), e.easing(), "{}", e.id());
    }
}

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
    let s = Settings::default().with("fade_easing", "ease_out");
    assert_eq!(Fade.easing(&s), Easing::QuadOut);
    assert_eq!(FadeThroughColor.easing(&Settings::default().with("fade_easing", "ease_in")), Easing::QuadIn);
    // A generic key another transition might use does not move the built-in fades.
    assert_eq!(Fade.easing(&Settings::default().with("easing", "ease_in")), Easing::Linear);
    // Swipe keeps its own choice, so a Fade's easing left in the settings does not move it.
    assert_eq!(Swipe.easing(&s), Swipe.easing(&none));
    assert_eq!(Swipe.easing(&s.clone().with("swipe_easing", "ease_in")), Easing::QuadIn);
    // A value from an older or edited file that is not a choice takes the default.
    let odd = Settings::default().with("fade_easing", "wobble").with("swipe_easing", "wobble");
    assert_eq!(Fade.easing(&odd), Easing::Linear);
    assert_eq!(Swipe.easing(&odd), Swipe.easing(&none));
    // Each eased transition offers the setting, with its default.
    for (schema, key, default) in [
        (Fade.schema(), "fade_easing", "linear"),
        (FadeThroughColor.schema(), "fade_easing", "linear"),
        (Swipe.schema(), "swipe_easing", "ease_in_out"),
    ] {
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
