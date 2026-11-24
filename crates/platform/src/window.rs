//! Windows: listing and capture.
//!
//! A window is stored in settings as its program name and title. When the exact window is gone
//! (the program was restarted), capture picks the first window of the same program.
//!
//! On Windows, [`WindowCapture::share_on_gpu`] captures the window with Windows.Graphics.Capture
//! into textures shared with the compositor; the picture is copied through memory when it cannot.

use craft_gpu_interop::WindowCapture as GpuWindow;
use studiocraft_media::video::VideoFrame;

use crate::PlatformError;
use crate::display::{SharedGrab, Step, step};

/// A window the system reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowInfo {
    /// The program that owns the window.
    pub app: String,
    pub title: String,
}

impl WindowInfo {
    /// The key stored in settings.
    pub fn key(&self) -> String {
        format!("{}\u{1f}{}", self.app, self.title)
    }

    /// The label shown in lists: `[program] title`.
    pub fn label(&self) -> String {
        format!("[{}] {}", self.app, self.title)
    }

    pub fn parse_key(key: &str) -> WindowInfo {
        match key.split_once('\u{1f}') {
            Some((app, title)) => WindowInfo { app: app.into(), title: title.into() },
            None => WindowInfo { app: String::new(), title: key.into() },
        }
    }
}

/// Visible, titled windows.
pub fn windows() -> Vec<WindowInfo> {
    imp::windows()
}

/// The index of the best match for `want` in `list`: same program and title, then the same
/// title, then the same program.
pub fn best_match(list: &[WindowInfo], want: &WindowInfo) -> Option<usize> {
    list.iter()
        .position(|w| w == want)
        .or_else(|| list.iter().position(|w| !want.title.is_empty() && w.title == want.title))
        .or_else(|| list.iter().position(|w| !want.app.is_empty() && w.app == want.app))
}

/// A window being captured.
pub struct WindowCapture {
    inner: imp::Capture,
    /// The capture into textures shared with the compositor, while it runs.
    gpu: Option<GpuWindow>,
}

impl WindowCapture {
    pub fn open(key: &str) -> Result<WindowCapture, PlatformError> {
        Ok(WindowCapture { inner: imp::Capture::open(&WindowInfo::parse_key(key))?, gpu: None })
    }

    /// Capture into textures shared with the compositor where the system allows it (Windows, with
    /// window capture into textures and shared fences). Returns whether the GPU capture runs;
    /// elsewhere it does not, and [`WindowCapture::grab_shared`] returns [`SharedGrab::Memory`].
    pub fn share_on_gpu(&mut self) -> bool {
        if self.gpu.is_none() {
            self.gpu = self.inner.gpu_target().and_then(|(window, display)| GpuWindow::open(window, &display).ok());
        }
        self.gpu.is_some()
    }

    /// The newest picture in a shared texture, without waiting. While the window is minimized the
    /// last picture stays. Once the compositor has reported that it cannot take the shared
    /// textures, or the GPU capture fails, this returns [`SharedGrab::Memory`] from then on.
    pub fn grab_shared(&mut self) -> SharedGrab {
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

    /// The window's current picture into `frame` (RGBA). Minimized windows return an error. On
    /// Linux the window is chosen in the desktop's picker, as for [`crate::display::DisplayCapture`].
    pub fn grab(&mut self, frame: &mut VideoFrame) -> Result<(), PlatformError> {
        self.inner.grab(frame)
    }
}

#[cfg(any(windows, target_os = "macos"))]
mod imp {
    use studiocraft_media::video::VideoFrame;
    use xcap::Window;

    use super::{WindowInfo, best_match};
    use crate::PlatformError;
    use crate::display::fill;

    fn info(w: &Window) -> Option<WindowInfo> {
        let title = w.title().ok()?;
        if title.trim().is_empty() || w.width().ok()? < 2 || w.height().ok()? < 2 {
            return None;
        }
        Some(WindowInfo { app: w.app_name().unwrap_or_default(), title })
    }

    pub fn windows() -> Vec<WindowInfo> {
        Window::all().unwrap_or_default().iter().filter_map(info).collect()
    }

    pub struct Capture {
        window: Window,
    }

    impl Capture {
        pub fn open(want: &WindowInfo) -> Result<Capture, PlatformError> {
            let all: Vec<(Window, WindowInfo)> =
                Window::all().map_err(PlatformError::failed)?.into_iter().filter_map(|w| info(&w).map(|i| (w, i))).collect();
            let infos: Vec<WindowInfo> = all.iter().map(|(_, i)| i.clone()).collect();
            let index = best_match(&infos, want).ok_or_else(|| PlatformError::NotFound(format!("the window \u{201c}{}\u{201d}", want.title)))?;
            let window = all.into_iter().nth(index).map(|(w, _)| w).ok_or_else(|| PlatformError::NotFound("the window".into()))?;
            Ok(Capture { window })
        }

        pub fn grab(&mut self, frame: &mut VideoFrame) -> Result<(), PlatformError> {
            if self.window.is_minimized().unwrap_or(false) {
                return Err(PlatformError::Failed("the window is minimized".into()));
            }
            let img = self.window.capture_image().map_err(PlatformError::failed)?;
            fill(frame, img.width(), img.height(), img.as_raw())
        }

        /// The window's handle and the device name of the display it is on, for the capture into
        /// shared textures.
        #[cfg(windows)]
        pub fn gpu_target(&self) -> Option<(isize, String)> {
            let display = self.window.current_monitor().ok()?.name().ok()?;
            Some((super::handle(self.window.id().ok()?), display))
        }

        #[cfg(target_os = "macos")]
        pub fn gpu_target(&self) -> Option<(isize, String)> {
            None
        }
    }
}

/// A window handle from the low 32 bits xcap reports. On 64-bit Windows a window handle is a
/// 32-bit value extended with its sign bit, so the low bits name the window.
#[cfg_attr(not(windows), allow(dead_code))]
fn handle(id: u32) -> isize {
    id as i32 as isize
}

/// On Linux the desktop's picker chooses the window, so the list is empty and every key opens the
/// picker.
#[cfg(target_os = "linux")]
mod imp {
    use studiocraft_media::video::VideoFrame;

    use super::WindowInfo;
    use crate::PlatformError;
    use crate::portal::{Kind, PortalCapture};

    pub fn windows() -> Vec<WindowInfo> {
        Vec::new()
    }

    pub struct Capture(PortalCapture);

    impl Capture {
        pub fn open(_want: &WindowInfo) -> Result<Capture, PlatformError> {
            Ok(Capture(PortalCapture::open(Kind::Window)?))
        }

        pub fn grab(&mut self, frame: &mut VideoFrame) -> Result<(), PlatformError> {
            self.0.grab(frame)
        }

        pub fn gpu_target(&self) -> Option<(isize, String)> {
            None
        }
    }
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
mod imp {
    use studiocraft_media::video::VideoFrame;

    use super::WindowInfo;
    use crate::PlatformError;

    pub fn windows() -> Vec<WindowInfo> {
        Vec::new()
    }

    pub struct Capture;

    impl Capture {
        pub fn open(_want: &WindowInfo) -> Result<Capture, PlatformError> {
            Err(PlatformError::Unsupported)
        }

        pub fn grab(&mut self, _frame: &mut VideoFrame) -> Result<(), PlatformError> {
            Err(PlatformError::Unsupported)
        }

        pub fn gpu_target(&self) -> Option<(isize, String)> {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_handle_keeps_its_sign_bit() {
        assert_eq!(handle(0x0001_02a4), 0x0001_02a4);
        assert_eq!(handle(0xffff_fffe), -2, "the high bits of a 64-bit handle repeat the sign bit");
    }

    #[test]
    fn without_a_gpu_capture_window_pictures_are_copied_through_memory() {
        // Build machines without a desktop have no window to capture.
        let Some(first) = windows().into_iter().next() else { return };
        let Ok(mut cap) = WindowCapture::open(&first.key()) else { return };
        assert!(matches!(cap.grab_shared(), SharedGrab::Memory), "nothing shared before share_on_gpu");
        if !cap.share_on_gpu() {
            assert!(matches!(cap.grab_shared(), SharedGrab::Memory));
        }
    }

    fn w(app: &str, title: &str) -> WindowInfo {
        WindowInfo { app: app.into(), title: title.into() }
    }

    #[test]
    fn keys_round_trip() {
        let a = w("notepad.exe", "notes.txt - Notepad");
        assert_eq!(WindowInfo::parse_key(&a.key()), a);
        assert_eq!(WindowInfo::parse_key("just a title"), w("", "just a title"));
    }

    #[test]
    fn matching_prefers_the_exact_window_then_the_title_then_the_program() {
        let list = [w("a.exe", "One"), w("b.exe", "Two"), w("a.exe", "Three")];
        assert_eq!(best_match(&list, &w("a.exe", "Three")), Some(2));
        assert_eq!(best_match(&list, &w("c.exe", "Two")), Some(1));
        assert_eq!(best_match(&list, &w("a.exe", "Gone")), Some(0));
        assert_eq!(best_match(&list, &w("z.exe", "Gone")), None);
    }

    #[test]
    fn listing_does_not_fail() {
        for info in windows() {
            assert!(!info.title.trim().is_empty());
        }
    }
}
