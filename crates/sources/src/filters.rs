//! Built-in filters.

use craft_audio_dsp::{AudioEffect, create_effect};
use craft_color_transform::{Mat3, SRGB, cct};
use studiocraft_media::audio::{BLOCK_FRAMES, CHANNELS, SAMPLE_RATE};
use studiocraft_media::mix::{from_db, to_db};
use studiocraft_model::{ChoiceOption, Field, FieldKind, Settings, SettingsSchema};
use studiocraft_plugin::{Filter, FilterInstance, FilterTarget, MAX_EFFECT_SIDE, Plugin, PluginError, Reframe, VIDEO_EFFECT_PARAMS, VideoEffect};

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
        let (frames, part) = block.as_chunks_mut::<CHANNELS>();
        for frame in frames {
            self.gain = if self.gain < target { (self.gain + step).min(target) } else { (self.gain - step).max(target) };
            frame.iter_mut().for_each(|s| *s *= self.gain);
        }
        // A trailing part frame is silenced, as the effect filters do.
        part.fill(0.0);
    }
}

/// One setting of a filter built on a `craft_audio_dsp` effect: the setting's key, the effect
/// parameter it sets, its default, its range and whether it takes whole numbers only, the same
/// as the filter's schema field.
struct DspSetting {
    key: &'static str,
    param: &'static str,
    default: f64,
    min: f64,
    max: f64,
    int: bool,
}

impl DspSetting {
    /// The setting's value, read by [`checked`].
    fn read(&self, s: &Settings) -> Result<f32, PluginError> {
        checked(s, self.key, self.default, self.min, self.max, self.int).map(|v| v as f32)
    }
}

/// A number setting: the default when the key is missing, and an error when the value is not a
/// number (settings store a NaN or infinite number as `null`), is not a whole number for a
/// whole-number setting, or is outside `min..=max`.
fn checked(s: &Settings, key: &str, default: f64, min: f64, max: f64, int: bool) -> Result<f64, PluginError> {
    let Some(raw) = s.get(key) else { return Ok(default) };
    let Some(v) = raw.as_f64().filter(|v| v.is_finite()) else {
        return Err(PluginError::Failed(format!("`{key}` must be a number, not {raw}")));
    };
    if int && v.fract() != 0.0 {
        return Err(PluginError::Failed(format!("`{key}` must be a whole number, not {v}")));
    }
    if !(min..=max).contains(&v) {
        return Err(PluginError::Failed(format!("`{key}` is {v}; it must be from {min} to {max}")));
    }
    Ok(v)
}

/// A whole-number setting from `min` to `max`, read by [`checked`].
fn checked_int(s: &Settings, key: &str, default: i64, min: i64, max: i64) -> Result<i64, PluginError> {
    checked(s, key, default as f64, min as f64, max as f64, true).map(|v| v as i64)
}

/// A choice setting: the default when the key is missing, and an error when the value is not
/// one of `options`.
fn checked_choice<'a>(s: &Settings, key: &str, default: &'a str, options: &[&'a str]) -> Result<&'a str, PluginError> {
    let Some(raw) = s.get(key) else { return Ok(default) };
    raw.as_str()
        .and_then(|v| options.iter().find(|o| **o == v).copied())
        .ok_or_else(|| PluginError::Failed(format!("`{key}` must be one of {}, not {raw}", options.join(", "))))
}

/// A color setting (`#rrggbb` or `#rrggbbaa`): the default when the key is missing, and an error
/// when the value is not a color.
fn checked_color(s: &Settings, key: &str, default: [u8; 4]) -> Result<[u8; 4], PluginError> {
    let Some(raw) = s.get(key) else { return Ok(default) };
    s.color(key).ok_or_else(|| PluginError::Failed(format!("`{key}` must be a color such as #00ff00, not {raw}")))
}

// The audio processing below splits a block into a left and a right buffer.
const _: () = assert!(CHANNELS == 2, "DspFilter assumes stereo blocks");

/// A filter that runs a `craft_audio_dsp` effect on the source's stereo audio.
struct DspFilter {
    fx: Box<dyn AudioEffect>,
    settings: &'static [DspSetting],
    /// The block split into its two channels; sized when the filter is created.
    left: Vec<f32>,
    right: Vec<f32>,
}

impl DspFilter {
    fn create(effect: &str, settings: &'static [DspSetting], s: &Settings) -> Result<Box<dyn FilterInstance>, PluginError> {
        let fx = create_effect(effect, SAMPLE_RATE as f32, CHANNELS).ok_or_else(|| PluginError::Failed(format!("no audio effect `{effect}`")))?;
        let mut f = DspFilter { fx, settings, left: vec![0.0; BLOCK_FRAMES], right: vec![0.0; BLOCK_FRAMES] };
        f.update(s)?;
        Ok(Box::new(f))
    }
}

impl FilterInstance for DspFilter {
    fn update(&mut self, s: &Settings) -> Result<(), PluginError> {
        // Check every setting before changing any, so a rejected update leaves the filter as it was.
        let mut values = [0.0f32; MAX_DSP_SETTINGS];
        for (v, d) in values.iter_mut().zip(self.settings) {
            *v = d.read(s)?;
        }
        for (v, d) in values.iter().zip(self.settings) {
            self.fx.set_param(d.param, *v);
        }
        Ok(())
    }
    fn process_audio(&mut self, block: &mut [f32]) {
        // A trailing part frame has no partner sample; it is silenced rather than passed around
        // the effect, so it cannot get past a limiter's ceiling.
        let whole = block.len() - block.len() % CHANNELS;
        let (block, part) = block.split_at_mut(whole);
        part.fill(0.0);
        for chunk in block.chunks_mut(BLOCK_FRAMES * CHANNELS) {
            let frames = chunk.len() / CHANNELS;
            let (left, right) = (&mut self.left[..frames], &mut self.right[..frames]);
            for (i, f) in chunk.as_chunks::<CHANNELS>().0.iter().enumerate() {
                left[i] = f[0];
                right[i] = f[1];
            }
            self.fx.process(&mut [&mut *left, &mut *right]);
            for (i, f) in chunk.as_chunks_mut::<CHANNELS>().0.iter_mut().enumerate() {
                f[0] = left[i];
                f[1] = right[i];
            }
        }
    }
}

/// The most settings one `DspFilter` has.
const MAX_DSP_SETTINGS: usize = 5;

const COMPRESSOR: &[DspSetting] = &[
    DspSetting { key: "threshold_db", param: "threshold", default: -18.0, min: -60.0, max: 0.0, int: false },
    DspSetting { key: "ratio", param: "ratio", default: 3.0, min: 1.0, max: 30.0, int: false },
    DspSetting { key: "attack_ms", param: "attack", default: 6.0, min: 1.0, max: 300.0, int: true },
    DspSetting { key: "release_ms", param: "release", default: 60.0, min: 1.0, max: 3000.0, int: true },
    DspSetting { key: "output_db", param: "makeup", default: 0.0, min: -32.0, max: 32.0, int: false },
];
const _: () = assert!(COMPRESSOR.len() <= MAX_DSP_SETTINGS);

/// Turns down a source's level above a threshold by a ratio (a 6 dB knee, peak detection).
pub struct Compressor;

impl Plugin for Compressor {
    fn id(&self) -> &'static str {
        "compressor"
    }
    fn name(&self) -> &'static str {
        "Compressor"
    }
    fn schema(&self) -> SettingsSchema {
        SettingsSchema::new(vec![
            float_field("threshold_db", "Threshold (dB)", -60.0, 0.0, 0.1, -18.0),
            float_field("ratio", "Ratio", 1.0, 30.0, 0.1, 3.0),
            Field::new("attack_ms", "Attack (ms)", FieldKind::Int { min: 1, max: 300 }, 6),
            Field::new("release_ms", "Release (ms)", FieldKind::Int { min: 1, max: 3000 }, 60),
            float_field("output_db", "Output gain (dB)", -32.0, 32.0, 0.1, 0.0),
        ])
    }
}

impl Filter for Compressor {
    fn target(&self) -> FilterTarget {
        FilterTarget::Audio
    }
    fn create(&self, settings: &Settings) -> Result<Box<dyn FilterInstance>, PluginError> {
        DspFilter::create("compressor", COMPRESSOR, settings)
    }
}

const LIMITER: &[DspSetting] = &[
    DspSetting { key: "ceiling_db", param: "ceiling", default: -1.0, min: -24.0, max: 0.0, int: false },
    DspSetting { key: "release_ms", param: "release", default: 60.0, min: 1.0, max: 1000.0, int: true },
];
const _: () = assert!(LIMITER.len() <= MAX_DSP_SETTINGS);

/// Keeps a source's true peak at or below a ceiling. It looks 5 ms ahead and its true-peak
/// filter adds 8 samples, so the source's audio plays 248 samples (about 5.2 ms) later than
/// without it.
pub struct Limiter;

impl Plugin for Limiter {
    fn id(&self) -> &'static str {
        "limiter"
    }
    fn name(&self) -> &'static str {
        "Limiter"
    }
    fn schema(&self) -> SettingsSchema {
        SettingsSchema::new(vec![
            float_field("ceiling_db", "Ceiling (dB)", -24.0, 0.0, 0.1, -1.0),
            Field::new("release_ms", "Release (ms)", FieldKind::Int { min: 1, max: 1000 }, 60),
        ])
    }
}

impl Filter for Limiter {
    fn target(&self) -> FilterTarget {
        FilterTarget::Audio
    }
    fn create(&self, settings: &Settings) -> Result<Box<dyn FilterInstance>, PluginError> {
        DspFilter::create("limiter", LIMITER, settings)
    }
}

const EQUALIZER: &[DspSetting] = &[
    DspSetting { key: "low_db", param: "low", default: 0.0, min: -24.0, max: 24.0, int: false },
    DspSetting { key: "mid_db", param: "mid", default: 0.0, min: -24.0, max: 24.0, int: false },
    DspSetting { key: "high_db", param: "high", default: 0.0, min: -24.0, max: 24.0, int: false },
];
const _: () = assert!(EQUALIZER.len() <= MAX_DSP_SETTINGS);

/// Three bands: a low shelf at 200 Hz, a peak at 1 kHz and a high shelf at 5 kHz.
pub struct Equalizer;

impl Plugin for Equalizer {
    fn id(&self) -> &'static str {
        "equalizer"
    }
    fn name(&self) -> &'static str {
        "Equalizer"
    }
    fn schema(&self) -> SettingsSchema {
        SettingsSchema::new(vec![
            float_field("low_db", "Low (dB)", -24.0, 24.0, 0.1, 0.0),
            float_field("mid_db", "Mid (dB)", -24.0, 24.0, 0.1, 0.0),
            float_field("high_db", "High (dB)", -24.0, 24.0, 0.1, 0.0),
        ])
    }
}

impl Filter for Equalizer {
    fn target(&self) -> FilterTarget {
        FilterTarget::Audio
    }
    fn create(&self, settings: &Settings) -> Result<Box<dyn FilterInstance>, PluginError> {
        DspFilter::create("simple_eq", EQUALIZER, settings)
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
        Effect::create("color_correction", COLOR_CORRECTION, |s| Ok((color_correction_params(s), Reframe::None)), settings)
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
        Effect::create("chroma_key", CHROMA_KEY, |s| Ok((chroma_key_params(s), Reframe::None)), settings)
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

const COLOR_KEY: &str = r"
fn effect(color: vec4<f32>, uv: vec2<f32>, size: vec2<f32>, p0: vec4<f32>, p1: vec4<f32>) -> vec4<f32> {
    // p0: the key color, brightness. p1: similarity, smoothness, contrast factor, gamma
    // exponent. extra_params()[0].x: opacity.
    let a = color.a;
    if a <= 0.0 { return vec4<f32>(0.0); }
    let c = max(color.rgb / a, vec3<f32>(0.0));
    let mask = clamp((distance(c, p0.rgb) - p1.x) / max(p1.y, 0.0001), 0.0, 1.0);
    var o = pow(c, vec3<f32>(p1.w));
    o = clamp((o - 0.5) * p1.z + 0.5 + p0.w, vec3<f32>(0.0), vec3<f32>(1.0));
    let na = a * mask * extra_params()[0].x;
    return vec4<f32>(o * na, na);
}
";

/// Makes the pixels close to one color transparent, measured as the distance between the two
/// colors in RGB (each channel from 0 to 1): pixels within Similarity / 1000 are keyed out, and
/// the next Smoothness / 1000 fades back in. The pixels left are adjusted by Gamma, Contrast and
/// Brightness and drawn at Opacity.
pub struct ColorKey;

/// The default key color, green.
pub const COLOR_KEY_DEFAULT: [u8; 4] = [0, 255, 0, 255];

impl Plugin for ColorKey {
    fn id(&self) -> &'static str {
        "color_key"
    }
    fn name(&self) -> &'static str {
        "Color key"
    }
    fn schema(&self) -> SettingsSchema {
        SettingsSchema::new(vec![
            Field::new("key_color", "Key color", FieldKind::Color, "#00ff00ff"),
            Field::new("similarity", "Similarity", FieldKind::Int { min: 1, max: 1000 }, 400),
            Field::new("smoothness", "Smoothness", FieldKind::Int { min: 1, max: 1000 }, 80),
            float_field("opacity", "Opacity (%)", 0.0, 100.0, 1.0, 100.0),
            float_field("contrast", "Contrast", -1.0, 1.0, 0.01, 0.0),
            float_field("brightness", "Brightness", -1.0, 1.0, 0.01, 0.0),
            float_field("gamma", "Gamma", -1.0, 1.0, 0.01, 0.0),
        ])
    }
}

impl Filter for ColorKey {
    fn target(&self) -> FilterTarget {
        FilterTarget::Video
    }
    fn create(&self, settings: &Settings) -> Result<Box<dyn FilterInstance>, PluginError> {
        Effect::create("color_key", COLOR_KEY, color_key_params, settings)
    }
}

fn color_key_params(s: &Settings) -> Result<(Params, Reframe), PluginError> {
    let [r, g, b, _] = checked_color(s, "key_color", COLOR_KEY_DEFAULT)?;
    let similarity = checked_int(s, "similarity", 400, 1, 1000)?;
    let smoothness = checked_int(s, "smoothness", 80, 1, 1000)?;
    let opacity = checked(s, "opacity", 100.0, 0.0, 100.0, false)?;
    let contrast = checked(s, "contrast", 0.0, -1.0, 1.0, false)?;
    let brightness = checked(s, "brightness", 0.0, -1.0, 1.0, false)?;
    let gamma = checked(s, "gamma", 0.0, -1.0, 1.0, false)?;
    // The same curves as color correction.
    let contrast = if contrast < 0.0 { 1.0 + contrast } else { 1.0 + 3.0 * contrast };
    let exponent = if gamma >= 0.0 { 1.0 / (1.0 + gamma) } else { 1.0 - gamma };
    let mut p = [0.0; VIDEO_EFFECT_PARAMS];
    p[..9].copy_from_slice(&[
        f32::from(r) / 255.0,
        f32::from(g) / 255.0,
        f32::from(b) / 255.0,
        brightness as f32,
        similarity as f32 / 1000.0,
        smoothness as f32 / 1000.0,
        contrast as f32,
        exponent as f32,
        (opacity / 100.0) as f32,
    ]);
    Ok((p, Reframe::None))
}

const CROP_PAD: &str = r"
fn effect(color: vec4<f32>, uv: vec2<f32>, size: vec2<f32>, p0: vec4<f32>, p1: vec4<f32>) -> vec4<f32> {
    // The compositor places the input; padding arrives transparent.
    return color;
}
";

/// Takes pixels off a source's edges or adds transparent pixels around it, or shows one area of
/// it. The source's size becomes the filter's output size, in every scene that shows it.
///
/// In the "edges" mode, a positive Left, Top, Right or Bottom takes that many pixels off the
/// side and a negative one adds that many transparent pixels. In the "area" mode, X and Y are
/// the top-left corner of the area shown and Width and Height its size; 0 reaches to the
/// source's right or bottom edge, and parts of the area outside the source are transparent.
/// Neither side of the output is longer than [`MAX_EFFECT_SIDE`] pixels.
pub struct CropPad;

/// The modes of [`CropPad`].
const CROP_PAD_MODES: &[&str] = &["edges", "area"];
const SIDE: i64 = MAX_EFFECT_SIDE as i64;

impl Plugin for CropPad {
    fn id(&self) -> &'static str {
        "crop_pad"
    }
    fn name(&self) -> &'static str {
        "Crop and pad"
    }
    fn schema(&self) -> SettingsSchema {
        let int = |key: &str, label: &str, min: i64| Field::new(key, label, FieldKind::Int { min, max: SIDE }, 0);
        SettingsSchema::new(vec![
            Field::new(
                "mode",
                "Mode",
                FieldKind::Choice { options: vec![ChoiceOption::new("edges", "Crop or pad each edge"), ChoiceOption::new("area", "Show an area")] },
                "edges",
            ),
            int("left", "Left", -SIDE),
            int("top", "Top", -SIDE),
            int("right", "Right", -SIDE),
            int("bottom", "Bottom", -SIDE),
            int("x", "X", 0),
            int("y", "Y", 0),
            int("width", "Width", 0),
            int("height", "Height", 0),
        ])
    }
}

impl Filter for CropPad {
    fn target(&self) -> FilterTarget {
        FilterTarget::Video
    }
    fn create(&self, settings: &Settings) -> Result<Box<dyn FilterInstance>, PluginError> {
        Effect::create("crop_pad", CROP_PAD, crop_pad_frame, settings)
    }
}

fn crop_pad_frame(s: &Settings) -> Result<(Params, Reframe), PluginError> {
    let mode = checked_choice(s, "mode", "edges", CROP_PAD_MODES)?;
    // Every setting is checked, whichever mode reads it, so a bad value is never kept unseen.
    let edge = |key: &str| checked_int(s, key, 0, -SIDE, SIDE).map(|v| v as i32);
    let (left, top, right, bottom) = (edge("left")?, edge("top")?, edge("right")?, edge("bottom")?);
    let area = |key: &str| checked_int(s, key, 0, 0, SIDE).map(|v| v as u32);
    let (x, y, width, height) = (area("x")?, area("y")?, area("width")?, area("height")?);
    let reframe = if mode == "area" { Reframe::Area { x, y, width, height } } else { Reframe::Edges { left, top, right, bottom } };
    Ok(([0.0; VIDEO_EFFECT_PARAMS], reframe))
}

const SHARPEN: &str = r"
fn effect(color: vec4<f32>, uv: vec2<f32>, size: vec2<f32>, p0: vec4<f32>, p1: vec4<f32>) -> vec4<f32> {
    // p0.x: how many times the difference from the eight neighbors' average is added.
    let px = uv * size;
    var sum = vec4<f32>(0.0);
    for (var dy = -1; dy <= 1; dy++) {
        for (var dx = -1; dx <= 1; dx++) {
            if dx != 0 || dy != 0 {
                sum += input_pixel(px + vec2<f32>(f32(dx), f32(dy)));
            }
        }
    }
    let rgb = color.rgb + (color.rgb - sum.rgb / 8.0) * p0.x;
    return vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(color.a)), color.a);
}
";

/// Makes edges in a source's picture crisper: each pixel moves away from the average of its
/// eight neighbors by [`SHARPEN_GAIN`] times Sharpness. Pixels on the picture's edge count
/// themselves in place of the neighbors outside it. Transparency is unchanged.
pub struct Sharpen;

/// How many times the difference from the neighbors' average a Sharpness of 1 adds.
pub const SHARPEN_GAIN: f64 = 4.0;
/// The default Sharpness.
pub const SHARPEN_DEFAULT: f64 = 0.08;

impl Plugin for Sharpen {
    fn id(&self) -> &'static str {
        "sharpen"
    }
    fn name(&self) -> &'static str {
        "Sharpen"
    }
    fn schema(&self) -> SettingsSchema {
        SettingsSchema::new(vec![float_field("sharpness", "Sharpness", 0.0, 1.0, 0.01, SHARPEN_DEFAULT)])
    }
}

impl Filter for Sharpen {
    fn target(&self) -> FilterTarget {
        FilterTarget::Video
    }
    fn create(&self, settings: &Settings) -> Result<Box<dyn FilterInstance>, PluginError> {
        Effect::create("sharpen", SHARPEN, sharpen_params, settings)
    }
}

fn sharpen_params(s: &Settings) -> Result<(Params, Reframe), PluginError> {
    let sharpness = checked(s, "sharpness", SHARPEN_DEFAULT, 0.0, 1.0, false)?;
    let mut p = [0.0; VIDEO_EFFECT_PARAMS];
    p[0] = (sharpness * SHARPEN_GAIN) as f32;
    Ok((p, Reframe::None))
}

/// Reads a video filter's pass from its settings.
type ReadPass = fn(&Settings) -> Result<(Params, Reframe), PluginError>;

/// A video filter that is one GPU pass with parameters read from its settings.
struct Effect {
    key: &'static str,
    wgsl: &'static str,
    params: Params,
    reframe: Reframe,
    read: ReadPass,
}

impl Effect {
    fn create(key: &'static str, wgsl: &'static str, read: ReadPass, s: &Settings) -> Result<Box<dyn FilterInstance>, PluginError> {
        let mut f = Effect { key, wgsl, params: [0.0; VIDEO_EFFECT_PARAMS], reframe: Reframe::None, read };
        f.update(s)?;
        Ok(Box::new(f))
    }
}

impl FilterInstance for Effect {
    /// A rejected update leaves the pass as it was.
    fn update(&mut self, s: &Settings) -> Result<(), PluginError> {
        (self.params, self.reframe) = (self.read)(s)?;
        Ok(())
    }
    fn video_effect(&self) -> Option<VideoEffect> {
        Some(VideoEffect { reframe: self.reframe, ..VideoEffect::new(self.key, self.wgsl, self.params) })
    }
}

/// Every built-in video filter's effect with default settings, for checking that they compile.
pub fn builtin_video_effects() -> Vec<VideoEffect> {
    [&ColorCorrection as &dyn Filter, &ChromaKey, &ColorKey, &CropPad, &Sharpen]
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

    /// `blocks` stereo blocks of a `freq` Hz tone at `amp`, the same on both channels.
    fn tone(freq: f64, amp: f32, blocks: usize) -> Vec<f32> {
        (0..blocks * BLOCK_FRAMES)
            .flat_map(|i| {
                let v = amp * (2.0 * std::f64::consts::PI * freq * i as f64 / f64::from(SAMPLE_RATE)).sin() as f32;
                [v, v]
            })
            .collect()
    }

    /// Run `f` over `x` block by block and return the peak of the last `tail` blocks.
    fn run_peak(f: &mut dyn FilterInstance, x: &mut [f32], tail: usize) -> f32 {
        x.chunks_mut(BLOCK_SAMPLES).for_each(|b| f.process_audio(b));
        x[x.len() - tail * BLOCK_SAMPLES..].iter().fold(0.0f32, |m, v| m.max(v.abs()))
    }

    #[test]
    fn the_compressor_turns_a_loud_tone_down_by_its_ratio() {
        // −6 dBFS peak through a −18 dB threshold at 4:1: 12 dB over becomes 3 dB over, −15 dBFS.
        // The peak detector relaxes a little between the tone's peaks, so the peaks come out
        // up to about 1 dB higher.
        let s = Compressor.schema().defaults().with("ratio", 4.0);
        let mut c = Compressor.create(&s).unwrap();
        let mut x = tone(1000.0, from_db(-6.0), 40);
        let peak = to_db(run_peak(c.as_mut(), &mut x, 4));
        assert!((-15.5..=-13.5).contains(&peak), "{peak} dBFS");
        // A quiet tone passes unchanged.
        let mut q = tone(1000.0, from_db(-40.0), 40);
        let peak = to_db(run_peak(c.as_mut(), &mut q, 4));
        assert!((peak - -40.0).abs() < 0.1, "{peak} dBFS");
    }

    #[test]
    fn the_limiter_keeps_the_peak_under_its_ceiling() {
        let s = Limiter.schema().defaults().with("ceiling_db", -6.0);
        let mut l = Limiter.create(&s).unwrap();
        let mut x = tone(997.0, 1.0, 20);
        let peak = to_db(run_peak(l.as_mut(), &mut x, 10));
        assert!((-6.5..=-6.0).contains(&peak), "{peak} dBFS");
    }

    #[test]
    fn the_equalizer_boosts_and_cuts_its_bands() {
        let level = |key: &str, db: f64, freq: f64| {
            let mut e = Equalizer.create(&Equalizer.schema().defaults().with(key, db)).unwrap();
            to_db(run_peak(e.as_mut(), &mut tone(freq, 0.1, 20), 4)) - to_db(0.1)
        };
        assert!((level("low_db", 6.0, 50.0) - 6.0).abs() < 0.5);
        assert!((level("mid_db", -6.0, 1000.0) - -6.0).abs() < 0.5);
        assert!((level("high_db", 6.0, 15_000.0) - 6.0).abs() < 0.5);
        // At the defaults the equalizer is flat.
        assert!(level("low_db", 0.0, 1000.0).abs() < 0.05);
    }

    /// The microphone chain: gate, compressor, limiter and equalizer, one after another on the
    /// same blocks. With settings at their range ends, a loud tone comes out finite and under
    /// the limiter's ceiling.
    #[test]
    fn a_microphone_chain_runs_on_the_same_blocks() {
        let extreme = Settings::new().with("ratio", 30.0).with("ceiling_db", 0.0).with("high_db", -24.0);
        let filters: [&dyn Filter; 4] = [&NoiseGate, &Compressor, &Limiter, &Equalizer];
        let mut chain: Vec<Box<dyn FilterInstance>> = filters.iter().map(|f| f.create(&extreme).unwrap()).collect();
        let mut x = tone(1000.0, 0.9, 40);
        for b in x.chunks_mut(BLOCK_SAMPLES) {
            chain.iter_mut().for_each(|f| f.process_audio(b));
        }
        let tail = &x[x.len() - 4 * BLOCK_SAMPLES..];
        let peak = tail.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(tail.iter().all(|v| v.is_finite()));
        // −0.9 dBFS through the default −18 dB threshold at 30:1 is about −17 dBFS.
        assert!((-19.0..=-15.0).contains(&to_db(peak)), "{} dBFS", to_db(peak));
        // A block shorter than the mixer's is processed, and the stray sample at the end of an
        // odd-length block is silenced instead of passing the limiter's ceiling untouched.
        for f in &mut chain {
            let mut odd = [0.5f32; 7];
            f.process_audio(&mut odd);
            assert_eq!(odd[6], 0.0);
            f.process_audio(&mut []);
        }
    }

    /// A supplied setting that is out of range, not a number (a NaN is stored as `null`) or not a
    /// whole number for a whole-number setting is an error, and the filter keeps its previous
    /// settings; a missing one reads as the default.
    #[test]
    fn out_of_range_settings_are_errors() {
        let bad = [
            (&Compressor as &dyn Filter, "ratio", serde_json::json!(1e300)),
            (&Compressor, "threshold_db", serde_json::json!(1.0)),
            (&Compressor, "threshold_db", serde_json::Value::from(f64::NAN)),
            (&Compressor, "attack_ms", serde_json::json!("fast")),
            (&Compressor, "attack_ms", serde_json::json!(6.5)),
            (&Limiter, "release_ms", serde_json::json!(1.5)),
            (&Limiter, "ceiling_db", serde_json::json!(-1e9)),
            (&Limiter, "release_ms", serde_json::json!(0)),
            (&Equalizer, "high_db", serde_json::json!(-1e9)),
            (&Equalizer, "low_db", serde_json::json!(true)),
        ];
        for (filter, key, value) in bad {
            let s = Settings::new().with(key, value.clone());
            assert!(filter.create(&s).is_err(), "{} {key} {value}", filter.id());
            let mut f = filter.create(&Settings::new()).unwrap();
            assert!(f.update(&s).is_err(), "{} {key} {value}", filter.id());
        }
        // A rejected update leaves every setting as it was.
        let mut c = Compressor.create(&Compressor.schema().defaults().with("ratio", 4.0)).unwrap();
        assert!(c.update(&Settings::new().with("ratio", 2.0).with("threshold_db", 5.0)).is_err());
        let mut x = tone(1000.0, from_db(-6.0), 40);
        let peak = to_db(run_peak(c.as_mut(), &mut x, 4));
        assert!((-15.5..=-13.5).contains(&peak), "{peak} dBFS: the 4:1 ratio still applies");
        // Missing settings and the range ends are accepted.
        for f in [&Compressor as &dyn Filter, &Limiter, &Equalizer] {
            assert!(f.create(&Settings::new()).is_ok());
            for field in &f.schema().fields {
                let (min, max) = match field.kind {
                    FieldKind::Float { min, max, .. } => (min, max),
                    FieldKind::Int { min, max } => (min as f64, max as f64),
                    _ => continue,
                };
                for v in [min, max] {
                    assert!(f.create(&Settings::new().with(&field.key, v)).is_ok(), "{} {} {v}", f.id(), field.key);
                }
            }
        }
    }

    /// Each built-in effect filter's ranges and defaults are its schema's.
    #[test]
    fn dsp_settings_match_the_schemas() {
        for (f, table) in [(&Compressor as &dyn Filter, COMPRESSOR), (&Limiter, LIMITER), (&Equalizer, EQUALIZER)] {
            let schema = f.schema();
            assert_eq!(schema.fields.len(), table.len(), "{}", f.id());
            for d in table {
                let field = schema.field(d.key).unwrap_or_else(|| panic!("{} has no {}", f.id(), d.key));
                let (min, max, int) = match field.kind {
                    FieldKind::Float { min, max, .. } => (min, max, false),
                    FieldKind::Int { min, max } => (min as f64, max as f64, true),
                    _ => panic!("{} {} is not a number", f.id(), d.key),
                };
                assert_eq!((min, max, int), (d.min, d.max, d.int), "{} {}", f.id(), d.key);
                assert_eq!(field.default.as_f64(), Some(d.default), "{} {}", f.id(), d.key);
            }
        }
    }

    /// The limiter delays its source by its look-ahead (5 ms, 240 samples) plus the true-peak
    /// filter's 8 samples: an impulse comes out 248 samples later.
    #[test]
    fn the_limiter_delays_its_source_by_248_samples() {
        assert_eq!(create_effect("limiter", SAMPLE_RATE as f32, CHANNELS).map(|fx| fx.latency()), Some(248));
        let mut l = Limiter.create(&Settings::new()).unwrap();
        let mut x = vec![0.0f32; 2 * BLOCK_SAMPLES];
        x[0] = 0.1;
        x[1] = 0.1;
        x.chunks_mut(BLOCK_SAMPLES).for_each(|b| l.process_audio(b));
        let at = x.iter().step_by(CHANNELS).enumerate().max_by(|a, b| a.1.abs().total_cmp(&b.1.abs())).map(|(i, _)| i);
        assert_eq!(at, Some(248));
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

    fn crop_pad(s: &Settings) -> Result<Reframe, PluginError> {
        CropPad.create(s).map(|f| f.video_effect().map_or(Reframe::None, |e| e.reframe))
    }

    #[test]
    fn crop_and_pad_reads_edges_and_areas() {
        let defaults = CropPad.schema().defaults();
        assert_eq!(crop_pad(&defaults).unwrap(), Reframe::Edges { left: 0, top: 0, right: 0, bottom: 0 });
        assert_eq!(crop_pad(&defaults).unwrap().size((640, 360)), (640, 360), "the defaults keep the size");
        assert_eq!(crop_pad(&Settings::new()).unwrap(), crop_pad(&defaults).unwrap(), "missing settings read as the defaults");
        let edges = defaults.clone().with("left", 10).with("top", -20).with("right", 30).with("bottom", -40);
        assert_eq!(crop_pad(&edges).unwrap(), Reframe::Edges { left: 10, top: -20, right: 30, bottom: -40 });
        assert_eq!(crop_pad(&edges).unwrap().size((640, 360)), (600, 420));
        // The area mode reads X, Y, Width and Height and leaves the edges alone.
        let area = edges.with("mode", "area").with("x", 100).with("y", 50).with("width", 320).with("height", 0);
        assert_eq!(crop_pad(&area).unwrap(), Reframe::Area { x: 100, y: 50, width: 320, height: 0 });
        assert_eq!(crop_pad(&area).unwrap().size((640, 360)), (320, 310));
        // The range ends are accepted.
        let side = i64::from(MAX_EFFECT_SIDE);
        for (key, v) in [("left", -side), ("bottom", side), ("x", side), ("height", side), ("width", 0)] {
            assert!(crop_pad(&Settings::new().with(key, v)).is_ok(), "{key} {v}");
        }
    }

    #[test]
    fn crop_and_pad_rejects_bad_settings_and_keeps_the_last_good_ones() {
        let side = i64::from(MAX_EFFECT_SIDE);
        let bad = [
            ("left", serde_json::json!(side + 1)),
            ("top", serde_json::json!(-side - 1)),
            ("right", serde_json::json!(1.5)),
            ("bottom", serde_json::json!("ten")),
            ("x", serde_json::json!(-1)),
            ("width", serde_json::Value::from(f64::NAN)),
            ("height", serde_json::json!(1e300)),
            ("mode", serde_json::json!("middle")),
            ("mode", serde_json::json!(1)),
        ];
        for (key, value) in bad {
            let s = Settings::new().with(key, value.clone());
            assert!(CropPad.create(&s).is_err(), "{key} {value}");
            let mut f = CropPad.create(&Settings::new().with("left", 4)).unwrap();
            assert!(f.update(&s).is_err(), "{key} {value}");
            let kept = f.video_effect().map(|e| e.reframe);
            assert_eq!(kept, Some(Reframe::Edges { left: 4, top: 0, right: 0, bottom: 0 }), "{key} {value}");
        }
        // A bad value is an error in either mode.
        assert!(crop_pad(&Settings::new().with("mode", "area").with("left", side + 1)).is_err());
        assert!(crop_pad(&Settings::new().with("x", -5)).is_err());
    }

    /// The schema's ranges are the ones the filter accepts.
    #[test]
    fn crop_and_pad_accepts_what_its_schema_offers() {
        let schema = CropPad.schema();
        for field in &schema.fields {
            match &field.kind {
                FieldKind::Int { min, max } => {
                    for v in [*min, *max] {
                        assert!(crop_pad(&Settings::new().with(&field.key, v)).is_ok(), "{} {v}", field.key);
                    }
                    for v in [min - 1, max + 1] {
                        assert!(crop_pad(&Settings::new().with(&field.key, v)).is_err(), "{} {v}", field.key);
                    }
                }
                FieldKind::Choice { options } => {
                    let values: Vec<&str> = options.iter().filter_map(|o| o.value.as_str()).collect();
                    assert_eq!(values, CROP_PAD_MODES);
                }
                _ => panic!("{} has an unexpected kind", field.key),
            }
        }
    }

    /// Crop and pad on the GPU: a 4 x 1 picture of red, green, blue and white with a column off
    /// the left and one transparent column added on the right draws green, blue, white and the
    /// item under it.
    #[test]
    fn crop_and_pad_draws_the_reframed_picture() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicU64;

        use studiocraft_media::Tick;
        use studiocraft_media::video::PixelFormat;
        use studiocraft_model::{SourceId, Transform, Vec2};
        use studiocraft_plugin::{VideoContent, VideoOut};
        use studiocraft_render::{Compositor, ItemDesc, Plan, SceneDesc};

        let Some(gpu) = gpu() else { return };
        let mut c = Compositor::new(gpu, 40, 16).unwrap();
        let video = Arc::new(VideoOut::default());
        let mut f = video.frame();
        assert!(f.reshape(4, 1, PixelFormat::Rgba8));
        f.data.copy_from_slice(&[255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255]);
        video.publish(VideoContent::Frame(f));
        let effect = CropPad.create(&Settings::new().with("left", 1).with("right", -1)).unwrap().video_effect().unwrap();
        let back = Arc::new(VideoOut::default());
        back.publish(VideoContent::Solid { rgba: [255, 0, 255, 255], width: 40, height: 16 });
        let under = ItemDesc { source: SourceId(2), video: back, transform: Transform::default(), effects: vec![] };
        let t = Transform { scale: Vec2::new(10.0, 10.0), ..Transform::default() };
        let item = ItemDesc { source: SourceId(1), video, transform: t, effects: vec![effect] };
        c.set_plan(Arc::new(Plan { program: SceneDesc { items: vec![under, item] }, ..Plan::default() }));
        c.frame(Tick(0), &AtomicU64::new(0));
        assert!(c.take_errors().is_empty());
        let px = c.read_program().unwrap();
        let at = |x: usize, y: usize| &px[(y * 40 + x) * 4..(y * 40 + x) * 4 + 4];
        // The 4 x 1 picture is drawn 10 times larger with linear filtering, so a pixel near a
        // column's middle takes up to a few percent of the next column.
        for (x, want) in [(4, [0, 255, 0, 255]), (14, [0, 0, 255, 255]), (24, [255, 255, 255, 255]), (35, [255, 0, 255, 255])] {
            assert!(at(x, 5).iter().zip(want).all(|(a, b)| a.abs_diff(b) <= 16), "x {x}: {:?}", at(x, 5));
        }
        assert!(at(5, 13).iter().zip([255, 0, 255, 255]).all(|(a, b)| a.abs_diff(b) <= 3), "the picture is 10 pixels tall");
    }

    /// A `w` x `h` RGBA frame drawn at the top-left of a 16 x 16 canvas through `effects`; the
    /// canvas as RGBA rows. `None` without a graphics adapter.
    fn draw_frame(pixels: &[[u8; 4]], w: u32, h: u32, effects: Vec<VideoEffect>) -> Option<Vec<u8>> {
        use std::sync::Arc;
        use std::sync::atomic::AtomicU64;

        use studiocraft_media::Tick;
        use studiocraft_media::video::PixelFormat;
        use studiocraft_model::{SourceId, Transform};
        use studiocraft_plugin::{VideoContent, VideoOut};
        use studiocraft_render::{Compositor, ItemDesc, Plan, SceneDesc};

        let mut c = Compositor::new(gpu()?, 16, 16).unwrap();
        let video = Arc::new(VideoOut::default());
        let mut f = video.frame();
        assert!(f.reshape(w, h, PixelFormat::Rgba8));
        for (dst, src) in f.data.as_chunks_mut::<4>().0.iter_mut().zip(pixels) {
            dst.copy_from_slice(src);
        }
        video.publish(VideoContent::Frame(f));
        let item = ItemDesc { source: SourceId(1), video, transform: Transform::default(), effects };
        c.set_plan(Arc::new(Plan { program: SceneDesc { items: vec![item] }, ..Plan::default() }));
        c.frame(Tick(0), &AtomicU64::new(0));
        assert!(c.take_errors().is_empty());
        Some(c.read_program().unwrap())
    }

    fn canvas_pixel(px: &[u8], x: usize, y: usize) -> [u8; 4] {
        let i = (y * 16 + x) * 4;
        [px[i], px[i + 1], px[i + 2], px[i + 3]]
    }

    /// Gray 100, 200, 100 in a row: the middle pixel's eight neighbors (the row above and below
    /// are the row itself at the edge) average 125, and its own value moves away from that.
    #[test]
    fn sharpen_moves_each_pixel_away_from_its_neighbors() {
        let gray = |v: u8| [v, v, v, 255];
        let row = [gray(100), gray(200), gray(100)];
        let sharpen = |s: f64| Sharpen.create(&Settings::new().with("sharpness", s)).unwrap().video_effect().unwrap();
        let Some(px) = draw_frame(&row, 3, 1, vec![sharpen(0.1)]) else { return };
        // Gain 0.4: 200 + 0.4 × (200 − 125) = 230, and 100 + 0.4 × (100 − 137.5) = 85.
        let near = |got: [u8; 4], want: u8| got[..3].iter().all(|c| c.abs_diff(want) <= 2) && got[3] == 255;
        assert!(near(canvas_pixel(&px, 1, 0), 230), "{:?}", canvas_pixel(&px, 1, 0));
        assert!(near(canvas_pixel(&px, 0, 0), 85), "{:?}", canvas_pixel(&px, 0, 0));
        assert!(near(canvas_pixel(&px, 2, 0), 85), "{:?}", canvas_pixel(&px, 2, 0));
        // At 0 the picture is unchanged, and a strong setting clips at white and black.
        let Some(px) = draw_frame(&row, 3, 1, vec![sharpen(0.0)]) else { return };
        assert!(near(canvas_pixel(&px, 1, 0), 200) && near(canvas_pixel(&px, 0, 0), 100));
        let Some(px) = draw_frame(&row, 3, 1, vec![sharpen(1.0)]) else { return };
        assert!(near(canvas_pixel(&px, 1, 0), 255) && near(canvas_pixel(&px, 0, 0), 0), "{:?}", &px[..12]);
    }

    /// A flat picture stays flat, and transparency is kept.
    #[test]
    fn sharpen_leaves_flat_areas_and_alpha_alone() {
        let flat = [[40, 120, 200, 128]; 9];
        let effect = Sharpen.create(&Settings::new().with("sharpness", 1.0)).unwrap().video_effect().unwrap();
        let Some(px) = draw_frame(&flat, 3, 3, vec![effect]) else { return };
        // Drawn over black at half opacity.
        let want = [20, 60, 100, 255];
        for (x, y) in [(0, 0), (1, 1), (2, 2)] {
            let got = canvas_pixel(&px, x, y);
            assert!(got.iter().zip(want).all(|(a, b)| a.abs_diff(b) <= 2), "({x}, {y}): {got:?}");
        }
    }

    #[test]
    fn sharpen_rejects_settings_outside_its_range() {
        assert_eq!(sharpen_params(&Settings::new()).unwrap().0[0], (SHARPEN_DEFAULT * SHARPEN_GAIN) as f32);
        assert_eq!(sharpen_params(&Sharpen.schema().defaults()).unwrap(), sharpen_params(&Settings::new()).unwrap());
        for v in [0.0, 1.0] {
            assert!(Sharpen.create(&Settings::new().with("sharpness", v)).is_ok());
        }
        for bad in [serde_json::json!(1.01), serde_json::json!(-0.1), serde_json::json!("sharp"), serde_json::Value::from(f64::NAN)] {
            let s = Settings::new().with("sharpness", bad.clone());
            assert!(Sharpen.create(&s).is_err(), "{bad}");
            let mut f = Sharpen.create(&Settings::new().with("sharpness", 0.5)).unwrap();
            assert!(f.update(&s).is_err(), "{bad}");
            assert_eq!(f.video_effect().map(|e| e.params[0]), Some(2.0), "{bad}: the last good setting stays");
        }
    }

    fn color_key(s: Settings) -> VideoEffect {
        ColorKey.create(&s).unwrap().video_effect().unwrap()
    }

    /// Green, red and a green 0.439 from the key: within Similarity 400 the pixel is keyed out,
    /// in the next 80 it fades, and beyond that it stays.
    #[test]
    fn color_key_keys_out_the_colors_near_the_key() {
        let row = [[0, 255, 0, 255], [255, 0, 0, 255], [0, 143, 0, 255]];
        let near = |got: [u8; 4], want: [u8; 4]| got.iter().zip(want).all(|(a, b)| a.abs_diff(b) <= 3);
        let Some(px) = draw_frame(&row, 3, 1, vec![color_key(Settings::new())]) else { return };
        // Drawn over black: a keyed-out pixel shows black.
        assert!(near(canvas_pixel(&px, 0, 0), [0, 0, 0, 255]), "{:?}", canvas_pixel(&px, 0, 0));
        assert!(near(canvas_pixel(&px, 1, 0), [255, 0, 0, 255]), "{:?}", canvas_pixel(&px, 1, 0));
        // (0.4392 - 0.4) / 0.08 = 0.49 of 143.
        assert!(near(canvas_pixel(&px, 2, 0), [0, 70, 0, 255]), "{:?}", canvas_pixel(&px, 2, 0));
        // Keyed on red instead, green stays and red goes.
        let Some(px) = draw_frame(&row, 3, 1, vec![color_key(Settings::new().with("key_color", "#ff0000"))]) else { return };
        assert!(near(canvas_pixel(&px, 0, 0), [0, 255, 0, 255]) && near(canvas_pixel(&px, 1, 0), [0, 0, 0, 255]));
    }

    /// The pixels left are adjusted and drawn at Opacity.
    #[test]
    fn color_key_adjusts_the_pixels_it_keeps() {
        let near = |got: [u8; 4], want: [u8; 4]| got.iter().zip(want).all(|(a, b)| a.abs_diff(b) <= 3);
        let row = [[255, 0, 0, 255], [64, 64, 64, 255]];
        let s = Settings::new().with("opacity", 50.0).with("brightness", 0.2);
        let Some(px) = draw_frame(&row, 2, 1, vec![color_key(s)]) else { return };
        // Red with 0.2 added is (1, 0.2, 0.2), at half opacity over black.
        assert!(near(canvas_pixel(&px, 0, 0), [128, 26, 26, 255]), "{:?}", canvas_pixel(&px, 0, 0));
        // Gamma 1 takes the square root: 0.251 becomes 0.501.
        let Some(px) = draw_frame(&row, 2, 1, vec![color_key(Settings::new().with("gamma", 1.0))]) else { return };
        assert!(near(canvas_pixel(&px, 1, 0), [128, 128, 128, 255]), "{:?}", canvas_pixel(&px, 1, 0));
    }

    #[test]
    fn color_key_reads_its_settings() {
        let (p, reframe) = color_key_params(&Settings::new()).unwrap();
        assert_eq!(reframe, Reframe::None);
        assert_eq!(p[..9], [0.0, 1.0, 0.0, 0.0, 0.4, 0.08, 1.0, 1.0, 1.0]);
        assert_eq!(color_key_params(&ColorKey.schema().defaults()).unwrap(), (p, reframe));
        let s = Settings::new()
            .with("key_color", "#3366ff80")
            .with("similarity", 1000)
            .with("smoothness", 1)
            .with("opacity", 25.0)
            .with("contrast", 1.0)
            .with("brightness", -1.0)
            .with("gamma", -1.0);
        let (p, _) = color_key_params(&s).unwrap();
        assert_eq!(p[..9], [0.2, 0.4, 1.0, -1.0, 1.0, 0.001, 4.0, 2.0, 0.25]);
        let (p, _) = color_key_params(&Settings::new().with("contrast", -0.5)).unwrap();
        assert_eq!(p[6], 0.5);
    }

    #[test]
    fn color_key_rejects_bad_settings_and_keeps_the_last_good_ones() {
        use serde_json::{Value, json};
        let bad: [(&str, Value); 13] = [
            ("key_color", json!("green")),
            ("key_color", json!("#00ff0")),
            ("key_color", json!(65280)),
            ("similarity", json!(0)),
            ("similarity", json!(1001)),
            ("similarity", json!(400.5)),
            ("smoothness", json!("80")),
            ("opacity", json!(100.5)),
            ("opacity", json!(-1)),
            ("contrast", json!(1.1)),
            ("brightness", json!(-1.5)),
            ("gamma", json!(2)),
            ("gamma", Value::from(f64::NAN)),
        ];
        for (key, v) in bad {
            let s = Settings::new().with(key, v.clone());
            assert!(ColorKey.create(&s).is_err(), "{key} = {v}");
            let mut f = ColorKey.create(&Settings::new().with("similarity", 500)).unwrap();
            assert!(f.update(&s).is_err(), "{key} = {v}");
            assert_eq!(f.video_effect().map(|e| e.params[4]), Some(0.5), "{key} = {v}: the last good settings stay");
        }
    }

    #[test]
    fn video_filters_have_effects_and_audio_filters_do_not() {
        assert_eq!(builtin_video_effects().len(), 5);
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
