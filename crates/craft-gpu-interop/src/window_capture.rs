//! Window capture into a ring of shared textures: what the capture does with each picture the
//! system delivers, apart from the system calls, which are in `wgc_ffi`.

use std::sync::Arc;
use std::time::Instant;

use crate::fallback::Fallback;
use crate::ring::{SharedFrame, SharedRing, SlotTable};
use crate::{CaptureError, Grab};

/// The longest side of a shared texture (Direct3D 11's limit).
pub(crate) const MAX_SIDE: u32 = 16_384;

const E_NOTIMPL: i32 = 0x8000_4001_u32 as i32;
const E_NOINTERFACE: i32 = 0x8000_4002_u32 as i32;
const REGDB_E_CLASSNOTREG: i32 = 0x8004_0154_u32 as i32;
const E_ACCESSDENIED: i32 = 0x8007_0005_u32 as i32;
const ERROR_INVALID_WINDOW_HANDLE: i32 = 0x8007_0578_u32 as i32;

/// The step of opening a window capture that failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// Checking that the system can capture windows into textures.
    Support,
    /// Making the capture item for the window.
    Item,
    /// Handing the Direct3D 11 device to the capture API.
    Device,
    /// Making the frame pool.
    Pool,
    /// Making or starting the capture session.
    Session,
}

/// The error for a failure with the system code `code` (an `HRESULT`) at `step` of opening a
/// window capture.
pub(crate) fn open_error(step: Step, code: i32, message: &str) -> CaptureError {
    if matches!(code, E_NOTIMPL | E_NOINTERFACE | REGDB_E_CLASSNOTREG) {
        // The capture classes or one of their interfaces are missing: an older Windows.
        return CaptureError::Fallback(Fallback::Unsupported);
    }
    match step {
        Step::Support => CaptureError::Fallback(Fallback::Unsupported),
        // The window closed between the listing and the opening; it is looked for again.
        Step::Item if code == ERROR_INVALID_WINDOW_HANDLE => CaptureError::Lost(format!("the window is gone: {message}")),
        // The system does not accept this window, for example one on another desktop.
        Step::Item => CaptureError::Fallback(Fallback::Refused),
        _ if code == E_ACCESSDENIED => CaptureError::Fallback(Fallback::Refused),
        Step::Device | Step::Pool | Step::Session => CaptureError::Failed(format!("starting the window capture: {message}")),
    }
}

/// The sides of a texture for a window of `width × height`, or the error when no texture can hold
/// it.
pub(crate) fn sides(width: i32, height: i32) -> Result<(u32, u32), CaptureError> {
    match (u32::try_from(width), u32::try_from(height)) {
        (Ok(w), Ok(h)) if w > 0 && h > 0 && w <= MAX_SIDE && h <= MAX_SIDE => Ok((w, h)),
        (Ok(w), Ok(h)) if w > MAX_SIDE || h > MAX_SIDE => {
            Err(CaptureError::Failed(format!("the window ({w} x {h}) is larger than a shared texture can hold")))
        }
        _ => Err(CaptureError::Lost("the window has no area".into())),
    }
}

/// What the capture does with a picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Plan {
    /// The window has no area (it is minimized): the picture is dropped.
    Skip,
    /// The window's size differs from the frame pool's buffers: the buffers are made this size,
    /// and the picture, which holds only part of the window or a margin outside it, is dropped.
    ResizePool(i32, i32),
    /// The picture is copied into the ring, after making a ring of the picture's size when the
    /// ring has another size.
    Copy { new_ring: bool },
}

/// The plan for a picture of a window that measured `content` when it was taken, in a buffer of
/// `picture`, with the frame pool's buffers at `pool` and the ring at `ring`.
pub(crate) fn plan(content: (i32, i32), picture: (u32, u32), pool: (i32, i32), ring: (u32, u32)) -> Result<Plan, CaptureError> {
    if content.0 <= 0 || content.1 <= 0 || picture.0 == 0 || picture.1 == 0 {
        return Ok(Plan::Skip);
    }
    sides(content.0, content.1)?;
    if content != pool {
        return Ok(Plan::ResizePool(content.0, content.1));
    }
    sides(i32::try_from(picture.0).unwrap_or(i32::MAX), i32::try_from(picture.1).unwrap_or(i32::MAX))?;
    Ok(Plan::Copy { new_ring: picture != ring })
}

/// The system side of a window capture: the window, the frame pool and the ring's textures.
pub(crate) trait Target {
    type Picture;
    /// Whether the window still exists.
    fn alive(&self) -> bool;
    /// The newest picture delivered since the last call, without waiting.
    fn next(&mut self) -> Result<Option<Self::Picture>, CaptureError>;
    /// The window's size when the picture was taken, and the size of the buffer it is in.
    fn sizes(picture: &Self::Picture) -> ((i32, i32), (u32, u32));
    /// The size of the frame pool's buffers.
    fn pool_size(&self) -> (i32, i32);
    fn resize_pool(&mut self, width: i32, height: i32) -> Result<(), CaptureError>;
    /// Replace the ring's textures with ones of `width × height`.
    fn new_ring(&mut self, width: u32, height: u32) -> Result<Arc<SharedRing>, CaptureError>;
    /// The released fence's value of the current ring.
    fn released(&self) -> u64;
    /// Copy `picture` into texture `slot` and signal `ready` after the copy.
    fn copy(&mut self, picture: Self::Picture, slot: usize, ready: u64) -> Result<(), CaptureError>;
}

/// A window captured into a ring of shared textures, with the slot bookkeeping.
pub(crate) struct Capture<T: Target> {
    /// The newest picture, kept while every texture is still being read. It is declared before
    /// the target, so it goes back to the frame pool before the pool is closed.
    held: Option<T::Picture>,
    pub(crate) target: T,
    ring: Arc<SharedRing>,
    slots: SlotTable,
}

impl<T: Target> Capture<T> {
    pub(crate) fn new(mut target: T) -> Result<Capture<T>, CaptureError> {
        let (width, height) = target.pool_size();
        let (width, height) = sides(width, height)?;
        let ring = target.new_ring(width, height)?;
        Ok(Capture { held: None, target, ring, slots: SlotTable::default() })
    }

    pub(crate) fn ring(&self) -> &Arc<SharedRing> {
        &self.ring
    }

    /// The newest picture of the window, copied into the ring, without waiting.
    pub(crate) fn grab(&mut self, now: Instant) -> Result<Grab, CaptureError> {
        if let Some(reason) = self.ring.fallback() {
            return Err(CaptureError::Fallback(reason));
        }
        if !self.target.alive() {
            return Err(CaptureError::Lost("the window was closed".into()));
        }
        if let Some(picture) = self.target.next()? {
            self.held = Some(picture);
        }
        let Some(picture) = self.held.take() else { return Ok(Grab::Unchanged) };
        let (content, size) = T::sizes(&picture);
        match plan(content, size, self.target.pool_size(), (self.ring.width(), self.ring.height()))? {
            Plan::Skip => return Ok(Grab::Unchanged),
            Plan::ResizePool(width, height) => {
                drop(picture);
                self.target.resize_pool(width, height)?;
                return Ok(Grab::Unchanged);
            }
            Plan::Copy { new_ring: true } => {
                // Frames already handed out keep the old ring alive until they are drawn.
                self.ring = self.target.new_ring(size.0, size.1)?;
                self.slots = SlotTable::default();
            }
            Plan::Copy { new_ring: false } => {}
        }
        // Every texture is still being read: the picture stays held and is copied next time,
        // unless a newer one replaces it.
        let Some(slot) = self.slots.pick(&self.ring, self.target.released(), now) else {
            self.held = Some(picture);
            return Ok(Grab::Unchanged);
        };
        let Some(ready) = self.slots.fill(&self.ring, slot, now) else { return Err(CaptureError::Lost("the fence values ran out".into())) };
        self.target.copy(picture, slot, ready)?;
        SharedFrame::new(Arc::clone(&self.ring), slot, ready)
            .map(Grab::Frame)
            .ok_or_else(|| CaptureError::Failed("no such texture in the ring".into()))
    }
}

#[cfg(windows)]
impl Target for crate::wgc_ffi::Session {
    type Picture = crate::wgc_ffi::Picture;

    fn alive(&self) -> bool {
        crate::wgc_ffi::Session::alive(self)
    }

    fn next(&mut self) -> Result<Option<Self::Picture>, CaptureError> {
        crate::wgc_ffi::Session::next(self)
    }

    fn sizes(picture: &Self::Picture) -> ((i32, i32), (u32, u32)) {
        (picture.content, picture.size)
    }

    fn pool_size(&self) -> (i32, i32) {
        self.pool_size
    }

    fn resize_pool(&mut self, width: i32, height: i32) -> Result<(), CaptureError> {
        crate::wgc_ffi::Session::resize_pool(self, width, height)
    }

    fn new_ring(&mut self, width: u32, height: u32) -> Result<Arc<SharedRing>, CaptureError> {
        let handles = crate::wgc_ffi::Session::new_ring(self, width, height)?;
        // A window's picture keeps its alpha, as its copy through memory does.
        Ok(Arc::new(SharedRing::new(width, height, self.adapter, true, handles)))
    }

    fn released(&self) -> u64 {
        crate::wgc_ffi::Session::released(self)
    }

    fn copy(&mut self, picture: Self::Picture, slot: usize, ready: u64) -> Result<(), CaptureError> {
        crate::wgc_ffi::Session::copy(self, picture, slot, ready)
    }
}
