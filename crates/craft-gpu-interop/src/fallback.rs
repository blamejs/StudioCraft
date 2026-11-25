//! Why a captured frame is copied through CPU memory instead of staying on the GPU.

use crate::ring::AdapterId;

/// A reason the GPU path is not used. The capture side copies through CPU memory instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Fallback {
    /// The compositor's device runs on neither Direct3D 12 nor Vulkan.
    UnsupportedBackend,
    /// The compositor runs on another adapter than the one that drives the display.
    OtherAdapter,
    /// Opening the shared textures or fences on the compositor's device failed.
    ImportFailed,
    /// The system has no desktop duplication, no window capture into textures or no shared fences
    /// (an older Windows, or another system).
    Unsupported,
    /// The desktop is not in 8-bit BGRA (an HDR desktop).
    Format,
    /// The display is rotated; desktop duplication gives its picture unrotated.
    Rotated,
    /// The compositor's Vulkan device was opened without `VK_KHR_external_memory_win32`, which
    /// opens shared Windows textures, or has no timeline semaphores (Vulkan 1.2). On Linux: it was
    /// opened without `VK_EXT_external_memory_dma_buf` and `VK_EXT_image_drm_format_modifier`
    /// (wgpu's `Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF`).
    MissingExtension,
    /// The hardware encoder cannot open a session on the compositor's graphics adapter, so the
    /// program frames it encodes go through CPU memory.
    NoEncoder,
    /// The system does not let this window be captured into textures (a window the capture API
    /// does not accept, or a capture the system or the person refused).
    Refused,
    /// The compositor's device cannot read the captured buffers' pixel format with their memory
    /// layout (their DRM format modifier), or a buffer's layout does not match the stream's. For
    /// program frames on Linux: the device cannot make an NV12 image of the ring's size that it
    /// can share as a DMA-BUF in any memory layout.
    Modifier,
}

impl Fallback {
    pub(crate) const ALL: [Fallback; 10] = [
        Fallback::UnsupportedBackend,
        Fallback::OtherAdapter,
        Fallback::ImportFailed,
        Fallback::Unsupported,
        Fallback::Format,
        Fallback::Rotated,
        Fallback::MissingExtension,
        Fallback::NoEncoder,
        Fallback::Refused,
        Fallback::Modifier,
    ];

    pub(crate) fn code(self) -> u8 {
        match self {
            Fallback::UnsupportedBackend => 1,
            Fallback::OtherAdapter => 2,
            Fallback::ImportFailed => 3,
            Fallback::Unsupported => 4,
            Fallback::Format => 5,
            Fallback::Rotated => 6,
            Fallback::MissingExtension => 7,
            Fallback::NoEncoder => 8,
            Fallback::Refused => 9,
            Fallback::Modifier => 10,
        }
    }

    pub(crate) fn from_code(code: u8) -> Option<Fallback> {
        Fallback::ALL.into_iter().find(|f| f.code() == code)
    }
}

impl std::fmt::Display for Fallback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Fallback::UnsupportedBackend => "the compositor runs on neither Direct3D 12 nor Vulkan",
            Fallback::OtherAdapter => "the compositor runs on another graphics adapter than the display",
            Fallback::ImportFailed => "the captured textures could not be opened on the compositor's device",
            Fallback::Unsupported => "this system cannot share captured textures between devices",
            Fallback::Format => "the desktop is not in 8-bit color",
            Fallback::Rotated => "the display is rotated",
            Fallback::MissingExtension => "the compositor's Vulkan device cannot open shared textures",
            Fallback::NoEncoder => "the hardware encoder cannot run on the compositor's graphics adapter",
            Fallback::Refused => "the system does not let this window be captured into textures",
            Fallback::Modifier => "the compositor's device cannot share the picture in a memory layout the other side reads",
        })
    }
}

/// The graphics API a compositor's device runs on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Dx12,
    /// `external_memory` is whether the device was opened with `VK_KHR_external_memory_win32`
    /// (wgpu's `Features::VULKAN_EXTERNAL_MEMORY_WIN32`); `timeline` is whether it has timeline
    /// semaphores whose value the CPU can read (Vulkan 1.2).
    Vulkan {
        external_memory: bool,
        timeline: bool,
    },
    Other,
}

/// Whether a ring captured on `capture` can be imported by a compositor on `backend` and
/// `compositor`. The backend is checked first, then the adapter, then the Vulkan device's
/// extension and semaphores. An unknown compositor adapter counts as another adapter.
pub fn check_import(backend: Backend, capture: AdapterId, compositor: Option<AdapterId>) -> Result<(), Fallback> {
    if backend == Backend::Other {
        return Err(Fallback::UnsupportedBackend);
    }
    if compositor != Some(capture) {
        return Err(Fallback::OtherAdapter);
    }
    if let Backend::Vulkan { external_memory, timeline } = backend
        && !(external_memory && timeline)
    {
        return Err(Fallback::MissingExtension);
    }
    Ok(())
}

/// The adapter an [`crate::ExportRing`] for a compositor on `backend` and `compositor` is made
/// on: the compositor's own. The backend is checked first, then whether the compositor's adapter
/// is known, then the Vulkan device's extension and semaphores, as for an import.
pub fn check_export(backend: Backend, compositor: Option<AdapterId>) -> Result<AdapterId, Fallback> {
    let Some(adapter) = compositor else {
        return Err(if backend == Backend::Other { Fallback::UnsupportedBackend } else { Fallback::OtherAdapter });
    };
    check_import(backend, adapter, Some(adapter)).map(|()| adapter)
}

/// The adapter a Linux [`crate::ExportRing`] of DMA-BUF buffers for a compositor on `backend` and
/// `compositor` is made on. The checks of [`check_export`] come first, with `external_memory`
/// meaning `VK_EXT_external_memory_dma_buf` and `VK_EXT_image_drm_format_modifier`; then
/// `modifiers`, the DRM format modifiers the device can export an NV12 image of the ring's size
/// with, must not be empty ([`Fallback::Modifier`]).
pub fn check_dmabuf_export(backend: Backend, compositor: Option<AdapterId>, modifiers: &[u64]) -> Result<AdapterId, Fallback> {
    let adapter = check_export(backend, compositor)?;
    if modifiers.is_empty() {
        return Err(Fallback::Modifier);
    }
    Ok(adapter)
}
