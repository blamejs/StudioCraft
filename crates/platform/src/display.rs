//! Displays: listing and capture.

use studiocraft_media::video::VideoFrame;

use crate::PlatformError;

/// A display the system reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DisplayInfo {
    /// Stable for the session; stored in a source's settings.
    pub id: String,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

/// Displays, the primary first.
pub fn displays() -> Vec<DisplayInfo> {
    let mut list = imp::displays();
    list.sort_by_key(|d| !d.primary);
    list
}

/// A display being captured.
pub struct DisplayCapture {
    inner: imp::Capture,
}

impl DisplayCapture {
    /// Capture the display with `id`; an empty or unknown id captures the primary display.
    pub fn open(id: &str) -> Result<DisplayCapture, PlatformError> {
        Ok(DisplayCapture { inner: imp::Capture::open(id)? })
    }

    /// The current picture into `frame` (RGBA).
    pub fn grab(&mut self, frame: &mut VideoFrame) -> Result<(), PlatformError> {
        self.inner.grab(frame)
    }
}

/// Copy an RGBA picture into `frame`.
#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn fill(frame: &mut VideoFrame, width: u32, height: u32, rgba: &[u8]) -> Result<(), PlatformError> {
    if !frame.reshape(width, height, studiocraft_media::video::PixelFormat::Rgba8) || frame.data.len() != rgba.len() {
        return Err(PlatformError::Failed(format!("a {width} x {height} picture could not be used")));
    }
    frame.data.copy_from_slice(rgba);
    Ok(())
}

#[cfg(any(windows, target_os = "macos"))]
mod imp {
    use studiocraft_media::video::VideoFrame;
    use xcap::Monitor;

    use super::{DisplayInfo, fill};
    use crate::PlatformError;

    pub fn displays() -> Vec<DisplayInfo> {
        Monitor::all().unwrap_or_default().iter().filter_map(info).collect()
    }

    fn info(m: &Monitor) -> Option<DisplayInfo> {
        let name = m.friendly_name().or_else(|_| m.name()).unwrap_or_default();
        Some(DisplayInfo {
            id: m.id().ok()?.to_string(),
            name: if name.is_empty() { "Display".into() } else { name },
            width: m.width().ok()?,
            height: m.height().ok()?,
            primary: m.is_primary().unwrap_or(false),
        })
    }

    pub struct Capture {
        monitor: Monitor,
    }

    impl Capture {
        pub fn open(id: &str) -> Result<Capture, PlatformError> {
            let all = Monitor::all().map_err(PlatformError::failed)?;
            let by_id = all.iter().position(|m| m.id().ok().map(|i| i.to_string()).as_deref() == Some(id));
            let primary = all.iter().position(|m| m.is_primary().unwrap_or(false));
            let index = by_id.or(primary).or((!all.is_empty()).then_some(0)).ok_or_else(|| PlatformError::NotFound("a display".into()))?;
            let monitor = all.into_iter().nth(index).ok_or_else(|| PlatformError::NotFound("a display".into()))?;
            Ok(Capture { monitor })
        }

        pub fn grab(&mut self, frame: &mut VideoFrame) -> Result<(), PlatformError> {
            let img = self.monitor.capture_image().map_err(PlatformError::failed)?;
            fill(frame, img.width(), img.height(), img.as_raw())
        }
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod imp {
    use studiocraft_media::video::VideoFrame;

    use super::DisplayInfo;
    use crate::PlatformError;

    pub fn displays() -> Vec<DisplayInfo> {
        Vec::new()
    }

    pub struct Capture;

    impl Capture {
        pub fn open(_id: &str) -> Result<Capture, PlatformError> {
            Err(PlatformError::Unsupported)
        }

        pub fn grab(&mut self, _frame: &mut VideoFrame) -> Result<(), PlatformError> {
            Err(PlatformError::Unsupported)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listing_and_capturing_the_primary_display() {
        let list = displays();
        // Machines without a display (build servers) have nothing to capture.
        let Some(first) = list.first() else { return };
        assert!(first.width > 0 && first.height > 0);
        let Ok(mut cap) = DisplayCapture::open("") else { return };
        let mut f = VideoFrame::default();
        if cap.grab(&mut f).is_ok() {
            assert!(f.is_consistent());
        }
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn fill_checks_the_size() {
        let mut f = VideoFrame::default();
        assert!(fill(&mut f, 2, 1, &[0; 8]).is_ok());
        assert!(fill(&mut f, 2, 2, &[0; 8]).is_err());
    }
}
