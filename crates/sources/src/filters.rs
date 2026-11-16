//! Built-in filters.

use studiocraft_media::audio::{BLOCK_FRAMES, CHANNELS, SAMPLE_RATE};
use studiocraft_media::mix::{from_db, to_db};
use studiocraft_model::{Field, FieldKind, Settings, SettingsSchema};
use studiocraft_plugin::{Filter, FilterInstance, FilterTarget, Plugin, PluginError, VideoEffect};

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
fn effect(color: vec4<f32>, uv: vec2<f32>, size: vec2<f32>, p0: vec4<f32>, p1: vec4<f32>) -> vec4<f32> {
    // p0: brightness, contrast factor, saturation factor, gamma exponent. p1.x: opacity.
    let a = color.a;
    if a <= 0.0 { return vec4<f32>(0.0); }
    var c = pow(max(color.rgb / a, vec3<f32>(0.0)), vec3<f32>(p0.w));
    c = (c - 0.5) * p0.y + 0.5 + p0.x;
    let luma = dot(c, vec3<f32>(0.2126, 0.7152, 0.0722));
    c = clamp(mix(vec3<f32>(luma), c, p0.z), vec3<f32>(0.0), vec3<f32>(1.0));
    let na = a * p1.x;
    return vec4<f32>(c * na, na);
}
";

/// Brightness, contrast, saturation, gamma and opacity.
pub struct ColorCorrection;

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
            float_field("opacity", "Opacity (%)", 0.0, 100.0, 1.0, 100.0),
        ])
    }
}

impl Filter for ColorCorrection {
    fn target(&self) -> FilterTarget {
        FilterTarget::Video
    }
    fn create(&self, settings: &Settings) -> Result<Box<dyn FilterInstance>, PluginError> {
        let mut f = Effect { key: "color_correction", wgsl: COLOR_CORRECTION, params: [0.0; 8], read: color_correction_params };
        f.update(settings)?;
        Ok(Box::new(f))
    }
}

fn color_correction_params(s: &Settings) -> [f32; 8] {
    let gamma = float(s, "gamma", 0.0).clamp(-3.0, 3.0);
    let exponent = if gamma >= 0.0 { 1.0 / (1.0 + gamma) } else { 1.0 - gamma };
    let contrast = float(s, "contrast", 0.0).clamp(-1.0, 1.0);
    let contrast = if contrast < 0.0 { 1.0 + contrast } else { 1.0 + 3.0 * contrast };
    let saturation = 1.0 + float(s, "saturation", 0.0).clamp(-1.0, 5.0);
    let opacity = float(s, "opacity", 100.0).clamp(0.0, 100.0) / 100.0;
    [float(s, "brightness", 0.0).clamp(-1.0, 1.0), contrast, saturation, exponent, opacity, 0.0, 0.0, 0.0]
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
        let mut f = Effect { key: "chroma_key", wgsl: CHROMA_KEY, params: [0.0; 8], read: chroma_key_params };
        f.update(settings)?;
        Ok(Box::new(f))
    }
}

fn chroma_key_params(s: &Settings) -> [f32; 8] {
    let [r, g, b, _] = s.color("key_color").unwrap_or([0, 255, 0, 255]);
    let unit = |key: &str, default: i64| s.i64(key).unwrap_or(default).clamp(0, 1000) as f32 / 1000.0;
    [
        f32::from(r) / 255.0,
        f32::from(g) / 255.0,
        f32::from(b) / 255.0,
        0.0,
        unit("similarity", 400) * 0.5,
        unit("smoothness", 80) * 0.5,
        unit("spill", 100),
        0.0,
    ]
}

/// A video filter that is one GPU pass with parameters read from its settings.
struct Effect {
    key: &'static str,
    wgsl: &'static str,
    params: [f32; 8],
    read: fn(&Settings) -> [f32; 8],
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

    #[test]
    fn video_filter_shaders_compile() {
        use std::sync::Arc;

        use studiocraft_model::SourceId;
        use studiocraft_plugin::{VideoContent, VideoOut};
        use studiocraft_render::{Compositor, Gpu, ItemDesc, Plan, SceneDesc};

        // Machines without a graphics adapter pass without checking.
        let Ok(gpu) = Gpu::headless() else { return };
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
        assert_eq!(neutral, [0.0, 1.0, 1.0, 1.0, 1.0, 0.0, 0.0, 0.0]);
    }
}
