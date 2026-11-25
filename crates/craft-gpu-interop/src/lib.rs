//! Captured frames shared with a compositor on the GPU, without a copy through CPU memory.
//!
//! On Windows, [`Duplication`] captures one display with DXGI desktop duplication on a Direct3D 11
//! device of the adapter that drives the display. Each new desktop picture is copied, on that GPU,
//! into one texture of a [`SharedRing`]: three textures and two fences shared through NT handles.
//! The capture hands out a [`SharedFrame`], which names the ring, the texture and the fence value
//! that marks the copy as done. [`WindowCapture`] does the same for one window with
//! Windows.Graphics.Capture, on a Direct3D 11 device of the adapter that drives the display the
//! window is on. When the window's size changes, its pictures go into a new ring of the new size.
//!
//! With the `wgpu` feature, an [`Importer`] opens the ring's textures and fences on a wgpu
//! Direct3D 12 or Vulkan device and draws the frame into the caller's texture. A display's frame is
//! drawn with alpha set to 1. A window's frame keeps its alpha, and its color, which the system
//! delivers multiplied by alpha, is drawn as straight color ([`SharedRing::keeps_alpha`]). It never
//! waits: a frame whose copy has not finished stays pending, and the next call tries
//! again. After the draw it signals the ring's released fence, which tells the capture side that
//! the texture may be written again. On Direct3D 12 the queue signals the fence. On Vulkan the
//! textures are opened through `VK_KHR_external_memory_win32` and the fences on a Direct3D 12
//! device of the same adapter. A timeline semaphore signaled by the queue tells the importer when
//! the draw has finished, and the importer then sets the released fence from the CPU.
//!
//! When the frame cannot stay on the GPU, the importer records a [`Fallback`] on the ring. The
//! capture side reads it with [`SharedRing::fallback`] and goes back to a CPU copy. The reasons are
//! a compositor on neither Direct3D 12 nor Vulkan, a compositor on another adapter than the
//! display, a Vulkan device without `VK_KHR_external_memory_win32` or timeline semaphores, an
//! import that failed, a system without shared fences or without window capture into textures,
//! and a window the system does not let be captured that way. Elsewhere than Windows the crate
//! builds, [`Duplication::open`] and [`WindowCapture::open`] return [`Fallback::Unsupported`], and
//! an import reports the same.
//!
//! The other direction carries the compositor's program frames to a hardware encoder. An
//! [`ExportRing`] holds three NV12 textures and two fences on a Direct3D 11 device of the
//! compositor's adapter, the device the encoder opens its session on, shared through NT handles.
//! With the `wgpu` feature, an [`Exporter`] opens them on the compositor's Direct3D 12 or Vulkan
//! device, copies each frame's luma and chroma planes into a free texture, and sets the ready
//! fence after the copy. The encoder side keeps its frames in a [`Feed`], takes each one once its
//! copy has finished, and sets the released fence once the frame's packet is out. Neither side
//! waits on the other. [`check_export`] gives the reason an export cannot run, and an encoder that
//! cannot open a session on the adapter records [`Fallback::NoEncoder`] on the ring; the
//! compositor then reads its frames back through CPU memory.
//!
//! The system calls are in private modules with `unsafe` allowed; every item this crate exports
//! is safe.

#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(windows)]
mod duplication;
mod export;
mod fallback;
#[cfg(feature = "wgpu")]
mod import;
mod ring;
#[cfg_attr(not(windows), allow(dead_code))]
mod window_capture;

#[cfg(all(windows, feature = "wgpu"))]
#[allow(unsafe_code)]
mod d3d12_ffi;
#[cfg(windows)]
#[allow(unsafe_code)]
mod dxgi_ffi;
#[cfg(all(windows, feature = "wgpu"))]
#[allow(unsafe_code)]
mod vulkan_ffi;
#[cfg(windows)]
#[allow(unsafe_code)]
mod wgc_ffi;

#[cfg(test)]
mod tests;

pub use export::{ExportFrame, ExportRing, Feed};
#[cfg(feature = "wgpu")]
pub use export::{Exporter, Ticket, device_backend};
pub use fallback::{Backend, Fallback, check_export, check_import};
#[cfg(feature = "wgpu")]
pub use import::{Drawn, Importer, Opaque};
pub use ring::{AdapterId, Handoff, RECLAIM, SLOTS, SharedFrame, SharedRing, SlotTable};

/// What one call to [`Duplication::grab`] or [`WindowCapture::grab`] gave.
#[derive(Debug)]
pub enum Grab {
    /// A new picture, copied into the ring.
    Frame(SharedFrame),
    /// The desktop or window has not changed, or every texture of the ring is still in use.
    Unchanged,
}

/// Why a capture stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaptureError {
    /// The capture cannot run on the GPU; the caller copies through CPU memory instead.
    Fallback(Fallback),
    /// The desktop changed mode, was locked or switched, the window was closed, or the device was
    /// lost. Opening the display or window again may work.
    Lost(String),
    /// Another system error.
    Failed(String),
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaptureError::Fallback(r) => write!(f, "the capture is copied through memory: {r}"),
            CaptureError::Lost(m) => write!(f, "the capture on the GPU was interrupted: {m}"),
            CaptureError::Failed(m) => write!(f, "the capture on the GPU failed: {m}"),
        }
    }
}

impl std::error::Error for CaptureError {}

/// One display captured into a [`SharedRing`].
pub struct Duplication {
    #[cfg(windows)]
    inner: duplication::Duplication,
}

impl Duplication {
    /// Capture the display whose device name is `device_name` (for example `\\.\DISPLAY1`, the
    /// name the system gives the display's monitor).
    pub fn open(device_name: &str) -> Result<Duplication, CaptureError> {
        #[cfg(windows)]
        {
            Ok(Duplication { inner: duplication::Duplication::open(device_name)? })
        }
        #[cfg(not(windows))]
        {
            let _ = device_name;
            Err(CaptureError::Fallback(Fallback::Unsupported))
        }
    }

    /// The ring this capture writes.
    pub fn ring(&self) -> Option<&std::sync::Arc<SharedRing>> {
        #[cfg(windows)]
        {
            Some(self.inner.ring())
        }
        #[cfg(not(windows))]
        {
            None
        }
    }

    /// The newest desktop picture, without waiting for one. Returns [`CaptureError::Fallback`]
    /// once the consumer has recorded a reason on the ring.
    pub fn grab(&mut self) -> Result<Grab, CaptureError> {
        #[cfg(windows)]
        {
            self.inner.grab(std::time::Instant::now())
        }
        #[cfg(not(windows))]
        {
            Err(CaptureError::Fallback(Fallback::Unsupported))
        }
    }
}

/// One window captured into a [`SharedRing`], or into a new ring after the window's size changes.
pub struct WindowCapture {
    #[cfg(windows)]
    inner: window_capture::Capture<wgc_ffi::Session>,
}

impl WindowCapture {
    /// Capture the window whose handle is `window` (its `HWND` as a number) on a Direct3D 11 device
    /// of the adapter that drives the display named `display` (for example `\\.\DISPLAY1`), the
    /// display the window is on. An older Windows without window capture into textures gives
    /// [`Fallback::Unsupported`], and a window the system does not let be captured that way gives
    /// [`Fallback::Refused`]. Elsewhere than Windows this returns [`Fallback::Unsupported`].
    pub fn open(window: isize, display: &str) -> Result<WindowCapture, CaptureError> {
        #[cfg(windows)]
        {
            Ok(WindowCapture { inner: window_capture::Capture::new(wgc_ffi::Session::open(window, display)?)? })
        }
        #[cfg(not(windows))]
        {
            let _ = (window, display);
            Err(CaptureError::Fallback(Fallback::Unsupported))
        }
    }

    /// The ring the newest picture was written to.
    pub fn ring(&self) -> Option<&std::sync::Arc<SharedRing>> {
        #[cfg(windows)]
        {
            Some(self.inner.ring())
        }
        #[cfg(not(windows))]
        {
            None
        }
    }

    /// The newest picture of the window, without waiting for one. Returns
    /// [`CaptureError::Fallback`] once the consumer has recorded a reason on the ring, and
    /// [`CaptureError::Lost`] once the window is closed.
    pub fn grab(&mut self) -> Result<Grab, CaptureError> {
        #[cfg(windows)]
        {
            self.inner.grab(std::time::Instant::now())
        }
        #[cfg(not(windows))]
        {
            Err(CaptureError::Fallback(Fallback::Unsupported))
        }
    }
}
