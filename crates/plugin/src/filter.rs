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
/// the source's color at `uv` (premultiplied alpha), the source size in pixels, and the
/// filter's parameters, returning the new premultiplied color. `p0` and `p1` hold
/// `params[0..8]`; the shader reads `params[8..24]` with `extra_params()`, a `mat4x4<f32>` whose
/// columns are `params[8..12]` to `params[20..24]`. The compositor compiles each distinct `key`
/// once.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VideoEffect {
    pub key: &'static str,
    pub wgsl: &'static str,
    pub params: [f32; VIDEO_EFFECT_PARAMS],
}

/// The number of parameters a [`VideoEffect`] carries.
pub const VIDEO_EFFECT_PARAMS: usize = 24;

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
