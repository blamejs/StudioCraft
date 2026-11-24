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
    /// The system has no desktop duplication or no shared fences (Windows before 10 version 1703,
    /// or another system).
    Unsupported,
    /// The desktop is not in 8-bit BGRA (an HDR desktop).
    Format,
    /// The display is rotated; desktop duplication gives its picture unrotated.
    Rotated,
    /// The compositor's Vulkan device was opened without `VK_KHR_external_memory_win32`, which
    /// opens shared Windows textures, or has no timeline semaphores (Vulkan 1.2).
    MissingExtension,
    /// The hardware encoder cannot open a session on the compositor's graphics adapter, so the
    /// program frames it encodes go through CPU memory.
    NoEncoder,
}

impl Fallback {
    pub(crate) const ALL: [Fallback; 8] = [
        Fallback::UnsupportedBackend,
        Fallback::OtherAdapter,
        Fallback::ImportFailed,
        Fallback::Unsupported,
        Fallback::Format,
        Fallback::Rotated,
        Fallback::MissingExtension,
        Fallback::NoEncoder,
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
            Fallback::MissingExtension => "the compositor's Vulkan device cannot open shared Windows textures",
            Fallback::NoEncoder => "the hardware encoder cannot run on the compositor's graphics adapter",
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
