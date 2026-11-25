//! Operating-system services behind one interface per need:
//!
//! - [`display`]: list displays and capture one.
//! - [`window`]: list windows and capture one.
//! - [`camera`]: list cameras and their formats and capture one.
//! - [`audio`]: list audio devices and capture an input or, on Windows and Linux, what an output
//!   plays.
//! - [`ffmpeg`]: find the ffmpeg program recordings, streams and media sources run.
//! - [`shell`]: open a folder in the file manager or a web page in the browser.
//! - [`virtual_camera`]: send the program to a device other apps open as a camera (Linux).
//! - [`net`]: whether this computer has a network route, without sending anything.
//!
//! Capture is implemented on Windows, macOS and Linux. On Linux, displays and windows are chosen in
//! the desktop's own picker through the screen-cast portal, audio goes through PipeWire and
//! cameras through V4L2. The Linux display list comes from Wayland's outputs, or from RandR under
//! X11. Elsewhere the lists are empty and opening a capture returns
//! [`PlatformError::Unsupported`].

pub mod audio;
pub mod camera;
mod camera_frames;
#[cfg(target_os = "linux")]
mod camera_gray;
mod camera_watch;
pub mod display;
#[cfg(target_os = "linux")]
mod display_list;
pub mod ffmpeg;
pub mod net;
#[cfg(target_os = "linux")]
mod portal;
#[cfg(target_os = "linux")]
mod pw_loop;
mod screen_frames;
pub mod shell;
pub mod virtual_camera;
pub mod window;

/// Why an operating-system service failed.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum PlatformError {
    #[error("this is not supported on this system yet")]
    Unsupported,
    #[error("{0} was not found")]
    NotFound(String),
    #[error("{0}")]
    Failed(String),
    /// The capture is open and waits for a choice in the system picker. Keep the capture.
    #[error("waiting for a choice in the system picker")]
    Waiting,
    /// The capture is running and the picture has not changed since the last one. Keep the
    /// capture and the last picture.
    #[error("the picture has not changed")]
    Unchanged,
    /// The person ended the capture: they canceled the system picker or stopped sharing. Do not
    /// open it again until its settings change.
    #[error("{0}")]
    Stopped(String),
    /// The device is still busy with earlier requests. The same request can succeed later.
    #[error("the audio device is busy")]
    Busy,
}

#[cfg(any(windows, target_os = "macos"))]
impl PlatformError {
    pub(crate) fn failed(e: impl std::fmt::Display) -> Self {
        PlatformError::Failed(e.to_string())
    }
}
