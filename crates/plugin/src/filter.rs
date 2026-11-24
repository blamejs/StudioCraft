//! Filters on a source's video or audio.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use studiocraft_model::Settings;

use crate::{Plugin, PluginError};

/// What a filter works on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterTarget {
    Video,
    Audio,
}

/// A filter type.
pub trait Filter: Plugin {
    fn target(&self) -> FilterTarget;
    fn create(&self, settings: &Settings) -> Result<Box<dyn FilterInstance>, PluginError>;
}

/// One GPU pass a video filter adds after its source is drawn.
///
/// `wgsl` defines
/// `fn effect(color: vec4<f32>, uv: vec2<f32>, size: vec2<f32>, p0: vec4<f32>, p1: vec4<f32>) -> vec4<f32>`:
/// the input's color under the output pixel at `uv` (premultiplied alpha, transparent where
/// [`VideoEffect::reframe`] adds padding), the output size in pixels, and the filter's
/// parameters, returning the new premultiplied color. `p0` and `p1` hold `params[0..8]`; the
/// shader reads `params[8..24]` with `extra_params()`, a `mat4x4<f32>` whose columns are
/// `params[8..12]` to `params[20..24]`. `input_size()` is the input's size in pixels, and
/// `input_pixel(px)` reads the input at a pixel position (pixel centers at .5), clamped to the
/// input's edges. The compositor compiles each distinct `key` once.
///
/// A pass with an [`image`](VideoEffect::image) can also call `image_at(st)`: the picture's
/// premultiplied color at `st` (0 to 1 across the picture, top-left at 0), transparent outside
/// it. Every pass with the same `key` has an image, or none does; a pass that differs from the
/// first one compiled under its key is skipped.
#[derive(Clone, Debug, PartialEq)]
pub struct VideoEffect {
    pub key: &'static str,
    pub wgsl: &'static str,
    pub params: [f32; VIDEO_EFFECT_PARAMS],
    /// The part of the input the pass draws, which sets the size of its output.
    pub reframe: Reframe,
    /// A picture the pass reads beside its input.
    pub image: Option<EffectImage>,
    /// Changes the parameters with time, before each frame's pass.
    pub animation: Option<Animation>,
}

impl VideoEffect {
    /// A pass whose output has its input's size.
    pub fn new(key: &'static str, wgsl: &'static str, params: [f32; VIDEO_EFFECT_PARAMS]) -> Self {
        Self { key, wgsl, params, reframe: Reframe::None, image: None, animation: None }
    }
}

/// How a pass's parameters change with time. Before each frame's pass the compositor calls
/// `step` with a copy of [`VideoEffect::params`], the seconds since the first frame it drew a
/// pass with this animation, and the size of the pass's input in pixels. Two animations are
/// equal when they come from the same [`Animation::new`] call; a new one starts at 0 seconds.
#[derive(Clone, Copy)]
pub struct Animation {
    id: u64,
    step: AnimationStep,
}

/// The function an [`Animation`] calls.
pub type AnimationStep = fn(&mut [f32; VIDEO_EFFECT_PARAMS], f64, (u32, u32));

impl Animation {
    pub fn new(step: AnimationStep) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Self { id: NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed), step }
    }
    /// Different for every [`Animation::new`] call.
    pub fn id(&self) -> u64 {
        self.id
    }
    /// The parameters `params` take `seconds` after the animation started, for an `input` of
    /// that size.
    pub fn step(&self, params: &mut [f32; VIDEO_EFFECT_PARAMS], seconds: f64, input: (u32, u32)) {
        (self.step)(params, seconds, input);
    }
}

impl PartialEq for Animation {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl std::fmt::Debug for Animation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Animation({})", self.id)
    }
}

/// A picture for a video filter pass: tightly packed RGBA rows with straight alpha, shared
/// between the filter and the compositor. Two images are equal when they are the same picture,
/// not when their pixels match; the compositor uploads each one once.
#[derive(Clone)]
pub struct EffectImage(Arc<ImagePixels>);

struct ImagePixels {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

impl EffectImage {
    /// A `width` x `height` picture, or `None` when a side is 0 or larger than
    /// [`MAX_EFFECT_SIDE`], or `rgba` does not hold exactly `width * height * 4` bytes.
    pub fn new(width: u32, height: u32, rgba: Vec<u8>) -> Option<Self> {
        let sides = (1..=MAX_EFFECT_SIDE).contains(&width) && (1..=MAX_EFFECT_SIDE).contains(&height);
        (sides && rgba.len() as u64 == u64::from(width) * u64::from(height) * 4).then(|| Self(Arc::new(ImagePixels { width, height, rgba })))
    }
    pub fn width(&self) -> u32 {
        self.0.width
    }
    pub fn height(&self) -> u32 {
        self.0.height
    }
    pub fn rgba(&self) -> &[u8] {
        &self.0.rgba
    }
    /// The same for every copy of this picture and different from every other picture alive.
    pub fn id(&self) -> usize {
        Arc::as_ptr(&self.0) as usize
    }
}

impl PartialEq for EffectImage {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl std::fmt::Debug for EffectImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "EffectImage({} x {})", self.0.width, self.0.height)
    }
}

/// The number of parameters a [`VideoEffect`] carries.
pub const VIDEO_EFFECT_PARAMS: usize = 24;

/// The largest width or height a video filter's output can have, in pixels.
pub const MAX_EFFECT_SIDE: u32 = 8192;

/// Which part of its input a video filter pass draws. Parts outside the input are transparent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Reframe {
    /// The whole input, at its size.
    #[default]
    None,
    /// The input with pixels taken off each side (positive values) or transparent pixels added
    /// (negative values).
    Edges { left: i32, top: i32, right: i32, bottom: i32 },
    /// The area of the input whose top-left corner is at `x`, `y`. A width or height of 0
    /// reaches to the input's right or bottom edge.
    Area { x: u32, y: u32, width: u32, height: u32 },
}

impl Reframe {
    /// Where the output of a `width` x `height` input starts in the input, and its size: output
    /// pixel (`ox`, `oy`) shows input pixel (`ox + left`, `oy + top`). A side that would be
    /// shorter than 1 pixel is 0, and a side longer than [`MAX_EFFECT_SIDE`] is cut to it at the
    /// right or bottom.
    pub fn apply(self, (width, height): (u32, u32)) -> (i64, i64, u32, u32) {
        let side = |v: i64| v.clamp(0, i64::from(MAX_EFFECT_SIDE)) as u32;
        let (w, h) = (i64::from(width), i64::from(height));
        match self {
            Reframe::None => (0, 0, side(w), side(h)),
            Reframe::Edges { left, top, right, bottom } => {
                let (l, t) = (i64::from(left), i64::from(top));
                (l, t, side(w - l - i64::from(right)), side(h - t - i64::from(bottom)))
            }
            Reframe::Area { x, y, width: aw, height: ah } => {
                let (x, y) = (i64::from(x), i64::from(y));
                let ow = if aw == 0 { w - x } else { i64::from(aw) };
                let oh = if ah == 0 { h - y } else { i64::from(ah) };
                (x, y, side(ow), side(oh))
            }
        }
    }

    /// The output size for a `width` x `height` input.
    pub fn size(self, input: (u32, u32)) -> (u32, u32) {
        let (_, _, w, h) = self.apply(input);
        (w, h)
    }

    /// The size an `input` has after each pass in turn: 0 x 0 once a pass leaves no pixels.
    pub fn chain(reframes: impl IntoIterator<Item = Reframe>, input: (u32, u32)) -> (u32, u32) {
        let mut size = input;
        for r in reframes {
            if size.0 == 0 || size.1 == 0 {
                break;
            }
            size = r.size(size);
        }
        if size.0 == 0 || size.1 == 0 { (0, 0) } else { size }
    }
}

/// A running filter. Video filters live on the compositor thread and audio filters on the mixer
/// thread, so instances are `Send`.
pub trait FilterInstance: Send {
    fn update(&mut self, settings: &Settings) -> Result<(), PluginError>;
    /// The pass a video filter draws.
    fn video_effect(&self) -> Option<VideoEffect> {
        None
    }
    /// Process one interleaved stereo block in place (audio filters).
    fn process_audio(&mut self, block: &mut [f32]) {
        let _ = block;
    }
    /// The handle to this filter's gain-reduction reading, for an audio filter that reduces
    /// gain. The filter writes the reading after each [`process_audio`](Self::process_audio)
    /// call; every call to `meter` returns a handle to the same reading.
    fn meter(&self) -> Option<FilterMeter> {
        None
    }
    /// How many sample frames later an audio filter's output carries its input, at the mixer's
    /// sample rate: 0 for a filter that does not delay its audio. Read after the filter is
    /// created or updated; it does not change while the filter processes audio.
    fn latency_frames(&self) -> usize {
        0
    }
}

/// A filter's gain reduction during its last processed block, in dB (0 or more), shared between
/// the mixer thread that writes it and any thread that reads it. Writing and reading take no
/// lock and allocate nothing; cloning shares the same reading.
#[derive(Clone, Debug, Default)]
pub struct FilterMeter(Arc<AtomicU32>);

impl FilterMeter {
    /// A reading of 0 dB.
    pub fn new() -> Self {
        Self::default()
    }
    /// Stores `db`. A value that is not a finite number of 0 or more is stored as 0.
    pub fn set(&self, db: f32) {
        let db = if db.is_finite() && db > 0.0 { db } else { 0.0 };
        self.0.store(db.to_bits(), Ordering::Relaxed);
    }
    /// The last stored reading, in dB.
    pub fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }
    /// True when both handles share one reading.
    pub fn same(&self, other: &FilterMeter) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_effect_image_holds_exactly_its_pixels_and_is_compared_by_identity() {
        let a = EffectImage::new(2, 1, vec![0; 8]).unwrap();
        assert_eq!((a.width(), a.height(), a.rgba().len()), (2, 1, 8));
        assert_eq!(a.clone(), a);
        assert_eq!(a.clone().id(), a.id());
        let b = EffectImage::new(2, 1, vec![0; 8]).unwrap();
        assert_ne!(a, b, "the same pixels in another picture");
        assert_ne!(a.id(), b.id());
        for (w, h, len) in [(0, 1, 0), (1, 0, 0), (2, 1, 7), (2, 1, 9), (MAX_EFFECT_SIDE + 1, 1, (MAX_EFFECT_SIDE as usize + 1) * 4)] {
            assert!(EffectImage::new(w, h, vec![0; len]).is_none(), "{w} x {h}, {len} bytes");
        }
        assert!(EffectImage::new(MAX_EFFECT_SIDE, 1, vec![0; MAX_EFFECT_SIDE as usize * 4]).is_some());
    }

    #[test]
    fn a_filter_meter_shares_one_reading_and_stores_only_non_negative_numbers() {
        let m = FilterMeter::new();
        assert_eq!(m.get(), 0.0);
        let reader = m.clone();
        assert!(reader.same(&m));
        assert!(!FilterMeter::new().same(&m));
        m.set(12.5);
        assert_eq!(reader.get(), 12.5);
        for bad in [-3.0, -0.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            m.set(7.0);
            m.set(bad);
            assert_eq!(reader.get().to_bits(), 0.0f32.to_bits(), "{bad}");
        }
        m.set(f32::MAX);
        assert_eq!(reader.get(), f32::MAX);
    }

    #[test]
    fn a_filter_instance_has_no_meter_and_no_latency_by_default() {
        struct Plain;
        impl FilterInstance for Plain {
            fn update(&mut self, _: &Settings) -> Result<(), PluginError> {
                Ok(())
            }
        }
        assert!(Plain.meter().is_none());
        assert_eq!(Plain.latency_frames(), 0);
    }

    #[test]
    fn an_animation_steps_parameters_and_is_compared_by_its_creation() {
        fn step(p: &mut [f32; VIDEO_EFFECT_PARAMS], seconds: f64, (w, h): (u32, u32)) {
            p[0] = seconds as f32;
            p[1] = (w + h) as f32;
        }
        let a = Animation::new(step);
        let mut p = [0.0; VIDEO_EFFECT_PARAMS];
        a.step(&mut p, 2.5, (3, 4));
        assert_eq!(p[..3], [2.5, 7.0, 0.0]);
        let copy = a;
        assert_eq!(copy, a);
        let b = Animation::new(step);
        assert_ne!(a, b, "the same step from another call");
        assert_ne!(a.id(), b.id());
    }

    #[test]
    fn reframing_crops_pads_and_picks_areas() {
        assert_eq!(Reframe::None.apply((640, 360)), (0, 0, 640, 360));
        let crop = Reframe::Edges { left: 10, top: 20, right: 30, bottom: 40 };
        assert_eq!(crop.apply((640, 360)), (10, 20, 600, 300));
        let pad = Reframe::Edges { left: -10, top: -5, right: -10, bottom: 0 };
        assert_eq!(pad.apply((100, 50)), (-10, -5, 120, 55));
        // Cropping more than the input leaves nothing.
        assert_eq!(Reframe::Edges { left: 60, top: 0, right: 60, bottom: 0 }.size((100, 50)), (0, 50));
        assert_eq!(Reframe::Edges { left: i32::MAX, top: i32::MIN, right: i32::MAX, bottom: i32::MIN }.size((100, 50)), (0, MAX_EFFECT_SIDE));
        // Padding past the largest side is cut at the right.
        assert_eq!(Reframe::Edges { left: -8000, top: 0, right: -8000, bottom: 0 }.apply((100, 50)), (-8000, 0, MAX_EFFECT_SIDE, 50));
        // An area of 0 reaches the input's edge; one past the input keeps its size.
        assert_eq!(Reframe::Area { x: 40, y: 10, width: 0, height: 0 }.apply((100, 50)), (40, 10, 60, 40));
        assert_eq!(Reframe::Area { x: 90, y: 0, width: 50, height: 20 }.apply((100, 50)), (90, 0, 50, 20));
        assert_eq!(Reframe::Area { x: 200, y: 0, width: 0, height: 0 }.size((100, 50)), (0, 50));
        // An empty input stays empty.
        assert_eq!(Reframe::None.size((0, 0)), (0, 0));
    }

    #[test]
    fn a_chain_of_passes_stays_empty_once_a_pass_leaves_nothing() {
        let crop = Reframe::Edges { left: 10, top: 0, right: 10, bottom: 0 };
        let pad = Reframe::Edges { left: -5, top: -5, right: -5, bottom: -5 };
        assert_eq!(Reframe::chain([crop, pad], (100, 50)), (90, 60));
        assert_eq!(Reframe::chain([], (100, 50)), (100, 50));
        let all = Reframe::Edges { left: 100, top: 0, right: 0, bottom: 0 };
        assert_eq!(Reframe::chain([all, pad], (100, 50)), (0, 0));
        assert_eq!(Reframe::chain([pad], (0, 50)), (0, 0));
    }
}
