//! Why a captured frame is copied through CPU memory instead of staying on the GPU.

use crate::ring::AdapterId;

/// A reason the GPU path is not used. The capture side copies through CPU memory instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Fallback {
    /// The compositor's device does not run on Direct3D 12.
    NotDx12,
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
}

impl Fallback {
    pub(crate) const ALL: [Fallback; 6] =
        [Fallback::NotDx12, Fallback::OtherAdapter, Fallback::ImportFailed, Fallback::Unsupported, Fallback::Format, Fallback::Rotated];

    pub(crate) fn code(self) -> u8 {
        match self {
            Fallback::NotDx12 => 1,
            Fallback::OtherAdapter => 2,
            Fallback::ImportFailed => 3,
            Fallback::Unsupported => 4,
            Fallback::Format => 5,
            Fallback::Rotated => 6,
        }
    }

    pub(crate) fn from_code(code: u8) -> Option<Fallback> {
        Fallback::ALL.into_iter().find(|f| f.code() == code)
    }
}

impl std::fmt::Display for Fallback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Fallback::NotDx12 => "the compositor does not run on Direct3D 12",
            Fallback::OtherAdapter => "the compositor runs on another graphics adapter than the display",
            Fallback::ImportFailed => "the captured textures could not be opened on the compositor's device",
            Fallback::Unsupported => "this system cannot share captured textures between devices",
            Fallback::Format => "the desktop is not in 8-bit color",
            Fallback::Rotated => "the display is rotated",
        })
    }
}

/// The graphics API a compositor's device runs on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Dx12,
    Other,
}

/// Whether a ring captured on `capture` can be imported by a compositor on `backend` and
/// `compositor`. An unknown compositor adapter counts as another adapter.
pub fn check_import(backend: Backend, capture: AdapterId, compositor: Option<AdapterId>) -> Result<(), Fallback> {
    if backend != Backend::Dx12 {
        return Err(Fallback::NotDx12);
    }
    if compositor != Some(capture) {
        return Err(Fallback::OtherAdapter);
    }
    Ok(())
}
