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
    /// Finished caption phrases for a stream to carry as closed captions; `None` sends none.
    /// Phrases already in the queue when the stream starts are discarded.
    pub captions: Option<Arc<Fifo<String>>>,
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
    /// The sum of [`DestinationState::reconnects`] over the destinations of a stream, read without
    /// allocating; 0 for other outputs.
    fn reconnects(&self) -> u64 {
        0
    }
    /// How many destinations of a stream are live and how many wait to try again, read without
    /// allocating; both 0 for other outputs.
    fn destination_counts(&self) -> DestinationCounts {
        DestinationCounts::default()
    }
    /// How many times a destination of a stream went live again after it lost its connection,
    /// summed over the destinations and read without allocating; 0 for other outputs.
    fn recoveries(&self) -> u64 {
        0
    }
    /// Start sending to the destination `id` of a running stream, which was started with it.
    /// With `key`, a destination that is not sending starts with that stream key. Starting one
    /// that is already sending does nothing, and it keeps its key.
    fn start_destination(&self, id: &str, key: Option<&str>) -> Result<(), PluginError> {
        let _ = key;
        Err(PluginError::Unavailable(format!("this output has no destination `{id}`")))
    }
    /// Start sending to the destination `setup.id` of a running stream, at its server and with
    /// its key as `setup` gives them now. A destination the stream was not started with is added
    /// to it. One that is not sending, or was asked to stop and has not stopped yet, takes the
    /// server, key and service of `setup` first. Starting one that is already sending does
    /// nothing, and it keeps its server and key. Returns whether the destination took `setup`.
    fn start_destination_as(&self, setup: &DestinationSetup) -> Result<bool, PluginError> {
        Err(PluginError::Unavailable(format!("this output has no destination `{}`", setup.id)))
    }
    /// Whether the destination `id` of a running stream sends or tries to and was not asked to
    /// stop, so a start of it would keep its server and key.
    fn destination_sending(&self, id: &str) -> bool {
        let _ = id;
        false
    }
    /// Stop sending to the destination `id` of a running stream; the others go on.
    fn stop_destination(&self, id: &str) -> Result<(), PluginError> {
        Err(PluginError::Unavailable(format!("this output has no destination `{id}`")))
    }
    /// The file a stream writes a copy of what it sends to, when it was started with one; `None`
    /// for other outputs.
    fn stream_copy(&self) -> Option<StreamCopyState> {
        None
    }
}

/// A destination of a stream as it is set up now, for [`OutputInstance::start_destination_as`].
#[derive(Clone, PartialEq, Eq)]
pub struct DestinationSetup {
    pub id: String,
    /// `rtmp://host/app` or `rtmps://host/app`.
    pub server: String,
    /// The stream key; empty when the server's address ends with the stream's name.
    pub key: String,
    /// The id of the streaming service it is set up for; empty for none.
    pub service: String,
}

impl std::fmt::Debug for DestinationSetup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let key = if self.key.is_empty() { "" } else { "<redacted>" };
        f.debug_struct("DestinationSetup").field("id", &self.id).field("key", &key).field("service", &self.service).finish_non_exhaustive()
    }
}

/// The copy of a stream written to a file: the same encoded video and sound the destinations get.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StreamCopyState {
    /// The file.
    pub path: PathBuf,
    /// Bytes written to the file so far.
    pub bytes: u64,
    /// Packets the file did not get because the disk fell behind: the copy then waits for the next
    /// keyframe.
    pub lost_packets: u64,
    /// Whether the file was finished after the stream ended.
    pub finished: bool,
    /// Why the copy stopped, when it did; the stream goes on without it.
    pub failure: Option<String>,
}

/// How many destinations of a stream are live and how many wait to try again.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DestinationCounts {
    /// [`DestinationStatus::Live`].
    pub live: u32,
    /// [`DestinationStatus::Retrying`] and [`DestinationStatus::WaitingForNetwork`].
    pub retrying: u32,
}

/// What a stream's destination is doing.
#[derive(Clone, Debug, PartialEq)]
pub enum DestinationStatus {
    /// Connecting for the first time.
    Connecting,
    /// Connected and sending.
    Live,
    /// The connection was lost or could not be made; the next try is `attempt`.
    Retrying { attempt: u32, failure: Failure },
    /// This computer has no network route to the server; try `attempt` starts when one returns.
    /// `failure` is the last failure.
    WaitingForNetwork { attempt: u32, failure: Failure },
    /// Stopped by request, or never started because it is off.
    Stopped,
    /// Given up on: the server refused the stream, or every try failed.
    Failed(Failure),
}

/// Why a destination's connection failed or ended.
#[derive(Clone, Debug, PartialEq)]
pub struct Failure {
    pub cause: Cause,
    /// The error as the connection reported it, in English, without the stream key.
    pub detail: String,
    /// When no address accepted the connection: each address tried, in order, with its outcome;
    /// at most four. Empty otherwise.
    pub addresses: Vec<AddressOutcome>,
}

impl Failure {
    /// A failure of `cause` with no addresses.
    pub fn new(cause: Cause, detail: impl Into<String>) -> Failure {
        Failure { cause, detail: detail.into(), addresses: Vec::new() }
    }
}

/// How the connection to one address of a destination's server ended.
#[derive(Clone, Debug, PartialEq)]
pub struct AddressOutcome {
    /// The IP address and port.
    pub address: String,
    pub cause: Cause,
    pub detail: String,
}

/// The kind of failure, with a stable id ([`Cause::id`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
    /// This computer has no network route to any of the server's addresses that were tried.
    NoRoute,
    /// The server's name could not be looked up.
    LookupFailed,
    /// The DNS server answered that the server's name does not exist (Windows only).
    LookupNotFound,
    /// The DNS server did not answer for the server's name: no answer within the lookup time on
    /// every system, or the Windows resolver's answer that the server did not reply.
    LookupNoAnswer,
    /// No lookup was started because earlier lookups of server names have not finished.
    LookupsPending,
    /// The server refused the TCP connection.
    Refused,
    /// The TCP connection was not accepted in time.
    TimedOut,
    /// The server's certificate does not chain to a trusted root.
    CertificateUntrusted,
    /// The server's certificate is not valid for its name.
    CertificateNameMismatch,
    /// The server's certificate has expired.
    CertificateExpired,
    /// The certificate check refused the server's certificate for another reason.
    CertificateNotAccepted,
    /// The secure connection failed for a reason other than the certificate.
    TlsFailed,
    /// The server closed or reset the connection before the stream started.
    ClosedDuringSetup,
    /// The server did not finish setting up the stream in time.
    SetupTimedOut,
    /// The server refused the `connect` command: usually the application in the address.
    RejectedConnect,
    /// The server refused to take the stream: usually the stream key.
    RejectedPublish,
    /// The connection was lost while the stream was live.
    Lost,
    /// The server address is not valid.
    BadAddress,
    /// The stream key is not valid.
    BadKey,
    /// Anything else.
    Other,
}

impl Cause {
    /// Every cause, in the order of [`Cause::id`]'s list.
    pub const ALL: [Cause; 20] = [
        Cause::NoRoute,
        Cause::LookupFailed,
        Cause::LookupNotFound,
        Cause::LookupNoAnswer,
        Cause::LookupsPending,
        Cause::Refused,
        Cause::TimedOut,
        Cause::CertificateUntrusted,
        Cause::CertificateNameMismatch,
        Cause::CertificateExpired,
        Cause::CertificateNotAccepted,
        Cause::TlsFailed,
        Cause::ClosedDuringSetup,
        Cause::SetupTimedOut,
        Cause::RejectedConnect,
        Cause::RejectedPublish,
        Cause::Lost,
        Cause::BadAddress,
        Cause::BadKey,
        Cause::Other,
    ];

    /// The stable id the control channel uses.
    pub fn id(self) -> &'static str {
        match self {
            Cause::NoRoute => "no_route",
            Cause::LookupFailed => "lookup_failed",
            Cause::LookupNotFound => "lookup_not_found",
            Cause::LookupNoAnswer => "lookup_no_answer",
            Cause::LookupsPending => "lookups_pending",
            Cause::Refused => "refused",
            Cause::TimedOut => "timed_out",
            Cause::CertificateUntrusted => "certificate_untrusted",
            Cause::CertificateNameMismatch => "certificate_name_mismatch",
            Cause::CertificateExpired => "certificate_expired",
            Cause::CertificateNotAccepted => "certificate_not_accepted",
            Cause::TlsFailed => "tls_failed",
            Cause::ClosedDuringSetup => "closed_during_setup",
            Cause::SetupTimedOut => "setup_timed_out",
            Cause::RejectedConnect => "rejected_connect",
            Cause::RejectedPublish => "rejected_publish",
            Cause::Lost => "lost",
            Cause::BadAddress => "bad_address",
            Cause::BadKey => "bad_key",
            Cause::Other => "other",
        }
    }
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
