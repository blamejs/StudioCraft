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
    /// Each destination of a stream, in the order the stream was started with; empty for other
    /// outputs.
    fn destinations(&self) -> Vec<DestinationState> {
        Vec::new()
    }
    /// Start sending to the destination `id` of a running stream, which was started with it.
    /// With `key`, a destination that is not sending starts with that stream key. Starting one
    /// that is already sending does nothing, and it keeps its key.
    fn start_destination(&self, id: &str, key: Option<&str>) -> Result<(), PluginError> {
        let _ = key;
        Err(PluginError::Unavailable(format!("this output has no destination `{id}`")))
    }
    /// Stop sending to the destination `id` of a running stream; the others go on.
    fn stop_destination(&self, id: &str) -> Result<(), PluginError> {
        Err(PluginError::Unavailable(format!("this output has no destination `{id}`")))
    }
}

/// What a stream's destination is doing.
#[derive(Clone, Debug, PartialEq)]
pub enum DestinationStatus {
    /// Connecting for the first time.
    Connecting,
    /// Connected and sending.
    Live,
    /// The connection was lost or could not be made; the next try is `attempt`.
    Retrying { attempt: u32, error: String },
    /// Stopped by request, or never started because it is off.
    Stopped,
    /// Given up on: the server refused the stream, or every try failed.
    Failed(String),
}

/// One destination of a running stream.
#[derive(Clone, Debug, PartialEq)]
pub struct DestinationState {
    pub id: String,
    /// The server, without the key.
    pub target: String,
    pub status: DestinationStatus,
    /// Bytes sent over the connections made so far.
    pub bytes: u64,
    /// Packets this destination lost because it fell behind.
    pub dropped: u64,
    /// Connections lost and made again.
    pub reconnects: u32,
}

/// The video and sound a replay buffer held when it was asked for them. Writing the clip does not
/// stop or slow the replay buffer, and a clip can still be written after the buffer stops.
pub trait ReplayClip: Send {
    /// Write the clip to `path`, replacing any file there. It can take a while, so it runs on a
    /// thread of its own. A file that could not be written whole is removed.
    fn write(self: Box<Self>, path: &Path) -> Result<(), PluginError>;
}
