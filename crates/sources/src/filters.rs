//! Built-in filters.

use craft_color_transform::{Mat3, SRGB, cct};
use studiocraft_media::audio::{BLOCK_FRAMES, CHANNELS, SAMPLE_RATE};
use studiocraft_media::mix::{from_db, to_db};
use studiocraft_model::{Field, FieldKind, Settings, SettingsSchema};
use studiocraft_plugin::{Filter, FilterInstance, FilterTarget, Plugin, PluginError, VIDEO_EFFECT_PARAMS, VideoEffect};

/// A video filter's shader parameters.
type Params = [f32; VIDEO_EFFECT_PARAMS];

fn float(s: &Settings, key: &str, default: f64) -> f32 {
    s.f64(key).filter(|v| v.is_finite()).unwrap_or(default) as f32
}

fn float_field(key: &str, label: &str, min: f64, max: f64, step: f64, default: f64) -> Field {
    Field::new(key, label, FieldKind::Float { min, max, step }, default)
}

/// Raises or lowers a source's volume.
pub struct Gain;

impl Plugin for Gain {
    fn id(&self) -> &'static str {
        "gain"
    }
    fn name(&self) -> &'static str {
        "Gain"
    }
    fn schema(&self) -> SettingsSchema {
        SettingsSchema::new(vec![float_field("db", "Gain (dB)", -30.0, 30.0, 0.1, 0.0)])
    }
}

impl Filter for Gain {
    fn target(&self) -> FilterTarget {
        FilterTarget::Audio
    }
    fn create(&self, settings: &Settings) -> Result<Box<dyn FilterInstance>, PluginError> {
        let mut g = GainInstance(1.0);
        g.update(settings)?;
        Ok(Box::new(g))
    }
}

struct GainInstance(f32);

impl FilterInstance for GainInstance {
    fn update(&mut self, s: &Settings) -> Result<(), PluginError> {
        self.0 = from_db(float(s, "db", 0.0).clamp(-30.0, 30.0));
        Ok(())
    }
    fn process_audio(&mut self, block: &mut [f32]) {
        block.iter_mut().for_each(|s| *s *= self.0);
    }
}

/// Silences a source while it is quieter than a threshold.
pub struct NoiseGate;

impl Plugin for NoiseGate {
    fn id(&self) -> &'static str {
        "noise_gate"
    }
    fn name(&self) -> &'static str {
        "Noise gate"
    }
    fn schema(&self) -> SettingsSchema {
        SettingsSchema::new(vec![
            float_field("open_db", "Open above (dB)", -96.0, 0.0, 0.1, -26.0),
            float_field("close_db", "Close below (dB)", -96.0, 0.0, 0.1, -32.0),
            Field::new("attack_ms", "Attack (ms)", FieldKind::Int { min: 1, max: 1000 }, 25),
            Field::new("hold_ms", "Hold (ms)", FieldKind::Int { min: 0, max: 5000 }, 200),
            Field::new("release_ms", "Release (ms)", FieldKind::Int { min: 1, max: 5000 }, 150),
        ])
    }
}

impl Filter for NoiseGate {
    fn target(&self) -> FilterTarget {
        FilterTarget::Audio
    }
    fn create(&self, settings: &Settings) -> Result<Box<dyn FilterInstance>, PluginError> {
        let mut g = GateInstance { open_db: -26.0, close_db: -32.0, attack: 0.0, release: 0.0, hold_s: 0.2, held: 0.0, open: false, gain: 0.0 };
        g.update(settings)?;
        Ok(Box::new(g))
    }
}

struct GateInstance {
    open_db: f32,
    close_db: f32,
    /// Gain change per sample frame while opening and closing.
    attack: f32,
    release: f32,
    hold_s: f32,
    /// Seconds the gate has stayed open below the close threshold.
    held: f32,
    open: bool,
    gain: f32,
}

fn per_frame(ms: f32) -> f32 {
    1.0 / (ms.max(1.0) / 1000.0 * SAMPLE_RATE as f32)
}

impl FilterInstance for GateInstance {
    fn update(&mut self, s: &Settings) -> Result<(), PluginError> {
        self.open_db = float(s, "open_db", -26.0);
        self.close_db = float(s, "close_db", -32.0).min(self.open_db);
        self.attack = per_frame(s.i64("attack_ms").unwrap_or(25) as f32);
        self.release = per_frame(s.i64("release_ms").unwrap_or(150) as f32);
        self.hold_s = s.i64("hold_ms").unwrap_or(200).max(0) as f32 / 1000.0;
        Ok(())
    }
    fn process_audio(&mut self, block: &mut [f32]) {
        let peak = block.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        let level = to_db(peak);
        if level >= self.open_db {
            self.open = true;
            self.held = 0.0;
        } else if level < self.close_db && self.open {
            self.held += BLOCK_FRAMES as f32 / SAMPLE_RATE as f32;
            if self.held >= self.hold_s {
                self.open = false;
            }
        }
        let (target, step) = if self.open { (1.0, self.attack) } else { (0.0, self.release) };
        for frame in block.as_chunks_mut::<CHANNELS>().0 {
            self.gain = if self.gain < target { (self.gain + step).min(target) } else { (self.gain - step).max(target) };
            frame.iter_mut().for_each(|s| *s *= self.gain);
        }
    }
}

const COLOR_CORRECTION: &str = r"
fn cc_to_linear(v: vec3<f32>) -> vec3<f32> {
    return select(pow((v + 0.055) / 1.055, vec3<f32>(2.4)), v / 12.92, v <= vec3<f32>(0.04045));
}

fn cc_from_linear(v: vec3<f32>) -> vec3<f32> {
    return select(1.055 * pow(v, vec3<f32>(1.0 / 2.4)) - 0.055, v * 12.92, v <= vec3<f32>(0.0031308));
}

fn effect(color: vec4<f32>, uv: vec2<f32>, size: vec2<f32>, p0: vec4<f32>, p1: vec4<f32>) -> vec4<f32> {
    // p0: brightness, contrast factor, saturation factor, gamma exponent. p1.x: opacity, p1.y: 1
    // when the white balance matrix (the first three columns of extra_params, linear light)
    // applies.
    let a = color.a;
    if a <= 0.0 { return vec4<f32>(0.0); }
    var c = max(color.rgb / a, vec3<f32>(0.0));
    if p1.y > 0.5 {
        let m = extra_params();
        let wb = mat3x3<f32>(m[0].xyz, m[1].xyz, m[2].xyz) * cc_to_linear(c);
        c = cc_from_linear(max(wb, vec3<f32>(0.0)));
    }
    c = pow(c, vec3<f32>(p0.w));
    c = (c - 0.5) * p0.y + 0.5 + p0.x;
    let luma = dot(c, vec3<f32>(0.2126, 0.7152, 0.0722));
    c = clamp(mix(vec3<f32>(luma), c, p0.z), vec3<f32>(0.0), vec3<f32>(1.0));
    let na = a * p1.x;
    return vec4<f32>(c * na, na);
}
";

/// White balance, brightness, contrast, saturation, gamma and opacity.
///
/// Temperature and tint name the light the picture was taken under; the filter makes a surface
/// lit by that light come out neutral in BT.709 / sRGB, whose white is D65 (Bradford adaptation
/// in linear light, `craft_color_transform::cct::wb_matrix`). Lower temperatures cool the
/// picture, and a positive tint adds magenta. At [`NEUTRAL_TEMPERATURE`] and [`NEUTRAL_TINT`],
/// the defaults, the filter leaves colors alone. Channels that the correction pushes above 1
/// clip, so under a strong correction the brightest whites take on the correction's color.
pub struct ColorCorrection;

/// The temperature of D65 as `cct::xy_to_temp_tint` reads it, to the nearest kelvin: the
/// filter's default.
pub const NEUTRAL_TEMPERATURE: f64 = 6502.0;
/// The tint of D65 (above the Planckian locus) as `cct::xy_to_temp_tint` reads it, to one
/// decimal: the filter's default.
pub const NEUTRAL_TINT: f64 = 9.5;

impl Plugin for ColorCorrection {
    fn id(&self) -> &'static str {
        "color_correction"
    }
    fn name(&self) -> &'static str {
        "Color correction"
    }
    fn schema(&self) -> SettingsSchema {
        SettingsSchema::new(vec![
            float_field("gamma", "Gamma", -3.0, 3.0, 0.01, 0.0),
            float_field("contrast", "Contrast", -1.0, 1.0, 0.01, 0.0),
            float_field("brightness", "Brightness", -1.0, 1.0, 0.01, 0.0),
            float_field("saturation", "Saturation", -1.0, 5.0, 0.01, 0.0),
            float_field("temperature", "Temperature (K)", 2000.0, 12000.0, 1.0, NEUTRAL_TEMPERATURE),
            float_field("tint", "Tint", -100.0, 100.0, 0.5, NEUTRAL_TINT),
            float_field("opacity", "Opacity (%)", 0.0, 100.0, 1.0, 100.0),
        ])
    }
}

impl Filter for ColorCorrection {
    fn target(&self) -> FilterTarget {
        FilterTarget::Video
    }
    fn create(&self, settings: &Settings) -> Result<Box<dyn FilterInstance>, PluginError> {
        let mut f = Effect { key: "color_correction", wgsl: COLOR_CORRECTION, params: [0.0; VIDEO_EFFECT_PARAMS], read: color_correction_params };
        f.update(settings)?;
        Ok(Box::new(f))
    }
}

/// A setting clamped to `min..=max`. A missing value reads as `default`; settings hold JSON
/// numbers, so a NaN or infinite value is stored as no value and reads as `default` too.
fn ranged(s: &Settings, key: &str, default: f64, min: f64, max: f64) -> f64 {
    s.f64(key).map_or(default, |v| v.clamp(min, max))
}

/// The white balance matrix for the filter's settings, in linear BT.709 / sRGB. The defaults
/// give the identity.
fn white_balance(s: &Settings) -> Mat3 {
    let temperature = ranged(s, "temperature", NEUTRAL_TEMPERATURE, 2000.0, 12000.0);
    let tint = ranged(s, "tint", NEUTRAL_TINT, -100.0, 100.0);
    if (temperature, tint) == (NEUTRAL_TEMPERATURE, NEUTRAL_TINT) {
        return Mat3::IDENTITY;
    }
    cct::wb_matrix(&SRGB, cct::temp_tint_to_xy(temperature, tint))
}

fn color_correction_params(s: &Settings) -> Params {
    let gamma = float(s, "gamma", 0.0).clamp(-3.0, 3.0);
    let exponent = if gamma >= 0.0 { 1.0 / (1.0 + gamma) } else { 1.0 - gamma };
    let contrast = float(s, "contrast", 0.0).clamp(-1.0, 1.0);
    let contrast = if contrast < 0.0 { 1.0 + contrast } else { 1.0 + 3.0 * contrast };
    let saturation = 1.0 + float(s, "saturation", 0.0).clamp(-1.0, 5.0);
    let opacity = float(s, "opacity", 100.0).clamp(0.0, 100.0) / 100.0;
    let mut p = [0.0; VIDEO_EFFECT_PARAMS];
    p[..5].copy_from_slice(&[float(s, "brightness", 0.0).clamp(-1.0, 1.0), contrast, saturation, exponent, opacity]);
    let wb = white_balance(s);
    if wb != Mat3::IDENTITY {
        p[5] = 1.0;
        // Column j of the matrix goes to extra_params()[j].
        for (j, col) in wb.transpose().to_f32().iter().enumerate() {
            p[8 + 4 * j..11 + 4 * j].copy_from_slice(col);
        }
    }
    p
}

const CHROMA_KEY: &str = r"
fn chroma_key_cbcr(c: vec3<f32>) -> vec2<f32> {
    return vec2<f32>(dot(c, vec3<f32>(-0.1146, -0.3854, 0.5)), dot(c, vec3<f32>(0.5, -0.4542, -0.0458)));
}

fn effect(color: vec4<f32>, uv: vec2<f32>, size: vec2<f32>, p0: vec4<f32>, p1: vec4<f32>) -> vec4<f32> {
    // p0.rgb: the key color. p1: similarity, smoothness, spill reduction.
    let a = color.a;
    if a <= 0.0 { return vec4<f32>(0.0); }
    let c = color.rgb / a;
    let d = distance(chroma_key_cbcr(c), chroma_key_cbcr(p0.rgb));
    let mask = clamp((d - p1.x) / max(p1.y, 0.0001), 0.0, 1.0);
    let spill = clamp(1.0 - (d - p1.x) / max(p1.z, 0.0001), 0.0, 1.0) * step(0.0001, p1.z);
    let luma = dot(c, vec3<f32>(0.2126, 0.7152, 0.0722));
    let rgb = mix(c, vec3<f32>(luma), spill);
    let na = a * mask;
    return vec4<f32>(rgb * na, na);
}
";

/// Makes one color (a green or blue screen) transparent.
pub struct ChromaKey;

impl Plugin for ChromaKey {
    fn id(&self) -> &'static str {
        "chroma_key"
    }
    fn name(&self) -> &'static str {
        "Chroma key"
    }
    fn schema(&self) -> SettingsSchema {
        SettingsSchema::new(vec![
            Field::new("key_color", "Key color", FieldKind::Color, "#00ff00ff"),
            Field::new("similarity", "Similarity", FieldKind::Int { min: 1, max: 1000 }, 400),
            Field::new("smoothness", "Smoothness", FieldKind::Int { min: 1, max: 1000 }, 80),
            Field::new("spill", "Key color spill reduction", FieldKind::Int { min: 0, max: 1000 }, 100),
        ])
    }
}

impl Filter for ChromaKey {
    fn target(&self) -> FilterTarget {
        FilterTarget::Video
    }
    fn create(&self, settings: &Settings) -> Result<Box<dyn FilterInstance>, PluginError> {
        let mut f = Effect { key: "chroma_key", wgsl: CHROMA_KEY, params: [0.0; VIDEO_EFFECT_PARAMS], read: chroma_key_params };
        f.update(settings)?;
        Ok(Box::new(f))
    }
}

fn chroma_key_params(s: &Settings) -> Params {
    let [r, g, b, _] = s.color("key_color").unwrap_or([0, 255, 0, 255]);
    let unit = |key: &str, default: i64| s.i64(key).unwrap_or(default).clamp(0, 1000) as f32 / 1000.0;
    let mut p = [0.0; VIDEO_EFFECT_PARAMS];
    p[..8].copy_from_slice(&[
        f32::from(r) / 255.0,
        f32::from(g) / 255.0,
        f32::from(b) / 255.0,
        0.0,
        unit("similarity", 400) * 0.5,
        unit("smoothness", 80) * 0.5,
        unit("spill", 100),
        0.0,
    ]);
    p
}

/// A video filter that is one GPU pass with parameters read from its settings.
struct Effect {
    key: &'static str,
    wgsl: &'static str,
    params: Params,
    read: fn(&Settings) -> Params,
}

impl FilterInstance for Effect {
    fn update(&mut self, s: &Settings) -> Result<(), PluginError> {
        self.params = (self.read)(s);
        Ok(())
    }
    fn video_effect(&self) -> Option<VideoEffect> {
        Some(VideoEffect { key: self.key, wgsl: self.wgsl, params: self.params })
    }
}

/// Every built-in video filter's effect with default settings, for checking that they compile.
pub fn builtin_video_effects() -> Vec<VideoEffect> {
    [&ColorCorrection as &dyn Filter, &ChromaKey]
        .iter()
        .filter_map(|f| f.create(&f.schema().defaults()).ok().and_then(|i| i.video_effect()))
        .collect()
}

#[cfg(test)]
mod tests {
    use studiocraft_media::audio::BLOCK_SAMPLES;

    use super::*;

    #[test]
    fn gain_scales_samples() {
        let mut g = Gain.create(&Settings::new().with("db", 6.0206)).unwrap();
        let mut b = vec![0.25f32; 4];
        g.process_audio(&mut b);
        assert!((b[0] - 0.5).abs() < 1e-3);
    }

    #[test]
    fn the_gate_closes_on_quiet_input_and_opens_on_loud_input() {
        let s = NoiseGate.schema().defaults().with("hold_ms", 0).with("release_ms", 1).with("attack_ms", 1);
        let mut g = NoiseGate.create(&s).unwrap();
        let mut quiet = vec![0.001f32; BLOCK_SAMPLES];
        g.process_audio(&mut quiet);
        assert_eq!(*quiet.last().unwrap(), 0.0);
        let mut loud = vec![0.5f32; BLOCK_SAMPLES];
        g.process_audio(&mut loud);
        assert!((*loud.last().unwrap() - 0.5).abs() < 1e-6);
    }

    /// One device for every test in this binary; tests run in parallel and opening many devices
    /// at once crashes some drivers. Machines without a graphics adapter get `None`, and the
    /// tests that need one pass without checking.
    fn gpu() -> Option<studiocraft_render::Gpu> {
        static GPU: std::sync::OnceLock<Option<studiocraft_render::Gpu>> = std::sync::OnceLock::new();
        GPU.get_or_init(|| studiocraft_render::Gpu::headless().ok()).clone()
    }

    #[test]
    fn video_filter_shaders_compile() {
        use std::sync::Arc;

        use studiocraft_model::SourceId;
        use studiocraft_plugin::{VideoContent, VideoOut};
        use studiocraft_render::{Compositor, ItemDesc, Plan, SceneDesc};

        let Some(gpu) = gpu() else { return };
        let mut c = Compositor::new(gpu, 16, 16).unwrap();
        let video = Arc::new(VideoOut::default());
        video.publish(VideoContent::Solid { rgba: [0, 255, 0, 255], width: 16, height: 16 });
        let item = ItemDesc { source: SourceId(1), video, transform: Default::default(), effects: builtin_video_effects() };
        c.set_plan(Arc::new(Plan { program: SceneDesc { items: vec![item] }, ..Plan::default() }));
        let errors = c.take_errors();
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn video_filters_have_effects_and_audio_filters_do_not() {
        assert_eq!(builtin_video_effects().len(), 2);
        assert!(Gain.create(&Settings::new()).unwrap().video_effect().is_none());
        let neutral = color_correction_params(&ColorCorrection.schema().defaults());
        let mut want = [0.0; VIDEO_EFFECT_PARAMS];
        want[..5].copy_from_slice(&[0.0, 1.0, 1.0, 1.0, 1.0]);
        assert_eq!(neutral, want);
    }

    /// The matrix columns in the parameters rebuild the white balance matrix.
    fn matrix_of(p: &Params) -> [[f32; 3]; 3] {
        let col = |j: usize| [p[8 + 4 * j], p[9 + 4 * j], p[10 + 4 * j]];
        let (c0, c1, c2) = (col(0), col(1), col(2));
        [[c0[0], c1[0], c2[0]], [c0[1], c1[1], c2[1]], [c0[2], c1[2], c2[2]]]
    }

    #[test]
    fn temperature_and_tint_set_the_white_balance_matrix() {
        let s = ColorCorrection.schema().defaults().with("temperature", 3200.0).with("tint", 10.0);
        let p = color_correction_params(&s);
        assert_eq!(p[5], 1.0);
        assert_eq!(matrix_of(&p), cct::wb_matrix(&SRGB, cct::temp_tint_to_xy(3200.0, 10.0)).to_f32());
    }

    /// A gray card lit by the light that temperature and tint name comes out neutral.
    #[test]
    fn a_gray_card_under_the_set_light_comes_out_neutral() {
        for (t, tint) in [(2000.0, 0.0), (3200.0, 10.0), (5600.0, -25.0), (6500.0, 0.0), (9000.0, 40.0), (12000.0, -100.0)] {
            let m = white_balance(&ColorCorrection.schema().defaults().with("temperature", t).with("tint", tint));
            let card = SRGB.from_xyz().apply(cct::temp_tint_to_xy(t, tint).to_xyz().unwrap());
            let out = m.apply(card);
            assert!((out[0] - out[1]).abs() < 1e-6 && (out[1] - out[2]).abs() < 1e-6, "{t} K {tint}: {out:?}");
        }
    }

    /// The defaults are D65's own temperature and tint: the identity, and settings next to them
    /// give a matrix next to the identity.
    #[test]
    fn the_default_white_balance_is_d65() {
        let (t, tint) = cct::xy_to_temp_tint(SRGB.white);
        assert!((t - NEUTRAL_TEMPERATURE).abs() <= 0.5 && (tint - NEUTRAL_TINT).abs() <= 0.05, "{t} {tint}");
        // Each slider snaps to its steps from its minimum; the default is one of them.
        let schema = ColorCorrection.schema();
        for (key, default) in [("temperature", NEUTRAL_TEMPERATURE), ("tint", NEUTRAL_TINT)] {
            let Some(FieldKind::Float { min, step, .. }) = schema.field(key).map(|f| &f.kind) else { panic!("no {key} field") };
            assert_eq!(((default - min) / step).fract(), 0.0, "{key}");
        }
        let defaults = schema.defaults();
        assert_eq!(white_balance(&defaults), Mat3::IDENTITY);
        let near = white_balance(&defaults.with("temperature", NEUTRAL_TEMPERATURE + 1.0));
        assert!(near.0.iter().flatten().zip(Mat3::IDENTITY.0.iter().flatten()).all(|(a, b)| (a - b).abs() < 1e-3), "{near:?}");
    }

    #[test]
    fn hostile_white_balance_settings_read_as_the_default_or_the_range_end() {
        let defaults = ColorCorrection.schema().defaults();
        let neutral = color_correction_params(&defaults);
        let at = |key: &str, v: f64| color_correction_params(&defaults.clone().with(key, v));
        for (key, low, high) in [("temperature", 2000.0, 12000.0), ("tint", -100.0, 100.0)] {
            // Values that are not finite are stored as no value.
            for v in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
                assert_eq!(at(key, v), neutral, "{key} {v}");
            }
            assert_eq!(at(key, -1.0e9), at(key, low));
            assert_eq!(at(key, 1.0e9), at(key, high));
            assert!(at(key, 1.0e300).iter().all(|v| v.is_finite()));
        }
    }

    /// The shader's white balance (sRGB decode, matrix, encode) gives the colors the matrix
    /// gives on the CPU.
    #[test]
    fn the_gpu_white_balance_matches_the_matrix() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicU64;

        use studiocraft_media::Tick;
        use studiocraft_model::SourceId;
        use studiocraft_plugin::{VideoContent, VideoOut};
        use studiocraft_render::{Compositor, ItemDesc, Plan, SceneDesc};

        let Some(gpu) = gpu() else { return };
        let mut c = Compositor::new(gpu, 4, 4).unwrap();
        let s = ColorCorrection.schema().defaults().with("temperature", 3200.0).with("tint", -20.0);
        let effect = ColorCorrection.create(&s).unwrap().video_effect().unwrap();
        let m = white_balance(&s);
        // White leaves the matrix with a blue channel above 1, which clips.
        assert!(m.apply([1.0; 3])[2] > 1.0, "{:?}", m.apply([1.0; 3]));
        for rgba in [[230, 150, 90, 255], [128, 128, 128, 255], [20, 200, 60, 255], [255, 255, 255, 255]] {
            let video = Arc::new(VideoOut::default());
            video.publish(VideoContent::Solid { rgba, width: 4, height: 4 });
            let item = ItemDesc { source: SourceId(1), video, transform: Default::default(), effects: vec![effect] };
            c.set_plan(Arc::new(Plan { program: SceneDesc { items: vec![item] }, ..Plan::default() }));
            c.frame(Tick(0), &AtomicU64::new(0));
            let px = c.read_program().unwrap();
            let lin = [0, 1, 2].map(|i| f64::from(craft_color::srgb_to_linear(f32::from(rgba[i]) / 255.0)));
            let want = m.apply(lin).map(|v| (craft_color::linear_to_srgb(v.clamp(0.0, 1.0) as f32) * 255.0).round() as i32);
            for i in 0..3 {
                assert!((i32::from(px[i]) - want[i]).abs() <= 2, "{rgba:?}: {:?} vs {want:?}", &px[..4]);
            }
        }
    }
}
