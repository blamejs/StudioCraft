//! Per-application audio for a Craft app's mixer:
//!
//! - [`Discovery`] keeps a list of the running applications that have audio ([`AppList`]),
//!   current as they start and quit, on one thread of its own. While a panel shows the list it
//!   also reads each application's peak level into a fixed table of atomics, without capturing
//!   the application.
//! - [`Discovery::open_capture`] captures one application's audio on a thread of its own, as
//!   48 kHz stereo `f32` samples, and hands each packet to a callback. A capture that stops
//!   answering is torn down off the caller's thread.
//! - [`Backend`] is the operating system's side. Windows 11 and Windows Server 2022 (build 20348)
//!   and later capture through WASAPI process loopback. Other systems report capture as
//!   unsupported with a reason. [`fake::FakeBackend`] is a scripted backend for tests.
//!
//! The crate depends on no app crate, and the callers decide how samples reach their mixer.

mod backoff;
mod capture;
mod discovery;
pub mod fake;
mod key;
mod list;
mod peaks;
mod reaper;
#[cfg(windows)]
mod win;

use std::sync::Arc;

pub use backoff::Backoff;
pub use capture::{AppCapture, CaptureShared, CaptureState, CaptureStatus, OnData, Target, now_ms};
pub use discovery::{Discovery, DiscoveryUse, RESCAN_MS};
pub use key::AppKey;
pub use list::{AppInfo, AppList};
pub use peaks::{PEAK_SLOTS, PeakTable};

/// The sample rate of captured audio.
pub const SAMPLE_RATE: u32 = 48_000;
/// The channel count of captured audio (interleaved left, right).
pub const CHANNELS: usize = 2;
/// The most application captures that run at once.
pub const MAX_CAPTURES: usize = 32;
/// A capture that has not reported for this long while it should be running counts as stalled.
pub const WEDGE_MS: u64 = 2_000;
/// How long a capture thread waits for the system before it checks for a stop request.
pub const CAPTURE_WAIT_MS: u32 = 200;

/// Why an application's audio could not be listed or captured. The texts are English; an app
/// translates them before showing them.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AppAudioError {
    /// This system cannot capture one application's audio; the text says why.
    #[error("{0}")]
    Unsupported(String),
    /// The application is not running, or has no audio.
    #[error("{0} is not running")]
    NotRunning(String),
    /// [`MAX_CAPTURES`] captures already run.
    #[error("{MAX_CAPTURES} applications are already being captured")]
    Limit,
    /// The system reported an error.
    #[error("{0}")]
    Failed(String),
}

/// Whether the system lets the app capture other applications' audio.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    /// The system asks for no permission.
    NotNeeded,
    /// The person allowed it.
    Granted,
    /// The person refused it in the system's privacy settings.
    Denied,
}

/// What this system supports.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Support {
    /// Whether one application's audio can be captured.
    pub supported: bool,
    /// Why not, when it can't. English.
    pub reason: Option<String>,
    pub permission: Permission,
}

impl Support {
    /// Supported, with no permission to ask for.
    pub fn yes() -> Support {
        Support { supported: true, reason: None, permission: Permission::NotNeeded }
    }

    /// Not supported, for `reason`.
    pub fn no(reason: impl Into<String>) -> Support {
        Support { supported: false, reason: Some(reason.into()), permission: Permission::NotNeeded }
    }
}

/// Wakes the discovery thread from a system notification thread. Waking never blocks; a wake that
/// finds one already pending is dropped.
#[derive(Clone)]
pub struct Wake(std::sync::mpsc::SyncSender<discovery::Cmd>);

impl Wake {
    /// Ask the discovery thread to list the applications again soon.
    pub fn wake(&self) {
        let _ = self.0.try_send(discovery::Cmd::Rescan);
    }
}

/// The operating system's side of application audio.
pub trait Backend: Send + Sync + 'static {
    /// Whether capture works here, and why not.
    fn support(&self) -> Support;

    /// Start watching for applications. Called on the discovery thread, which keeps the returned
    /// watcher and calls it from there only. `wake` may be called from any thread.
    fn watch(&self, wake: Wake) -> Result<Box<dyn Watcher>, AppAudioError>;

    /// Capture `target` until `shared` asks to stop, giving each packet of 48 kHz stereo samples
    /// to `on_data`. Runs on a capture thread of its own. It must call [`CaptureShared::beat`] at
    /// least every [`CAPTURE_WAIT_MS`] while it waits, call [`CaptureShared::running`] once the
    /// system delivers audio, and return when [`CaptureShared::stopping`] is true.
    fn capture(&self, target: Target, shared: &CaptureShared, on_data: &mut OnData) -> Result<(), AppAudioError>;
}

/// Lists applications and reads their peak levels. It lives on the discovery thread.
pub trait Watcher {
    /// The running applications that have audio.
    fn scan(&mut self) -> Result<Vec<AppInfo>, AppAudioError>;

    /// The current peak level, 0 to 1, of each application in `apps` (from the last scan), given
    /// to `out` with the application's index. Applications without a reading are left out.
    fn peaks(&mut self, apps: &[AppInfo], out: &mut dyn FnMut(usize, f32));
}

/// The backend of the system the app runs on.
pub fn system_backend() -> Arc<dyn Backend> {
    #[cfg(windows)]
    {
        Arc::new(win::WinBackend)
    }
    #[cfg(not(windows))]
    {
        Arc::new(Unsupported)
    }
}

/// Why a system other than Windows does not capture one application. Interfaces show it through a
/// translation row with this text.
pub const NOT_AVAILABLE: &str = "Capturing one application's audio is not available on this system yet.";
/// Why a Windows build before 20348 does not capture one application. Interfaces show it through a
/// translation row with this text.
pub const NEEDS_NEWER_WINDOWS: &str =
    "Capturing one application needs Windows 11 or Windows Server 2022. Audio output capture still records everything the computer plays.";

/// The backend for systems without application capture yet.
#[cfg(not(windows))]
struct Unsupported;

#[cfg(not(windows))]
impl Backend for Unsupported {
    fn support(&self) -> Support {
        Support::no(NOT_AVAILABLE)
    }

    fn watch(&self, _wake: Wake) -> Result<Box<dyn Watcher>, AppAudioError> {
        Err(AppAudioError::Unsupported(self.support().reason.unwrap_or_default()))
    }

    fn capture(&self, _target: Target, _shared: &CaptureShared, _on_data: &mut OnData) -> Result<(), AppAudioError> {
        Err(AppAudioError::Unsupported(self.support().reason.unwrap_or_default()))
    }
}

#[cfg(test)]
mod tests;
