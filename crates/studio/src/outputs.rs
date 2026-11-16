//! Starting and stopping the recording and the stream.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use studiocraft_engine::mixer::MixerCmd;
use studiocraft_media::audio::SAMPLE_RATE;
use studiocraft_media::queue::Fifo;
use studiocraft_media::stats::EncoderStats;
use studiocraft_media::video::{PixelFormat, PooledFrame};
use studiocraft_model::Settings;
use studiocraft_platform::ffmpeg;
use studiocraft_plugin::{OutputInstance, OutputJob, OutputStatus, PluginError, RateControl, VideoFormat};

use crate::{Notice, Studio, StudioError};

/// The two outputs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OutputKind {
    Recording,
    Streaming,
}

impl OutputKind {
    fn noun(self) -> &'static str {
        match self {
            OutputKind::Recording => "recording",
            OutputKind::Streaming => "stream",
        }
    }
}

/// What the interface shows about an output.
#[derive(Clone, Debug, PartialEq)]
pub struct OutputState {
    /// Running (frames go to it).
    pub active: bool,
    /// Finishing its file or stream after a stop.
    pub stopping: bool,
    pub status: OutputStatus,
    /// The file, or the server.
    pub target: String,
    pub elapsed: Duration,
    /// Bytes written so far.
    pub bytes: u64,
}

impl OutputState {
    pub fn status_text(&self) -> String {
        match (&self.status, self.stopping) {
            (_, true) => "stopping".into(),
            (OutputStatus::Starting, _) => "starting".into(),
            (OutputStatus::Running, _) => "running".into(),
            (OutputStatus::Stopped, _) => "stopped".into(),
            (OutputStatus::Failed(e), _) => format!("failed: {e}"),
        }
    }
}

struct Active {
    id: u64,
    instance: Box<dyn OutputInstance>,
    stats: Arc<EncoderStats>,
    started: Instant,
    target: String,
}

struct Stopping {
    kind: OutputKind,
    target: String,
    /// Whether the user was already told the output failed while it ran.
    failed: bool,
    handle: JoinHandle<Result<(), PluginError>>,
}

#[derive(Default)]
pub(crate) struct Outputs {
    active: HashMap<OutputKind, Active>,
    stopping: Vec<Stopping>,
    next_id: u64,
    /// Video encoders each ffmpeg program offers, asked once per program.
    encoders: HashMap<PathBuf, Vec<String>>,
    /// Notices of outputs stopped on the calling thread, for the next [`Outputs::poll`].
    finished: Vec<Option<Notice>>,
}

/// The notice for an output whose stop returned `r`; none for a failure already shown while it
/// ran.
fn finish_notice(kind: OutputKind, target: &str, failed: bool, r: Result<(), PluginError>) -> Option<Notice> {
    match r {
        Ok(()) if kind == OutputKind::Recording => Some(Notice::new("Recording saved to {path}", vec![("path", target.to_string())], false)),
        Ok(()) => Some(Notice::new("The stream ended", Vec::new(), false)),
        Err(e) if failed => {
            log::warn!("{} ended with an error: {e}", kind.noun());
            None
        }
        Err(e) => {
            let template = match kind {
                OutputKind::Recording => "The recording could not be finished: {error}",
                OutputKind::Streaming => "The stream could not be finished: {error}",
            };
            Some(Notice::new(template, vec![("error", e.to_string())], true))
        }
    }
}

/// What changed in [`Outputs::poll`].
#[derive(Default)]
pub(crate) struct Events {
    pub notices: Vec<Notice>,
    pub sinks_changed: bool,
    /// Outputs whose audio queue the mixer should drop.
    pub removed: Vec<u64>,
}

impl Outputs {
    pub(crate) fn any_active(&self) -> bool {
        !self.active.is_empty() || !self.stopping.is_empty()
    }

    pub(crate) fn video_sinks(&self) -> Vec<Arc<Fifo<Arc<PooledFrame>>>> {
        let mut kinds: Vec<_> = self.active.keys().copied().collect();
        kinds.sort_by_key(|k| *k == OutputKind::Streaming);
        kinds.iter().filter_map(|k| self.active.get(k)).map(|a| a.instance.video()).collect()
    }

    pub(crate) fn stats(&self) -> Vec<Arc<EncoderStats>> {
        self.active.values().map(|a| Arc::clone(&a.stats)).collect()
    }

    pub(crate) fn state(&self, kind: OutputKind) -> Option<OutputState> {
        if let Some(a) = self.active.get(&kind) {
            return Some(OutputState {
                active: true,
                stopping: false,
                status: a.instance.status(),
                target: a.target.clone(),
                elapsed: a.started.elapsed(),
                bytes: a.stats.bytes.load(std::sync::atomic::Ordering::Relaxed),
            });
        }
        self.stopping.iter().find(|s| s.kind == kind).map(|s| OutputState {
            active: false,
            stopping: true,
            status: OutputStatus::Running,
            target: s.target.clone(),
            elapsed: Duration::ZERO,
            bytes: 0,
        })
    }

    /// Stop `kind` on a thread of its own (finishing a file can take seconds); the id of its
    /// audio queue.
    fn stop(&mut self, kind: OutputKind, failed: bool) -> Option<u64> {
        let a = self.active.remove(&kind)?;
        let target = a.target.clone();
        // The instance stays reachable here, so it can be stopped on this thread when no thread
        // can be started for it.
        let cell = Arc::new(Mutex::new(Some(a.instance)));
        let for_thread = Arc::clone(&cell);
        let handle = std::thread::Builder::new().name(format!("stop {}", kind.noun())).spawn(move || match for_thread.lock() {
            Ok(mut i) => i.take().map_or(Ok(()), |mut i| i.stop()),
            Err(_) => Err(PluginError::Unavailable("the output was lost".into())),
        });
        match handle {
            Ok(handle) => self.stopping.push(Stopping { kind, target, failed, handle }),
            Err(e) => {
                log::error!("no thread to finish the {}: {e}", kind.noun());
                let r = match cell.lock() {
                    Ok(mut i) => i.take().map_or(Ok(()), |mut i| i.stop()),
                    Err(_) => Err(PluginError::Unavailable("the output was lost".into())),
                };
                self.finished.push(finish_notice(kind, &target, failed, r));
            }
        }
        Some(a.id)
    }

    /// Finished stops and failed outputs.
    pub(crate) fn poll(&mut self) -> Events {
        let mut ev = Events::default();
        let failed: Vec<(OutputKind, String)> = self
            .active
            .iter()
            .filter_map(|(k, a)| match a.instance.status() {
                OutputStatus::Failed(e) => Some((*k, e)),
                _ => None,
            })
            .collect();
        for (kind, e) in failed {
            let template = match kind {
                OutputKind::Recording => "The recording stopped: {error}",
                OutputKind::Streaming => "The stream stopped: {error}",
            };
            ev.notices.push(Notice::new(template, vec![("error", e.to_string())], true));
            if let Some(id) = self.stop(kind, true) {
                ev.removed.push(id);
                ev.sinks_changed = true;
            }
        }
        let (done, running): (Vec<_>, Vec<_>) = self.stopping.drain(..).partition(|s| s.handle.is_finished());
        self.stopping = running;
        ev.notices.extend(self.finished.drain(..).flatten());
        for s in done {
            match s.handle.join() {
                Ok(r) => ev.notices.extend(finish_notice(s.kind, &s.target, s.failed, r)),
                Err(_) => {
                    let template = match s.kind {
                        OutputKind::Recording => "The recording could not be finished",
                        OutputKind::Streaming => "The stream could not be finished",
                    };
                    ev.notices.push(Notice::new(template, Vec::new(), true));
                }
            }
        }
        ev
    }

    /// Finish everything before the app closes.
    pub(crate) fn stop_all_blocking(&mut self) {
        for (_, mut a) in self.active.drain() {
            let _ = a.instance.stop();
        }
        for s in self.stopping.drain(..) {
            let _ = s.handle.join();
        }
    }
}

/// Create an empty file for a new recording in `folder`, named from the local date and time, and
/// return its path. The file is created only under a name that no file has yet, so two recordings
/// started in the same second get different files. The folder is created when it is missing.
pub fn recording_path(folder: &Path, extension: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(folder)?;
    let stamp = chrono::Local::now().format("%Y-%m-%d %H-%M-%S").to_string();
    for n in 1..1000 {
        let path = if n == 1 { folder.join(format!("{stamp}.{extension}")) } else { folder.join(format!("{stamp} ({n}).{extension}")) };
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(_) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, "999 recordings already have this second's name"))
}

/// The folder recordings go to: the one in the settings, else the user's Videos folder.
pub fn recording_folder(configured: &str) -> PathBuf {
    let configured = configured.trim();
    if !configured.is_empty() {
        return PathBuf::from(configured);
    }
    dirs::video_dir().or_else(dirs::home_dir).unwrap_or_else(|| PathBuf::from("."))
}

impl Studio {
    pub(crate) fn toggle_output(&mut self, kind: OutputKind) -> Result<(), StudioError> {
        if self.outputs.active.contains_key(&kind) {
            self.stop_output(kind);
            Ok(())
        } else {
            self.start_output(kind)
        }
    }

    pub(crate) fn stop_output(&mut self, kind: OutputKind) {
        if let Some(id) = self.outputs.stop(kind, false) {
            self.mixer.send(MixerCmd::RemoveOutput { id });
            self.update_encode_target();
        }
    }

    pub(crate) fn start_output(&mut self, kind: OutputKind) -> Result<(), StudioError> {
        if self.outputs.active.contains_key(&kind) {
            return Ok(());
        }
        if self.outputs.stopping.iter().any(|s| s.kind == kind) {
            return Err(StudioError::Output(format!("the last {} is still being finished", kind.noun())));
        }
        if self.video.is_none() {
            return Err(StudioError::Output("nothing can be recorded or streamed without the graphics device".into()));
        }
        if kind == OutputKind::Streaming && self.profile.stream.server.trim().is_empty() {
            return Err(StudioError::Output("set a server in Settings › Stream first".into()));
        }
        let program = ffmpeg::locate(&self.profile.output.ffmpeg_path).map_err(|e| StudioError::Output(e.to_string()))?;
        let p = &self.profile;
        let (output_id, settings, encoder_id, rate, audio_kbps) = match kind {
            OutputKind::Recording => {
                let rate = p.output.quality.quality_level().map_or(RateControl::Lossless, RateControl::Quality);
                ("file", Settings::new(), p.output.video_encoder.clone(), rate, 192)
            }
            OutputKind::Streaming => {
                let settings = Settings::new().with("server", p.stream.server.as_str()).with("key", p.stream.key.as_str());
                let rate = RateControl::Bitrate {
                    kbps: p.stream.video_bitrate_kbps.clamp(100, 200_000),
                    keyframe_s: p.stream.keyframe_interval_s.clamp(1, 20),
                };
                ("rtmp", settings, p.stream.video_encoder.clone(), rate, p.stream.audio_bitrate_kbps)
            }
        };
        let registry = Arc::clone(self.engine.registry());
        let encoder = registry.encoder(&encoder_id)?;
        let offered = self.outputs.encoders.entry(program.clone()).or_insert_with(|| ffmpeg::encoders(&program));
        if !offered.is_empty() && !offered.iter().any(|c| c == encoder.ffmpeg_codec()) {
            return Err(StudioError::Output(format!(
                "this ffmpeg has no {} encoder; choose another encoder in Settings › Output",
                encoder.ffmpeg_codec()
            )));
        }
        let v = self.profile.video.sanitized();
        let stats = Arc::new(EncoderStats::default());
        let job = OutputJob {
            video: VideoFormat { width: v.output_width, height: v.output_height, fps_num: v.fps_num, fps_den: v.fps_den, pixel: PixelFormat::Nv12 },
            sample_rate: SAMPLE_RATE,
            video_encoder: encoder,
            video_encoder_settings: Settings::new(),
            rate,
            audio_bitrate_kbps: audio_kbps,
            ffmpeg: program,
            stats: Arc::clone(&stats),
            start: self.clock.now(),
        };
        let output = registry.output(output_id)?;
        let mut settings = settings;
        let reserved = match kind {
            OutputKind::Recording => {
                let folder = recording_folder(&self.profile.output.folder);
                let path = recording_path(&folder, self.profile.output.format.extension())
                    .map_err(|e| StudioError::Output(format!("no file for the recording could be made in {}: {e}", folder.display())))?;
                settings = settings.with("path", path.to_string_lossy().as_ref());
                Some(path)
            }
            OutputKind::Streaming => None,
        };
        let instance = match output.start(&settings, job) {
            Ok(instance) => instance,
            Err(e) => {
                if let Some(path) = reserved {
                    let _ = std::fs::remove_file(path);
                }
                return Err(e.into());
            }
        };
        self.outputs.next_id += 1;
        let id = self.outputs.next_id;
        self.mixer.send(MixerCmd::AddOutput { id, queue: instance.audio() });
        let target = instance.target();
        self.outputs.active.insert(kind, Active { id, instance, stats, started: Instant::now(), target });
        self.update_encode_target();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_names_do_not_overwrite() {
        let dir = std::env::temp_dir().join(format!("studiocraft-names-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let first = recording_path(&dir, "mkv").unwrap();
        assert!(first.exists(), "the name is taken as soon as it is chosen");
        let second = recording_path(&dir, "mkv").unwrap();
        assert_ne!(first, second);
        assert!(second.exists());
        // Both can come from the same second, or the clock can tick between the two calls.
        let name = second.to_string_lossy();
        assert!(name.ends_with(" (2).mkv") || !name.contains(" ("), "{name}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_stop_that_cannot_finish_the_file_is_reported_once() {
        let mut o = Outputs::default();
        for (kind, failed) in [(OutputKind::Recording, false), (OutputKind::Streaming, true)] {
            let handle = std::thread::spawn(|| Err(PluginError::Unavailable("the muxer stopped".into())));
            o.stopping.push(Stopping { kind, target: String::new(), failed, handle });
        }
        let mut notices = Vec::new();
        while o.any_active() {
            notices.extend(o.poll().notices);
            std::thread::yield_now();
        }
        let texts: Vec<(String, bool)> = notices.iter().map(|n| (n.text(), n.error)).collect();
        assert_eq!(
            texts,
            [("The recording could not be finished: the muxer stopped".to_string(), true)],
            "a stream whose failure was shown while it ran is not reported again"
        );
    }

    #[test]
    fn the_configured_folder_wins() {
        assert_eq!(recording_folder(" D:/takes "), PathBuf::from("D:/takes"));
        assert!(!recording_folder("").as_os_str().is_empty());
    }
}
