//! Captured frames shared with a compositor on the GPU, without a copy through CPU memory.
//!
//! On Windows, [`Duplication`] captures one display with DXGI desktop duplication on a Direct3D 11
//! device of the adapter that drives the display. Each new desktop picture is copied, on that GPU,
//! into one texture of a [`SharedRing`]: three textures and two fences shared through NT handles.
//! The capture hands out a [`SharedFrame`], which names the ring, the texture and the fence value
//! that marks the copy as done.
//!
//! With the `wgpu` feature, an [`Importer`] opens the ring's textures and fences on a wgpu
//! Direct3D 12 device and draws the frame into the caller's texture with alpha set to 1. It never
//! waits: a frame whose copy has not finished stays pending, and the next call tries again. After
//! the draw it signals the ring's released fence on its queue, which tells the capture side that
//! the texture may be written again.
//!
//! When the frame cannot stay on the GPU, the importer records a [`Fallback`] on the ring. The
//! capture side reads it with [`SharedRing::fallback`] and goes back to a CPU copy. The reasons are
//! a compositor that is not on Direct3D 12, a compositor on another adapter than the display, an
//! import that failed, and a system without shared fences. Elsewhere than Windows the crate builds,
//! [`Duplication::open`] returns [`Fallback::Unsupported`], and an import reports the same.
//!
//! The system calls are in private modules with `unsafe` allowed; every item this crate exports
//! is safe.

#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(windows)]
mod duplication;
mod fallback;
#[cfg(feature = "wgpu")]
mod import;
mod ring;

#[cfg(all(windows, feature = "wgpu"))]
#[allow(unsafe_code)]
mod d3d12_ffi;
#[cfg(windows)]
#[allow(unsafe_code)]
mod dxgi_ffi;

#[cfg(test)]
mod tests;

pub use fallback::{Backend, Fallback, check_import};
#[cfg(feature = "wgpu")]
pub use import::{Drawn, Importer, Opaque};
pub use ring::{AdapterId, Handoff, RECLAIM, SLOTS, SharedFrame, SharedRing, SlotTable};

/// What one call to [`Duplication::grab`] gave.
#[derive(Debug)]
pub enum Grab {
    /// A new desktop picture, copied into the ring.
    Frame(SharedFrame),
    /// The desktop has not changed, or every texture of the ring is still in use.
    Unchanged,
}

/// Why a capture stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaptureError {
    /// The capture cannot run on the GPU; the caller copies through CPU memory instead.
    Fallback(Fallback),
    /// The desktop changed mode, was locked or switched, or the device was lost. Opening the
    /// display again may work.
    Lost(String),
    /// Another system error.
    Failed(String),
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaptureError::Fallback(r) => write!(f, "the capture is copied through memory: {r}"),
            CaptureError::Lost(m) => write!(f, "the display capture was interrupted: {m}"),
            CaptureError::Failed(m) => write!(f, "the display capture failed: {m}"),
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
