//! The GPU compositor.
//!
//! Each frame the compositor uploads the newest frame of every source on screen, runs each
//! source's video filters as GPU passes, draws the items of the program scene (and, in studio
//! mode, the preview scene) bottom to top at the canvas size, blends two scenes during a
//! transition, and, while a recording or stream runs, converts the program picture to NV12 at the
//! output size and reads it back for the encoders. On request it also draws small pictures of
//! other scenes, for scene lists, at a capped rate (see [`ThumbnailRequest`]).
//!
//! Frames stay on the GPU from upload to the canvas. The interface shows the canvas textures
//! directly. The encoders and the scopes read pixels back through a ring of buffers that never
//! makes the compositor wait for the GPU. A still capture is read back into a buffer of its own and
//! answered on a later frame, so it does not make the compositor wait either. A hardware encoder on
//! the compositor's adapter can take the program frames on the GPU instead
//! ([`RenderShared::encode_gpu`], [`GpuFrame`]); when that cannot run,
//! [`Compositor::export_fallback`] gives the reason and the read-back carries them.
//!
//! [`Gpu::open`] chooses the adapter with `craft-gpu-device` (re-exported as [`device`]): the
//! user's adapter preference, fixes for drivers that crash and a software fallback. Every device
//! is watched for a loss; after one, [`Gpu::is_lost`] is true, the compositor stops drawing with
//! it, and a device this crate opened can be opened again with [`Gpu::reopen`].

mod compositor;
mod nv12;
mod plan;
mod shaders;
pub mod transitions;

pub use craft_gpu_device as device;
use craft_gpu_device::{Candidate, DeviceHealth, Fault};
#[cfg(not(target_arch = "wasm32"))]
use craft_gpu_device::{Notice, OpenError, Preference, Request};

pub use compositor::{CAPTURE_SLOTS, CanvasViews, Capture, CaptureRequest, Compositor, RenderShared, Ring, ThumbnailViews, View, unpremultiply};
pub use nv12::{EncodeTarget, GpuFrame, GpuSink};
pub use plan::{
    ItemDesc, MAX_THUMBNAIL_FPS, MAX_THUMBNAIL_SIDE, MAX_THUMBNAILS, MIN_THUMBNAIL_SIDE, Plan, SceneDesc, ThumbnailRequest, TransitionDesc,
};

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

/// What [`Gpu::reopen`] opens another device with.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Debug)]
struct Reopen {
    instance: wgpu::Instance,
    /// The preference the device was opened for.
    preference: Preference,
}

/// A GPU device and its queue. Cloning shares them.
#[derive(Clone, Debug)]
pub struct Gpu {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    health: DeviceHealth,
    /// The adapter the device was opened on, when it is known.
    adapter: Option<Candidate>,
    /// Set for a device this crate opened; the window's device belongs to the window.
    #[cfg(not(target_arch = "wasm32"))]
    reopen: Option<Reopen>,
}

/// The device studiocraft opens: its own label, and the adapter's own limits, so an adapter with
/// lower limits than wgpu's defaults (an older or software one) still opens. A Vulkan device on
/// Windows also opens shared textures when the driver can, and one on Linux DMA-BUF buffers, for a
/// display capture that stays on the GPU.
#[cfg(not(target_arch = "wasm32"))]
const REQUEST: Request<'static> = Request {
    label: Some("studiocraft"),
    required_features: wgpu::Features::empty(),
    optional_features: wgpu::Features::VULKAN_EXTERNAL_MEMORY_WIN32.union(wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF),
    limits: None,
};

impl Gpu {
    /// A device without a window (tests, benchmarks, the command-line tool), on the automatic
    /// adapter choice.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn headless() -> Result<Gpu, RenderError> {
        Gpu::open(&Preference::Auto).map(|(gpu, _)| gpu)
    }

    /// A device without a window on the adapter `preference` asks for, with what the user should
    /// be told about the choice (a missing adapter, a fallback to software). An adapter whose
    /// device fails to open is skipped for the next one.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn open(preference: &Preference) -> Result<(Gpu, Vec<Notice>), RenderError> {
        Gpu::open_on(craft_gpu_device::native_instance(), preference)
    }

    /// [`Gpu::open`] on the adapters of `instance`.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn open_on(instance: wgpu::Instance, preference: &Preference) -> Result<(Gpu, Vec<Notice>), RenderError> {
        let opened = craft_gpu_device::open(&instance, None, preference, &REQUEST).map_err(|e| match e {
            OpenError::NoAdapter => RenderError::NoAdapter(e.to_string()),
            _ => RenderError::NoDevice(e.to_string()),
        })?;
        craft_gpu_interop::publish_import_caps(&opened.device);
        let gpu = Gpu {
            device: opened.device,
            queue: opened.queue,
            health: opened.health,
            adapter: Some(opened.candidate),
            reopen: Some(Reopen { instance, preference: preference.clone() }),
        };
        Ok((gpu, opened.notices))
    }

    /// Wrap a device another part of the app opened (the window's). Its losses and uncaptured
    /// errors are recorded instead of ending the process.
    pub fn new(device: wgpu::Device, queue: wgpu::Queue) -> Gpu {
        let health = DeviceHealth::watch(&device);
        craft_gpu_interop::publish_import_caps(&device);
        Gpu {
            device,
            queue,
            health,
            adapter: None,
            #[cfg(not(target_arch = "wasm32"))]
            reopen: None,
        }
    }

    /// The same device, with the adapter it was opened on.
    pub fn with_adapter(mut self, adapter: Candidate) -> Gpu {
        self.adapter = Some(adapter);
        self
    }

    /// The adapter the device was opened on, when it is known.
    pub fn adapter(&self) -> Option<&Candidate> {
        self.adapter.as_ref()
    }

    /// The device's health: losses and uncaptured errors.
    pub fn health(&self) -> &DeviceHealth {
        &self.health
    }

    /// Why the device was lost, once it was, or reported an uncaptured error (out of memory,
    /// validation, internal), after which its output cannot be trusted. Either way it is treated
    /// as lost: nothing drawn with it reaches the screen or an encoder.
    pub fn lost(&self) -> Option<String> {
        self.health.fault().map(|f| f.detail().to_string())
    }

    /// Whether the device was lost.
    pub fn is_lost(&self) -> bool {
        self.lost().is_some()
    }

    /// Whether [`Gpu::reopen`] can open another device: true for a device this crate opened,
    /// false for the window's.
    pub fn can_reopen(&self) -> bool {
        #[cfg(not(target_arch = "wasm32"))]
        return self.reopen.is_some();
        #[cfg(target_arch = "wasm32")]
        false
    }

    /// The preference a device this crate opened was opened for; `None` for the window's.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn preference(&self) -> Option<&Preference> {
        self.reopen.as_ref().map(|r| &r.preference)
    }

    /// Open another device on the instance this one was opened on, for `preference`.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn reopen(&self, preference: &Preference) -> Result<(Gpu, Vec<Notice>), RenderError> {
        let Some(r) = &self.reopen else {
            return Err(RenderError::NoDevice("the window's graphics device can only be opened again by the window".into()));
        };
        Gpu::open_on(r.instance.clone(), preference)
    }

    /// Hand finished GPU work to its callbacks without waiting. Notices a loss; does nothing on
    /// a lost device.
    pub fn poll(&self) {
        if !self.is_lost() {
            self.health.probe(&self.device);
        }
    }

    /// Wait until the GPU finished all submitted work. A lost device is an error, and wgpu's
    /// panic when it polls one is caught and recorded as the loss.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn wait(&self) -> Result<(), RenderError> {
        if let Some(why) = self.lost() {
            return Err(RenderError::Device(format!("the graphics device was lost: {why}")));
        }
        let poll = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.device.poll(wgpu::PollType::wait_indefinitely())));
        match poll {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(RenderError::Device(e.to_string())),
            Err(_) => {
                self.health.mark(Fault::Lost("the device stopped responding".into()));
                Err(RenderError::Device("the graphics device stopped responding".into()))
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// An instance whose only adapter is wgpu's noop one, which opens devices without a GPU.
    fn noop() -> wgpu::Instance {
        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
        desc.backends = wgpu::Backends::NOOP;
        desc.backend_options.noop = wgpu::NoopBackendOptions { enable: true, ..Default::default() };
        wgpu::Instance::new(desc)
    }

    #[test]
    fn an_opened_device_knows_its_adapter_and_preference() {
        let (gpu, notices) = Gpu::open_on(noop(), &Preference::Auto).unwrap();
        // The noop adapter is a software one, which Auto did not ask for.
        assert_eq!(notices, vec![Notice::SoftwareOnly]);
        assert!(gpu.adapter().is_some_and(Candidate::is_software));
        assert_eq!(gpu.preference(), Some(&Preference::Auto));
        assert!(gpu.can_reopen());
        assert!(!gpu.is_lost());
        assert!(gpu.wait().is_ok());
    }

    #[test]
    fn a_lost_device_stops_waiting_and_is_opened_again() {
        let (gpu, _) = Gpu::open_on(noop(), &Preference::Software).unwrap();
        gpu.device.destroy();
        gpu.poll();
        assert!(gpu.is_lost(), "{:?}", gpu.health());
        assert!(gpu.lost().is_some());
        assert!(matches!(gpu.wait(), Err(RenderError::Device(_))));
        // polling a lost device again does nothing
        gpu.poll();
        let (again, _) = gpu.reopen(&Preference::Software).unwrap();
        assert!(!again.is_lost());
        assert!(again.wait().is_ok());
    }

    #[test]
    fn the_windows_device_is_not_opened_again_here() {
        let (opened, _) = Gpu::open_on(noop(), &Preference::Software).unwrap();
        let window = Gpu::new(opened.device.clone(), opened.queue.clone());
        assert!(!window.can_reopen());
        assert_eq!(window.preference(), None);
        assert!(window.reopen(&Preference::Auto).is_err());
        let named = window.with_adapter(opened.adapter().cloned().unwrap());
        assert_eq!(named.adapter(), opened.adapter());
        // after an uncaptured error the output cannot be trusted, so it counts as a loss
        assert!(!named.is_lost());
        named.health().mark(Fault::Error("validation".into()));
        assert_eq!(named.lost().as_deref(), Some("validation"));
    }

    /// The compositor hands on an export's frame only when its submission reported no error, so a
    /// rejected submission must be counted before `submit` returns.
    #[test]
    fn a_rejected_submission_is_counted_before_submit_returns() {
        let (gpu, _) = Gpu::open_on(noop(), &Preference::Software).unwrap();
        let buffer = |usage| gpu.device.create_buffer(&wgpu::BufferDescriptor { label: None, size: 16, usage, mapped_at_creation: false });
        let (src, dst) = (buffer(wgpu::BufferUsages::COPY_SRC), buffer(wgpu::BufferUsages::COPY_DST));
        let errors = gpu.health().errors();
        gpu.queue.submit([gpu.device.create_command_encoder(&Default::default()).finish()]);
        assert_eq!(gpu.health().errors(), errors, "a valid submission");
        let mut bad = gpu.device.create_command_encoder(&Default::default());
        // A copy whose size is not a multiple of 4 bytes is invalid.
        bad.copy_buffer_to_buffer(&src, 0, &dst, 0, 3u64);
        gpu.queue.submit([bad.finish()]);
        assert!(gpu.health().errors() > errors, "the rejected submission is counted at once");
    }
}
