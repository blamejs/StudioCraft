//! Filters on a source's video or audio.

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
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VideoEffect {
    pub key: &'static str,
    pub wgsl: &'static str,
    pub params: [f32; VIDEO_EFFECT_PARAMS],
    /// The part of the input the pass draws, which sets the size of its output.
    pub reframe: Reframe,
}

impl VideoEffect {
    /// A pass whose output has its input's size.
    pub fn new(key: &'static str, wgsl: &'static str, params: [f32; VIDEO_EFFECT_PARAMS]) -> Self {
        Self { key, wgsl, params, reframe: Reframe::None }
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
