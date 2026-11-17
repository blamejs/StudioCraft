//! Recording and streaming.
//!
//! Both outputs run an ffmpeg process ([`writer`]): a recording writes a Matroska, MP4 or
//! QuickTime file, and a stream sends FLV over RTMP or RTMPS. The video encoders ffmpeg offers
//! are plug-ins ([`encoders`]).

pub mod encoders;
pub mod writer;

use std::path::Path;
use std::sync::Arc;

use studiocraft_media::audio::PooledBlock;
use studiocraft_media::queue::Fifo;
use studiocraft_media::video::PooledFrame;
use studiocraft_model::{Field, FieldKind, PathMode, Settings, SettingsSchema};
use studiocraft_plugin::{EncoderKind, Output, OutputInstance, OutputJob, OutputStatus, Plugin, PluginError, RateControl, Registry};

use crate::writer::{Writer, audio_args};

/// Register both outputs and every video encoder.
pub fn register_all(reg: &mut Registry) -> Result<(), PluginError> {
    reg.register_output(Arc::new(Recording))?;
    reg.register_output(Arc::new(Stream))?;
    for e in encoders::all() {
        reg.register_encoder(Arc::new(e))?;
    }
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

/// A running output: a writer and what it writes to.
struct Running {
    writer: Writer,
    target: String,
}

impl OutputInstance for Running {
    fn video(&self) -> Arc<Fifo<Arc<PooledFrame>>> {
        self.writer.video()
    }
    fn audio(&self) -> Arc<Fifo<PooledBlock>> {
        self.writer.audio()
    }
    fn status(&self) -> OutputStatus {
        self.writer.status()
    }
    fn target(&self) -> String {
        self.target.clone()
    }
    fn stop(&mut self) -> Result<(), PluginError> {
        self.writer.stop()
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
        let lossless_audio = (job.rate == RateControl::Lossless).then(|| lossless_audio_codec(container));
        let codec = codec_args(&job, lossless_audio)?;
        let mut out: Vec<String> = vec!["-f".into(), container.into()];
        if container != "matroska" {
            // MP4 and QuickTime files get their index moved to the front when the recording ends.
            out.extend(["-movflags", "+faststart"].map(String::from));
        }
        out.push(path.to_string_lossy().into_owned());
        let writer = Writer::start(&job, &codec, &out, "")?;
        Ok(Box::new(Running { writer, target: path.display().to_string() }))
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
        let codec = codec_args(&job, None)?;
        let out = vec!["-f".into(), "flv".into(), "-flvflags".into(), "no_duration_filesize".into(), url];
        let writer = Writer::start(&job, &codec, &out, settings.str("key").unwrap_or_default().trim())?;
        // The key is a secret: the target shown is the server alone.
        Ok(Box::new(Running { writer, target: server.trim().to_string() }))
    }
}

#[cfg(test)]
mod tests;
