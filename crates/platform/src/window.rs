//! Windows: listing and capture.
//!
//! A window is stored in settings as its program name and title. When the exact window is gone
//! (the program was restarted), capture picks the first window of the same program.

use studiocraft_media::video::VideoFrame;

use crate::PlatformError;

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
}

impl WindowCapture {
    pub fn open(key: &str) -> Result<WindowCapture, PlatformError> {
        Ok(WindowCapture { inner: imp::Capture::open(&WindowInfo::parse_key(key))? })
    }

    /// The window's current picture into `frame` (RGBA). Minimized windows return an error.
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
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
