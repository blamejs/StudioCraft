//! The GPU compositor.
//!
//! Each frame the compositor uploads the newest frame of every source on screen, runs each
//! source's video filters as GPU passes, draws the items of the program scene (and, in studio
//! mode, the preview scene) bottom to top at the canvas size, blends two scenes during a
//! transition, and, while a recording or stream runs, converts the program picture to NV12 at the
//! output size and reads it back for the encoders.
//!
//! Frames stay on the GPU from upload to the canvas. The interface shows the canvas textures
//! directly; only encoders read pixels back, through a ring of buffers that never makes the
//! compositor wait for the GPU.

mod compositor;
mod nv12;
mod plan;
mod shaders;
pub mod transitions;

use std::sync::Arc;

pub use compositor::{CAPTURE_SLOTS, CanvasViews, Capture, CaptureRequest, Compositor, RenderShared, Ring, View, unpremultiply};
pub use nv12::EncodeTarget;
pub use plan::{ItemDesc, Plan, SceneDesc, TransitionDesc};

/// Format of the canvas textures. Pixels are sRGB-encoded and composited as stored; the
/// interface samples them through an sRGB view (see [`CanvasViews`]).
pub const CANVAS_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
/// The view format the interface samples the canvas through.
pub const CANVAS_VIEW_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

/// Errors from the GPU.
#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    #[error("no graphics adapter is available: {0}")]
    NoAdapter(String),
    #[error("the graphics device could not be opened: {0}")]
    NoDevice(String),
    #[error("a shader failed to compile ({key}): {message}")]
    Shader { key: String, message: String },
    #[error("the graphics device reported an error: {0}")]
    Device(String),
}

/// A GPU device and its queue. Cloning shares them.
#[derive(Clone, Debug)]
pub struct Gpu {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
}

impl Gpu {
    /// A device without a window (tests, benchmarks, the command-line tool).
    pub fn headless() -> Result<Gpu, RenderError> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let options = wgpu::RequestAdapterOptions { power_preference: wgpu::PowerPreference::HighPerformance, ..Default::default() };
        let adapter = pollster::block_on(instance.request_adapter(&options)).map_err(|e| RenderError::NoAdapter(e.to_string()))?;
        let desc = wgpu::DeviceDescriptor { label: Some("studiocraft"), ..Default::default() };
        let (device, queue) = pollster::block_on(adapter.request_device(&desc)).map_err(|e| RenderError::NoDevice(e.to_string()))?;
        Ok(Gpu::new(device, queue))
    }

    /// Wrap a device another part of the app opened (the window's).
    pub fn new(device: wgpu::Device, queue: wgpu::Queue) -> Gpu {
        // Errors are caught by scopes around the calls that can fail; wgpu's default handler
        // would end the process.
        device.on_uncaptured_error(Arc::new(|e| log::error!("graphics device: {e}")));
        Gpu { device, queue }
    }

    /// Run `f`, returning the first validation or memory error it caused.
    pub fn scoped<T>(&self, key: &str, f: impl FnOnce() -> T) -> Result<T, RenderError> {
        let memory = self.device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
        let validation = self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let out = f();
        let v = pollster::block_on(validation.pop());
        let m = pollster::block_on(memory.pop());
        match v.or(m) {
            None => Ok(out),
            Some(e) => Err(RenderError::Shader { key: key.to_string(), message: e.to_string() }),
        }
    }
}
