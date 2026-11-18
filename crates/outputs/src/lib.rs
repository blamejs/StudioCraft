//! Recording, streaming and the replay buffer.
//!
//! With an ffmpeg encoder, both outputs run an ffmpeg process ([`writer`]): a recording writes a
//! Matroska, MP4 or QuickTime file, and a stream sends FLV over RTMP or RTMPS. With the built-in
//! encoders, a recording writes a Matroska file from this process ([`native`]). The video encoders
//! ffmpeg offers are plug-ins ([`encoders`]), and so are the built-in ones: H.264
//! ([`studiocraft_encode::H264`]) and NVENC H.264 and HEVC ([`studiocraft_encode::Nvenc`]). The
//! hardware choice ([`encoders::Hardware`]) is resolved by the studio to one of the ffmpeg encoders,
//! or to built-in NVENC for an MKV recording or the replay buffer, before an output starts. The
//! replay buffer ([`replay`]) runs the built-in encoders and keeps their newest packets in memory
//! until a save writes them to a Matroska file.

pub mod encoders;
pub mod native;
pub mod replay;
pub mod writer;

use std::path::Path;
use std::sync::Arc;

use studiocraft_media::audio::PooledBlock;
use studiocraft_media::queue::Fifo;
use studiocraft_media::video::PooledFrame;
use studiocraft_model::{Field, FieldKind, PathMode, Settings, SettingsSchema};
use studiocraft_plugin::{EncoderKind, Output, OutputInstance, OutputJob, OutputStatus, Plugin, PluginError, RateControl, Registry};

use crate::native::Native;
use crate::writer::{Writer, audio_args};

/// Register the recording, the stream, the replay buffer and every video encoder: the ffmpeg
/// ones, the hardware choice, then the built-in ones.
pub fn register_all(reg: &mut Registry) -> Result<(), PluginError> {
    reg.register_output(Arc::new(Recording))?;
    reg.register_output(Arc::new(Stream))?;
    reg.register_output(Arc::new(replay::Replay))?;
    for e in encoders::all() {
        reg.register_encoder(Arc::new(e))?;
    }
    reg.register_encoder(Arc::new(encoders::Hardware))?;
    reg.register_encoder(Arc::new(studiocraft_encode::H264))?;
    reg.register_encoder(Arc::new(studiocraft_encode::Nvenc::H264))?;
    reg.register_encoder(Arc::new(studiocraft_encode::Nvenc::HEVC))?;
    Ok(())
}

/// The codec arguments of `job`: its video encoder's, then audio. The video encoder must be a
/// video encoder the ffmpeg program runs.
fn codec_args(job: &OutputJob, lossless_audio: Option<&str>) -> Result<Vec<String>, PluginError> {
    if job.video_encoder.kind() != EncoderKind::Video {
        return Err(PluginError::Failed(format!("{} is not a video encoder", job.video_encoder.name())));
    }
    let fps = f64::from(job.video.fps_num) / f64::from(job.video.fps_den.max(1));
    let video = job
        .video_encoder
        .ffmpeg()
        .ok_or_else(|| PluginError::Unavailable(format!("{} does not run inside the ffmpeg program", job.video_encoder.name())))?;
    let mut a = video.ffmpeg_args(&job.video_encoder_settings, job.rate, fps)?;
    match lossless_audio {
        Some(codec) => a.extend(["-c:a".to_string(), codec.to_string()]),
        None => a.extend(audio_args(job.audio_bitrate_kbps)),
    }
    Ok(a)
}

/// The lossless audio codec for a container: FLAC in Matroska, ALAC in MP4 and QuickTime.
fn lossless_audio_codec(container: &str) -> &'static str {
    if container == "matroska" { "flac" } else { "alac" }
}

/// The ffmpeg container for a file name, by its extension.
pub fn container_for(path: &Path) -> Result<&'static str, PluginError> {
    match path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
        Some("mkv") => Ok("matroska"),
        Some("mp4") => Ok("mp4"),
        Some("mov") => Ok("mov"),
        _ => Err(PluginError::Failed(format!("{} does not end in .mkv, .mp4 or .mov", path.display()))),
    }
}

/// What encodes and writes a running output.
enum Backend {
    /// An ffmpeg process.
    Ffmpeg(Writer),
    /// The built-in encoders.
    Native(Native),
}

/// A running output: what writes it, and what it writes to.
struct Running {
    backend: Backend,
    target: String,
}

impl OutputInstance for Running {
    fn video(&self) -> Arc<Fifo<Arc<PooledFrame>>> {
        match &self.backend {
            Backend::Ffmpeg(w) => w.video(),
            Backend::Native(n) => n.video(),
        }
    }
    fn audio(&self) -> Arc<Fifo<PooledBlock>> {
        match &self.backend {
            Backend::Ffmpeg(w) => w.audio(),
            Backend::Native(n) => n.audio(),
        }
    }
    fn status(&self) -> OutputStatus {
        match &self.backend {
            Backend::Ffmpeg(w) => w.status(),
            Backend::Native(n) => n.status(),
        }
    }
    fn target(&self) -> String {
        self.target.clone()
    }
    fn stop(&mut self) -> Result<(), PluginError> {
        match &mut self.backend {
            Backend::Ffmpeg(w) => w.stop(),
            Backend::Native(n) => n.stop(),
        }
    }
}

/// A recording to a file.
pub struct Recording;

impl Plugin for Recording {
    fn id(&self) -> &'static str {
        "file"
    }
    fn name(&self) -> &'static str {
        "Recording"
    }
    fn schema(&self) -> SettingsSchema {
        let extensions = ["mkv", "mp4", "mov"].map(String::from).to_vec();
        SettingsSchema::new(vec![Field::new(
            "path",
            "File",
            FieldKind::Path { mode: PathMode::SaveFile, filter_name: "Recordings".into(), extensions },
            "",
        )])
    }
}

impl Output for Recording {
    fn start(&self, settings: &Settings, job: OutputJob) -> Result<Box<dyn OutputInstance>, PluginError> {
        let path = Path::new(settings.str("path").unwrap_or_default());
        if path.as_os_str().is_empty() {
            return Err(PluginError::Failed("no file name was given for the recording".into()));
        }
        let container = container_for(path)?;
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(|e| PluginError::Failed(format!("the folder {} could not be created: {e}", dir.display())))?;
        }
        let target = path.display().to_string();
        if job.video_encoder.ffmpeg().is_none() {
            if job.video_encoder.kind() != EncoderKind::Video {
                return Err(PluginError::Failed(format!("{} is not a video encoder", job.video_encoder.name())));
            }
            if container != "matroska" {
                return Err(PluginError::Failed(format!(
                    "{} records MKV files only; choose MKV as the format in Settings › Output",
                    job.video_encoder.name()
                )));
            }
            return Ok(Box::new(Running { backend: Backend::Native(Native::start(&job, path)?), target }));
        }
        let lossless_audio = (job.rate == RateControl::Lossless).then(|| lossless_audio_codec(container));
        let codec = codec_args(&job, lossless_audio)?;
        let mut out: Vec<String> = vec!["-f".into(), container.into()];
        if container != "matroska" {
            // MP4 and QuickTime files get their index moved to the front when the recording ends.
            out.extend(["-movflags", "+faststart"].map(String::from));
        }
        out.push(path.to_string_lossy().into_owned());
        let writer = Writer::start(&job, &codec, &out, "")?;
        Ok(Box::new(Running { backend: Backend::Ffmpeg(writer), target }))
    }
}

/// A live stream to an RTMP or RTMPS server.
pub struct Stream;

impl Plugin for Stream {
    fn id(&self) -> &'static str {
        "rtmp"
    }
    fn name(&self) -> &'static str {
        "Stream"
    }
    fn schema(&self) -> SettingsSchema {
        SettingsSchema::new(vec![
            Field::new("server", "Server", FieldKind::Text { multiline: false }, ""),
            Field::new("key", "Stream key", FieldKind::Text { multiline: false }, ""),
        ])
    }
}

/// The URL ffmpeg publishes to: the server with the key appended.
pub fn stream_url(server: &str, key: &str) -> Result<String, PluginError> {
    let server = server.trim();
    let lower = server.to_ascii_lowercase();
    if !(lower.starts_with("rtmp://") || lower.starts_with("rtmps://")) {
        return Err(PluginError::Failed("the server must start with rtmp:// or rtmps://".into()));
    }
    let key = key.trim();
    if key.chars().any(char::is_whitespace) {
        return Err(PluginError::Failed("the stream key cannot contain spaces".into()));
    }
    Ok(if key.is_empty() { server.to_string() } else { format!("{}/{key}", server.trim_end_matches('/')) })
}

impl Output for Stream {
    fn start(&self, settings: &Settings, job: OutputJob) -> Result<Box<dyn OutputInstance>, PluginError> {
        let server = settings.str("server").unwrap_or_default();
        let url = stream_url(server, settings.str("key").unwrap_or_default())?;
        if !matches!(job.rate, RateControl::Bitrate { .. }) {
            return Err(PluginError::Failed("streams need a bitrate".into()));
        }
        if job.video_encoder.kind() != EncoderKind::Video || !job.video_encoder.streams() {
            return Err(PluginError::Failed(format!("streams need an H.264 encoder, and {} is not one", job.video_encoder.name())));
        }
        if job.video_encoder.ffmpeg().is_none() {
            return Err(PluginError::Failed(format!(
                "{} records files only; choose an ffmpeg encoder for streams in Settings › Output",
                job.video_encoder.name()
            )));
        }
        let codec = codec_args(&job, None)?;
        let out = vec!["-f".into(), "flv".into(), "-flvflags".into(), "no_duration_filesize".into(), url];
        let writer = Writer::start(&job, &codec, &out, settings.str("key").unwrap_or_default().trim())?;
        // The key is a secret: the target shown is the server alone.
        Ok(Box::new(Running { backend: Backend::Ffmpeg(writer), target: server.trim().to_string() }))
    }
}

#[cfg(test)]
mod tests;
