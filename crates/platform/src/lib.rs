//! Operating-system services behind one interface per need:
//!
//! - [`display`]: list displays and capture one.
//! - [`window`]: list windows and capture one.
//! - [`audio`]: list audio devices and capture an input or, on Windows, what an output plays.
//! - [`ffmpeg`]: find the ffmpeg program recordings, streams and media sources run.
//! - [`shell`]: open a folder in the file manager or a web page in the browser.
//!
//! Capture is implemented on Windows and macOS. Elsewhere the lists are empty and opening a
//! capture returns [`PlatformError::Unsupported`].

pub mod audio;
pub mod display;
pub mod ffmpeg;
pub mod shell;
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
}

#[cfg(any(windows, target_os = "macos"))]
impl PlatformError {
    pub(crate) fn failed(e: impl std::fmt::Display) -> Self {
        PlatformError::Failed(e.to_string())
    }
}
