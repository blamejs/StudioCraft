//! Recording, streaming and the replay buffer.
//!
//! With an ffmpeg encoder, a recording runs an ffmpeg process ([`writer`]) that writes a Matroska,
//! MP4 or QuickTime file. With the built-in encoders, a recording writes a Matroska file from this
//! process ([`native`]). A stream runs the built-in encoders and sends FLV over RTMP or RTMPS from
//! this process to each of its destinations ([`stream`]). The video encoders
//! ffmpeg offers are plug-ins ([`encoders`]), and so are the built-in ones: H.264
//! ([`studiocraft_encode::H264`]), NVENC H.264, HEVC and AV1 ([`studiocraft_encode::Nvenc`]), the
//! Media Foundation hardware H.264, HEVC and AV1 encoders ([`studiocraft_encode::MediaFoundation`]),
//! the VideoToolbox hardware H.264 and HEVC encoders ([`studiocraft_encode::VideoToolbox`]) and
//! the VA-API hardware H.264, HEVC and AV1 encoders ([`studiocraft_encode::VaApi`]). The
//! hardware choice ([`encoders::Hardware`]) is resolved by the studio to one of the ffmpeg encoders,
//! or to a built-in hardware encoder for an MKV recording, a stream or the replay buffer, before an
//! output starts. The
//! replay buffer ([`replay`]) runs the built-in encoders and keeps their newest packets in memory
//! until a save writes them to a Matroska file.

pub mod encoders;
pub mod native;
pub mod replay;
pub mod stream;
pub mod writer;

use std::path::Path;
use std::sync::Arc;

use studiocraft_media::audio::PooledBlock;
use studiocraft_media::queue::Fifo;
use studiocraft_media::video::PooledFrame;
use studiocraft_model::{Field, FieldKind, PathMode, ReconnectSettings, Settings, SettingsSchema};
use studiocraft_plugin::{EncoderKind, Output, OutputInstance, OutputJob, OutputStatus, Plugin, PluginError, RateControl, Registry};

use crate::native::Native;
pub use crate::stream::{AutoCheck, Streaming, Target};
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
    reg.register_encoder(Arc::new(studiocraft_encode::Nvenc::AV1))?;
    reg.register_encoder(Arc::new(studiocraft_encode::MediaFoundation::H264))?;
    reg.register_encoder(Arc::new(studiocraft_encode::MediaFoundation::HEVC))?;
    reg.register_encoder(Arc::new(studiocraft_encode::MediaFoundation::AV1))?;
    reg.register_encoder(Arc::new(studiocraft_encode::VideoToolbox::H264))?;
    reg.register_encoder(Arc::new(studiocraft_encode::VideoToolbox::HEVC))?;
    reg.register_encoder(Arc::new(studiocraft_encode::VaApi::H264))?;
    reg.register_encoder(Arc::new(studiocraft_encode::VaApi::HEVC))?;
    reg.register_encoder(Arc::new(studiocraft_encode::VaApi::AV1))?;
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

/// A live stream to one or more RTMP or RTMPS servers, through the built-in encoders ([`stream`]).
///
/// The settings hold `destinations`, a list of `{id, server, key, enabled}` objects. Without that
/// list, `server` and `key` give one destination with the id `main`.
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

/// The destinations in a stream's settings.
pub fn stream_targets(settings: &Settings) -> Result<Vec<Target>, PluginError> {
    let Some(list) = settings.get("destinations") else {
        let server = settings.str("server").unwrap_or_default();
        return Ok(vec![Target { id: "main".into(), server: server.into(), key: settings.str("key").unwrap_or_default().into(), enabled: true }]);
    };
    let Some(list) = list.as_array() else {
        return Err(PluginError::Failed("the stream's destinations are not a list".into()));
    };
    let most = craft_packet::MAX_SUBSCRIBERS;
    if list.len() > most {
        return Err(PluginError::Failed(format!("a stream can have at most {most} destinations, not {}", list.len())));
    }
    list.iter()
        .enumerate()
        .map(|(i, d)| {
            let text = |key: &str| -> Result<String, PluginError> {
                match d.get(key) {
                    None | Some(serde_json::Value::Null) => Ok(String::new()),
                    Some(serde_json::Value::String(s)) => Ok(s.clone()),
                    Some(_) => Err(PluginError::Failed(format!("destination {} has a {key} that is not text", i + 1))),
                }
            };
            let enabled = match d.get("enabled") {
                None => true,
                Some(v) => {
                    v.as_bool().ok_or_else(|| PluginError::Failed(format!("destination {} has an enabled flag that is not true or false", i + 1)))?
                }
            };
            if !d.is_object() {
                return Err(PluginError::Failed(format!("destination {} is not an object", i + 1)));
            }
            Ok(Target { id: text("id")?, server: text("server")?, key: text("key")?, enabled })
        })
        .collect()
}

/// The reconnect settings in a stream's settings: the `reconnect` object, each value optional and
/// defaulting to [`ReconnectSettings::default`]'s. A value of the wrong type, or outside its range
/// ([`ReconnectSettings::check`]), is an error.
pub fn stream_reconnect(settings: &Settings) -> Result<ReconnectSettings, PluginError> {
    let mut r = ReconnectSettings::default();
    let Some(v) = settings.get("reconnect") else { return Ok(r) };
    let Some(obj) = v.as_object() else {
        return Err(PluginError::Failed("the stream's reconnect settings are not an object".into()));
    };
    if let Some(v) = obj.get("enabled") {
        r.enabled = v.as_bool().ok_or_else(|| PluginError::Failed("the reconnect setting enabled is not true or false".into()))?;
    }
    for (key, field) in [("first_delay_s", &mut r.first_delay_s), ("max_delay_s", &mut r.max_delay_s), ("max_tries", &mut r.max_tries)] {
        if let Some(v) = obj.get(key) {
            *field = v
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| PluginError::Failed(format!("the reconnect setting {key} is not a whole number from 0 to {}", u32::MAX)))?;
        }
    }
    r.check().map_err(PluginError::Failed)?;
    Ok(r)
}

/// What the automatic connection checks of a stream need from its settings: the `profile` key its
/// checks are kept under (empty when absent), each destination's `service` (an unknown or empty one is no service), and
/// the top-level `check_service` flag, off when absent. A value of the wrong type is an error.
pub fn stream_checks(settings: &Settings) -> Result<AutoCheck, PluginError> {
    let service_hosts = match settings.get("check_service") {
        None => false,
        Some(v) => v.as_bool().ok_or_else(|| PluginError::Failed("the stream's check_service setting is not true or false".into()))?,
    };
    let profile = match settings.get("profile") {
        None => String::new(),
        Some(v) => v.as_str().map(str::to_string).ok_or_else(|| PluginError::Failed("the stream's profile setting is not text".into()))?,
    };
    let mut services = Vec::new();
    if let Some(list) = settings.get("destinations").and_then(|v| v.as_array()) {
        for (i, d) in list.iter().enumerate() {
            let service = match d.get("service") {
                None => continue,
                Some(serde_json::Value::String(s)) => s,
                Some(_) => return Err(PluginError::Failed(format!("destination {} has a service that is not text", i + 1))),
            };
            let id = d.get("id").and_then(|v| v.as_str()).unwrap_or_default();
            if let Some(s) = studiocraft_model::services::service(service) {
                services.push((id.to_string(), s));
            }
        }
    }
    Ok(AutoCheck { profile, services, service_hosts })
}

impl Output for Stream {
    fn start(&self, settings: &Settings, job: OutputJob) -> Result<Box<dyn OutputInstance>, PluginError> {
        let targets = stream_targets(settings)?;
        let reconnect = stream_reconnect(settings)?;
        let checks = stream_checks(settings)?;
        Ok(Box::new(Streaming::start(&job, targets, reconnect, checks)?))
    }
}

#[cfg(test)]
mod tests;
