//! Outputs: recordings, streams and the replay buffer.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use studiocraft_media::Tick;
use studiocraft_media::audio::PooledBlock;
use studiocraft_media::queue::Fifo;
use studiocraft_media::stats::EncoderStats;
use studiocraft_media::video::{PixelFormat, PooledFrame};
use studiocraft_model::Settings;

use crate::encoder::{Encoder, RateControl};
use crate::{Plugin, PluginError};

/// Frames an output receives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoFormat {
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    pub pixel: PixelFormat,
}

/// Everything an output needs to start.
#[derive(Clone)]
pub struct OutputJob {
    pub video: VideoFormat,
    pub sample_rate: u32,
    pub video_encoder: Arc<dyn Encoder>,
    pub video_encoder_settings: Settings,
    pub rate: RateControl,
    pub audio_bitrate_kbps: u32,
    /// The ffmpeg program to run.
    pub ffmpeg: PathBuf,
    pub stats: Arc<EncoderStats>,
    /// The master-clock time the output starts at: earlier frames and audio are not written.
    pub start: Tick,
}

/// An output type.
pub trait Output: Plugin {
    /// Start writing. The returned instance owns its encoder thread and process.
    fn start(&self, settings: &Settings, job: OutputJob) -> Result<Box<dyn OutputInstance>, PluginError>;
}

/// The state of a running output.
#[derive(Clone, Debug, PartialEq)]
pub enum OutputStatus {
    Starting,
    Running,
    /// Stopped by request; the file or stream ended cleanly.
    Stopped,
    Failed(String),
}

/// A running output. The compositor pushes frames into [`OutputInstance::video`] and the mixer
/// pushes blocks into [`OutputInstance::audio`]; neither waits for the encoder. Frames are shared:
/// a recording and a stream running together receive the same converted frame.
pub trait OutputInstance: Send {
    fn video(&self) -> Arc<Fifo<Arc<PooledFrame>>>;
    fn audio(&self) -> Arc<Fifo<PooledBlock>>;
    fn status(&self) -> OutputStatus;
    /// Where it writes: a file path, or the server without the key.
    fn target(&self) -> String;
    /// Ask it to finish and wait (bounded) until the file or stream is closed.
    fn stop(&mut self) -> Result<(), PluginError>;
    /// What the output keeps in memory now, to be written to a file while it keeps running. Only
    /// a replay buffer keeps anything; other outputs return an error.
    fn replay(&self) -> Result<Box<dyn ReplayClip>, PluginError> {
        Err(PluginError::Unavailable("only the replay buffer keeps what it encodes".into()))
    }
}

/// The video and sound a replay buffer held when it was asked for them. Writing the clip does not
/// stop or slow the replay buffer, and a clip can still be written after the buffer stops.
pub trait ReplayClip: Send {
    /// Write the clip to `path`, replacing any file there. It can take a while, so it runs on a
    /// thread of its own. A file that could not be written whole is removed.
    fn write(self: Box<Self>, path: &Path) -> Result<(), PluginError>;
}
