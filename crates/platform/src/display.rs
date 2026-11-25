//! Displays: listing and capture.

use craft_gpu_interop::{CaptureError, Duplication, Grab, SharedFrame};
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

/// Displays, the primary first. On Linux this does not wait for the system: it returns the list the
/// last query answered and starts a new query, so a change reaches the next call.
pub fn displays() -> Vec<DisplayInfo> {
    let mut list = imp::displays();
    list.sort_by_key(|d| !d.primary);
    list
}

/// Start reading the display list in the background on Linux, so the first [`displays`] call has
/// an answer. Elsewhere this does nothing.
pub fn prefetch() {
    #[cfg(target_os = "linux")]
    crate::display_list::displays();
}

/// A display being captured.
pub struct DisplayCapture {
    inner: imp::Capture,
    /// The capture into textures shared with the compositor, while it runs.
    gpu: Option<Duplication>,
}

/// What [`DisplayCapture::grab_shared`] or [`crate::window::WindowCapture::grab_shared`] gave.
#[derive(Debug)]
pub enum SharedGrab {
    /// A new picture in a texture shared with the compositor.
    Frame(SharedFrame),
    /// The display or window has not changed since the last picture.
    Unchanged,
    /// The capture does not run on the GPU (any more); [`DisplayCapture::grab`] copies the
    /// picture into memory.
    Memory,
}

/// What the GPU capture does after one grab.
#[derive(Debug)]
pub(crate) enum Step {
    Give(SharedGrab),
    /// The desktop changed mode or was switched, or the window was lost: open the capture again.
    Reopen,
    /// Stop the GPU capture and copy through memory from now on.
    Stop,
}

pub(crate) fn step(grab: Result<Grab, CaptureError>) -> Step {
    match grab {
        Ok(Grab::Frame(f)) => Step::Give(SharedGrab::Frame(f)),
        Ok(Grab::Unchanged) => Step::Give(SharedGrab::Unchanged),
        Err(CaptureError::Lost(_)) => Step::Reopen,
        Err(CaptureError::Fallback(_) | CaptureError::Failed(_)) => Step::Stop,
    }
}

impl DisplayCapture {
    /// Capture the display with `id`; an empty or unknown id captures the primary display. On
    /// Linux this opens the desktop's picker and returns without waiting for the choice.
    pub fn open(id: &str) -> Result<DisplayCapture, PlatformError> {
        Ok(DisplayCapture { inner: imp::Capture::open(id, true)?, gpu: None })
    }

    /// Capture the display with `id`, or fail with [`PlatformError::NotFound`] when no connected
    /// display has that id. On Linux this opens the desktop's picker, as [`DisplayCapture::open`]
    /// does.
    pub fn open_exact(id: &str) -> Result<DisplayCapture, PlatformError> {
        Ok(DisplayCapture { inner: imp::Capture::open(id, false)?, gpu: None })
    }

    /// Capture the display with `id`, as [`DisplayCapture::open`] does, into textures shared with
    /// the compositor where the system allows it ([`DisplayCapture::share_on_gpu`]). On Linux the
    /// stream is asked for DMA-BUFs before the picker opens.
    pub fn open_shared(id: &str) -> Result<DisplayCapture, PlatformError> {
        #[cfg(target_os = "linux")]
        let inner = {
            let _ = id;
            imp::Capture::open_shared()?
        };
        #[cfg(not(target_os = "linux"))]
        let inner = imp::Capture::open(id, true)?;
        let mut capture = DisplayCapture { inner, gpu: None };
        capture.share_on_gpu();
        Ok(capture)
    }

    /// Capture into textures shared with the compositor where the system allows it: on Windows
    /// with desktop duplication and shared fences, and on Linux with DMA-BUF buffers from the
    /// screen-cast stream once the compositor has published what its Vulkan device imports.
    /// Returns whether the GPU capture was started; elsewhere it is not, and
    /// [`DisplayCapture::grab_shared`] returns [`SharedGrab::Memory`]. On Linux the stream decides
    /// when it negotiates, and [`DisplayCapture::grab_shared`] returns [`SharedGrab::Memory`]
    /// while its buffers are in memory.
    pub fn share_on_gpu(&mut self) -> bool {
        if let Some(on) = self.inner.share_on_gpu() {
            return on;
        }
        if self.gpu.is_none() {
            self.gpu = self.inner.device_name().and_then(|name| Duplication::open(&name).ok());
        }
        self.gpu.is_some()
    }

    /// The newest picture in a shared texture, without waiting. Once the compositor has reported
    /// that it cannot take the shared textures, or the GPU capture fails, this returns
    /// [`SharedGrab::Memory`] from then on.
    pub fn grab_shared(&mut self) -> SharedGrab {
        if let Some(grab) = self.inner.grab_shared() {
            return grab;
        }
        let Some(gpu) = &mut self.gpu else { return SharedGrab::Memory };
        match step(gpu.grab()) {
            Step::Give(g) => g,
            Step::Reopen => {
                self.gpu = None;
                if self.share_on_gpu() { SharedGrab::Unchanged } else { SharedGrab::Memory }
            }
            Step::Stop => {
                self.gpu = None;
                SharedGrab::Memory
            }
        }
    }

    /// The current picture into `frame` (RGBA, or BGRA on Linux). On Linux the picture arrives
    /// once a display has been chosen in the desktop's picker. Until then this returns
    /// [`PlatformError::Waiting`], and while the screen does not change [`PlatformError::Unchanged`].
    pub fn grab(&mut self, frame: &mut VideoFrame) -> Result<(), PlatformError> {
        self.inner.grab(frame)
    }
}

/// A size in a display's reported units as pixels, `scale` pixels to the unit. A scale that is
/// not a finite number from 1/8 to 8 is taken as 1.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
fn pixels(width: u32, height: u32, scale: f32) -> (u32, u32) {
    let k = if scale.is_finite() && (0.125..=8.0).contains(&scale) { f64::from(scale) } else { 1.0 };
    let side = |v: u32| (f64::from(v) * k).round().clamp(0.0, f64::from(u32::MAX)) as u32;
    (side(width), side(height))
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

    use super::{DisplayInfo, fill, pixels};
    use crate::PlatformError;

    pub fn displays() -> Vec<DisplayInfo> {
        Monitor::all().unwrap_or_default().iter().filter_map(info).collect()
    }

    fn info(m: &Monitor) -> Option<DisplayInfo> {
        let name = m.friendly_name().or_else(|_| m.name()).unwrap_or_default();
        let (width, height) = pixels(m.width().ok()?, m.height().ok()?, scale(m));
        Some(DisplayInfo {
            id: m.id().ok()?.to_string(),
            name: if name.is_empty() { "Display".into() } else { name },
            width,
            height,
            primary: m.is_primary().unwrap_or(false),
        })
    }

    /// Pixels per reported unit: macOS reports a display's size in points, Windows in pixels.
    #[cfg(target_os = "macos")]
    fn scale(m: &Monitor) -> f32 {
        m.scale_factor().unwrap_or(1.0)
    }

    #[cfg(windows)]
    fn scale(_m: &Monitor) -> f32 {
        1.0
    }

    pub struct Capture {
        monitor: Monitor,
    }

    impl Capture {
        /// The display with `id`; with `fallback`, the primary display (or the first) when no
        /// display has that id.
        pub fn open(id: &str, fallback: bool) -> Result<Capture, PlatformError> {
            let all = Monitor::all().map_err(PlatformError::failed)?;
            let by_id = all.iter().position(|m| m.id().ok().map(|i| i.to_string()).as_deref() == Some(id));
            let primary = all.iter().position(|m| m.is_primary().unwrap_or(false)).or((!all.is_empty()).then_some(0));
            let index = if fallback { by_id.or(primary) } else { by_id }.ok_or_else(|| PlatformError::NotFound("a display".into()))?;
            let monitor = all.into_iter().nth(index).ok_or_else(|| PlatformError::NotFound("a display".into()))?;
            Ok(Capture { monitor })
        }

        pub fn grab(&mut self, frame: &mut VideoFrame) -> Result<(), PlatformError> {
            let img = self.monitor.capture_image().map_err(PlatformError::failed)?;
            fill(frame, img.width(), img.height(), img.as_raw())
        }

        /// The display's device name (`\\.\DISPLAY1`), which desktop duplication looks the
        /// display up by.
        #[cfg(windows)]
        pub fn device_name(&self) -> Option<String> {
            self.monitor.name().ok()
        }

        #[cfg(target_os = "macos")]
        pub fn device_name(&self) -> Option<String> {
            None
        }

        /// The capture's own GPU path; this one has none, and desktop duplication takes over.
        pub fn share_on_gpu(&mut self) -> Option<bool> {
            None
        }

        pub fn grab_shared(&mut self) -> Option<super::SharedGrab> {
            None
        }
    }
}

/// On Linux the list comes from Wayland or X11, and the desktop's picker chooses the display to
/// capture, so every id opens the picker.
#[cfg(target_os = "linux")]
mod imp {
    use studiocraft_media::video::VideoFrame;

    use super::DisplayInfo;
    use crate::PlatformError;
    use crate::portal::{Kind, PortalCapture};

    pub fn displays() -> Vec<DisplayInfo> {
        crate::display_list::displays()
    }

    pub struct Capture(PortalCapture);

    impl Capture {
        pub fn open(_id: &str, _fallback: bool) -> Result<Capture, PlatformError> {
            Ok(Capture(PortalCapture::open(Kind::Display)?))
        }

        /// A capture whose stream is asked for DMA-BUFs before the portal thread starts.
        pub fn open_shared() -> Result<Capture, PlatformError> {
            Ok(Capture(PortalCapture::open_with(Kind::Display, true)?))
        }

        pub fn grab(&mut self, frame: &mut VideoFrame) -> Result<(), PlatformError> {
            self.0.grab(frame)
        }

        pub fn device_name(&self) -> Option<String> {
            None
        }

        /// The stream's DMA-BUF buffers, imported by the compositor.
        pub fn share_on_gpu(&mut self) -> Option<bool> {
            Some(self.0.share_on_gpu())
        }

        pub fn grab_shared(&mut self) -> Option<super::SharedGrab> {
            Some(self.0.grab_shared())
        }
    }
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
mod imp {
    use studiocraft_media::video::VideoFrame;

    use super::DisplayInfo;
    use crate::PlatformError;

    pub fn displays() -> Vec<DisplayInfo> {
        Vec::new()
    }

    pub struct Capture;

    impl Capture {
        pub fn open(_id: &str, _fallback: bool) -> Result<Capture, PlatformError> {
            Err(PlatformError::Unsupported)
        }

        pub fn grab(&mut self, _frame: &mut VideoFrame) -> Result<(), PlatformError> {
            Err(PlatformError::Unsupported)
        }

        pub fn device_name(&self) -> Option<String> {
            None
        }

        pub fn share_on_gpu(&mut self) -> Option<bool> {
            None
        }

        pub fn grab_shared(&mut self) -> Option<super::SharedGrab> {
            None
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

    #[test]
    fn the_gpu_capture_stops_on_a_fallback_and_reopens_after_a_mode_change() {
        let ring = craft_gpu_interop::SharedRing::detached(2, 2);
        let frame = SharedFrame::new(ring, 0, 1).unwrap();
        assert!(matches!(step(Ok(Grab::Frame(frame))), Step::Give(SharedGrab::Frame(f)) if f.ready() == 1));
        assert!(matches!(step(Ok(Grab::Unchanged)), Step::Give(SharedGrab::Unchanged)));
        assert!(matches!(step(Err(CaptureError::Lost("mode".into()))), Step::Reopen));
        for e in [CaptureError::Fallback(craft_gpu_interop::Fallback::UnsupportedBackend), CaptureError::Failed("x".into())] {
            assert!(matches!(step(Err(e)), Step::Stop));
        }
    }

    #[test]
    fn without_a_gpu_capture_pictures_are_copied_through_memory() {
        // Machines without a display (build servers) have nothing to capture.
        if displays().is_empty() {
            return;
        }
        let Ok(mut cap) = DisplayCapture::open("") else { return };
        assert!(matches!(cap.grab_shared(), SharedGrab::Memory), "nothing shared before share_on_gpu");
        if !cap.share_on_gpu() {
            assert!(matches!(cap.grab_shared(), SharedGrab::Memory));
        }
    }

    #[test]
    fn a_size_in_points_becomes_pixels() {
        assert_eq!(pixels(1512, 982, 2.0), (3024, 1964));
        assert_eq!(pixels(1920, 1080, 1.0), (1920, 1080));
        assert_eq!(pixels(1280, 800, 1.5), (1920, 1200));
        for bad in [f32::NAN, f32::INFINITY, 0.0, -2.0, 100.0] {
            assert_eq!(pixels(1920, 1080, bad), (1920, 1080), "{bad}");
        }
        assert_eq!(pixels(u32::MAX, 1, 8.0), (u32::MAX, 8));
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn an_exact_open_does_not_fall_back_to_another_display() {
        assert!(DisplayCapture::open_exact("no such display").is_err());
        // Where a display exists, its own id opens it.
        if let Some(first) = displays().first() {
            assert!(DisplayCapture::open_exact(&first.id).is_ok());
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
