//! Starting and stopping the recording, the stream and the replay buffer, and saving replays.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use studiocraft_engine::mixer::MixerCmd;
use studiocraft_media::Tick;
use studiocraft_media::audio::SAMPLE_RATE;
use studiocraft_media::queue::Fifo;
use studiocraft_media::stats::EncoderStats;
use studiocraft_media::video::{PixelFormat, PooledFrame};
use studiocraft_model::{Profile, RecordingQuality, Settings};
use studiocraft_outputs::replay::MAX_SAVES;
use studiocraft_platform::ffmpeg;
use studiocraft_plugin::{
    Availability, Encoder, EncoderEnv, EncoderKind, OutputInstance, OutputJob, OutputStatus, PluginError, RateControl, VideoFormat,
};

use crate::{Notice, Studio, StudioError};

/// The outputs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OutputKind {
    Recording,
    Streaming,
    /// The replay buffer: the newest seconds kept in memory until a save writes them to a file.
    Replay,
}

impl OutputKind {
    fn noun(self) -> &'static str {
        match self {
            OutputKind::Recording => "recording",
            OutputKind::Streaming => "stream",
            OutputKind::Replay => "replay buffer",
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

/// A replay being written to its file.
struct Saving {
    path: PathBuf,
    handle: JoinHandle<Result<(), PluginError>>,
}

#[derive(Default)]
pub(crate) struct Outputs {
    active: HashMap<OutputKind, Active>,
    stopping: Vec<Stopping>,
    saving: Vec<Saving>,
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
        Ok(()) => Some(match kind {
            OutputKind::Recording => Notice::new("Recording saved to {path}", vec![("path", target.to_string())], false),
            OutputKind::Streaming => Notice::new("The stream ended", Vec::new(), false),
            OutputKind::Replay => Notice::new("The replay buffer stopped", Vec::new(), false),
        }),
        Err(e) if failed => {
            log::warn!("{} ended with an error: {e}", kind.noun());
            None
        }
        Err(e) => {
            let template = match kind {
                OutputKind::Recording => "The recording could not be finished: {error}",
                OutputKind::Streaming => "The stream could not be finished: {error}",
                OutputKind::Replay => "The replay buffer could not be stopped: {error}",
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
                OutputKind::Replay => "The replay buffer stopped: {error}",
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
                        OutputKind::Replay => "The replay buffer could not be stopped",
                    };
                    ev.notices.push(Notice::new(template, Vec::new(), true));
                }
            }
        }
        let (saved, writing): (Vec<_>, Vec<_>) = self.saving.drain(..).partition(|s| s.handle.is_finished());
        self.saving = writing;
        for s in saved {
            ev.notices.push(save_notice(&s.path, s.handle.join()));
        }
        ev
    }

    /// Replays still being written.
    pub(crate) fn saving(&self) -> usize {
        self.saving.len()
    }

    /// An error while [`MAX_SAVES`] replays are being written: each one holds its packets in
    /// memory until its file is written.
    fn check_save_room(&self) -> Result<(), StudioError> {
        if self.saving.len() >= MAX_SAVES {
            return Err(StudioError::Output(format!("{MAX_SAVES} replays are already being saved; try again when one is done")));
        }
        Ok(())
    }

    /// Finish everything before the app closes.
    pub(crate) fn stop_all_blocking(&mut self) {
        for (_, mut a) in self.active.drain() {
            let _ = a.instance.stop();
        }
        for s in self.stopping.drain(..) {
            let _ = s.handle.join();
        }
        for s in self.saving.drain(..) {
            let _ = s.handle.join();
        }
    }
}

/// The notice for a replay whose save to `path` ended with `r` (`Err` when its thread panicked).
fn save_notice(path: &Path, r: std::thread::Result<Result<(), PluginError>>) -> Notice {
    match r {
        Ok(Ok(())) => Notice::new("Replay saved to {path}", vec![("path", path.display().to_string())], false),
        Ok(Err(e)) => Notice::new("The replay could not be saved: {error}", vec![("error", e.to_string())], true),
        Err(_) => Notice::new("The replay could not be saved", Vec::new(), true),
    }
}

/// Create an empty file for a new recording in `folder`, named from the local date and time, and
/// return its path. The file is created only under a name that no file has yet, so two recordings
/// started in the same second get different files. The folder is created when it is missing.
pub fn recording_path(folder: &Path, extension: &str) -> std::io::Result<PathBuf> {
    named_path(folder, "", extension)
}

/// [`recording_path`] for a saved replay: the name starts with `Replay `.
pub fn replay_path(folder: &Path) -> std::io::Result<PathBuf> {
    named_path(folder, "Replay ", "mkv")
}

fn named_path(folder: &Path, prefix: &str, extension: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(folder)?;
    let stamp = format!("{prefix}{}", chrono::Local::now().format("%Y-%m-%d %H-%M-%S"));
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

/// What an output is started with.
#[derive(Debug, PartialEq)]
struct Plan {
    output_id: &'static str,
    /// The output plug-in's settings.
    settings: Settings,
    encoder_id: String,
    /// The video encoder's settings, from the profile.
    encoder_settings: Settings,
    rate: RateControl,
    audio_kbps: u32,
}

/// How profile `p` starts the output `kind`.
fn plan(p: &Profile, kind: OutputKind) -> Plan {
    match kind {
        OutputKind::Recording => Plan {
            output_id: "file",
            settings: Settings::new(),
            encoder_id: p.output.video_encoder.clone(),
            encoder_settings: p.output.video_encoder_settings(),
            rate: p.output.quality.quality_level().map_or(RateControl::Lossless, RateControl::Quality),
            audio_kbps: 192,
        },
        OutputKind::Streaming => Plan {
            output_id: "rtmp",
            settings: Settings::new().with("server", p.stream.server.as_str()).with("key", p.stream.key.as_str()),
            encoder_id: p.stream.video_encoder.clone(),
            encoder_settings: p.stream.video_encoder_settings(),
            rate: RateControl::Bitrate {
                kbps: p.stream.video_bitrate_kbps.clamp(100, 200_000),
                keyframe_s: p.stream.keyframe_interval_s.clamp(1, 20),
            },
            audio_kbps: p.stream.audio_bitrate_kbps,
        },
        OutputKind::Replay => {
            let r = p.replay.sanitized();
            Plan {
                output_id: "replay",
                settings: Settings::new().with("seconds", r.seconds).with("memory_mb", r.memory_mb),
                // The replay buffer runs the built-in encoder, with its settings from Settings › Output.
                encoder_id: REPLAY_ENCODER.into(),
                encoder_settings: p.output.encoder_settings.get(REPLAY_ENCODER).cloned().unwrap_or_default(),
                // The recording's quality; the built-in encoder has no lossless mode, so Lossless keeps High.
                rate: RateControl::Quality(p.output.quality.quality_level().or(RecordingQuality::High.quality_level()).unwrap_or(23)),
                audio_kbps: 192,
            }
        }
    }
}

/// The video encoder the replay buffer runs.
const REPLAY_ENCODER: &str = "h264";

impl Plan {
    /// The job that runs `encoder` with this plan's encoder settings, rate and audio bitrate.
    fn job(&self, encoder: Arc<dyn Encoder>, video: VideoFormat, ffmpeg: PathBuf, stats: Arc<EncoderStats>, start: Tick) -> OutputJob {
        OutputJob {
            video,
            sample_rate: SAMPLE_RATE,
            video_encoder: encoder,
            video_encoder_settings: self.encoder_settings.clone(),
            rate: self.rate,
            audio_bitrate_kbps: self.audio_kbps,
            ffmpeg,
            stats,
            start,
        }
    }
}

/// Where the output `kind` can run encoders, when the ffmpeg program listed `offered` (empty when
/// that list could not be read): recordings and the replay buffer also run the built-in ones.
fn encoder_env(offered: &[String], kind: OutputKind) -> EncoderEnv<'_> {
    EncoderEnv { builtin: kind != OutputKind::Streaming, ..EncoderEnv::ffmpeg(offered) }
}

/// Whether the output `kind` can start with `encoder`, when the ffmpeg program listed `offered`
/// (empty when that list could not be read).
fn check_encoder(encoder: &dyn Encoder, offered: &[String], kind: OutputKind) -> Result<(), StudioError> {
    let tab = match kind {
        OutputKind::Recording | OutputKind::Replay => "Output",
        OutputKind::Streaming => "Stream",
    };
    if encoder.kind() != EncoderKind::Video {
        return Err(StudioError::Output(format!("{} is not a video encoder; choose another encoder in Settings › {tab}", encoder.name())));
    }
    match encoder.availability(&encoder_env(offered, kind)) {
        a if a.can_start() => Ok(()),
        Availability::NotRunHere => {
            Err(StudioError::Output(format!("{} cannot run in a {}; choose another encoder in Settings › {tab}", encoder.name(), kind.noun())))
        }
        _ => {
            let codec = encoder.ffmpeg().map_or(encoder.codec().id(), |f| f.ffmpeg_codec());
            Err(StudioError::Output(format!("this ffmpeg has no {codec} encoder; choose another encoder in Settings › {tab}")))
        }
    }
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
        let planned = plan(&self.profile, kind);
        let registry = Arc::clone(self.engine.registry());
        let encoder = registry.encoder(&planned.encoder_id)?;
        let program = if encoder.ffmpeg().is_some() {
            let program = ffmpeg::locate(&self.profile.output.ffmpeg_path).map_err(|e| StudioError::Output(e.to_string()))?;
            let offered = self.outputs.encoders.entry(program.clone()).or_insert_with(|| ffmpeg::encoders(&program));
            check_encoder(encoder.as_ref(), offered, kind)?;
            program
        } else {
            // A built-in encoder runs without the ffmpeg program.
            check_encoder(encoder.as_ref(), &[], kind)?;
            PathBuf::new()
        };
        let v = self.profile.video.sanitized();
        let stats = Arc::new(EncoderStats::default());
        let video = VideoFormat { width: v.output_width, height: v.output_height, fps_num: v.fps_num, fps_den: v.fps_den, pixel: PixelFormat::Nv12 };
        let job = planned.job(encoder, video, program, Arc::clone(&stats), self.clock.now());
        let output = registry.output(planned.output_id)?;
        let mut settings = planned.settings;
        let reserved = match kind {
            OutputKind::Recording => {
                let folder = recording_folder(&self.profile.output.folder);
                let path = recording_path(&folder, self.profile.output.format.extension())
                    .map_err(|e| StudioError::Output(format!("no file for the recording could be made in {}: {e}", folder.display())))?;
                settings = settings.with("path", path.to_string_lossy().as_ref());
                Some(path)
            }
            OutputKind::Streaming | OutputKind::Replay => None,
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

    /// Write what the replay buffer holds to a new file in the recording folder, on a thread of
    /// its own; the buffer keeps running. [`Studio::tick`] reports the result as a notice.
    pub(crate) fn save_replay(&mut self) -> Result<(), StudioError> {
        let Some(active) = self.outputs.active.get(&OutputKind::Replay) else {
            return Err(StudioError::Output("the replay buffer is not running; start it first".into()));
        };
        self.outputs.check_save_room()?;
        let folder = recording_folder(&self.profile.output.folder);
        let path =
            replay_path(&folder).map_err(|e| StudioError::Output(format!("no file for the replay could be made in {}: {e}", folder.display())))?;
        let clip = match active.instance.replay() {
            Ok(clip) => clip,
            Err(e) => {
                let _ = std::fs::remove_file(&path);
                return Err(e.into());
            }
        };
        let for_thread = path.clone();
        let handle = std::thread::Builder::new().name("save replay".into()).spawn(move || {
            let r = clip.write(&for_thread);
            if r.is_err() {
                // The empty file reserved for the name, when the write did not get to it.
                let _ = std::fs::remove_file(&for_thread);
            }
            r
        });
        match handle {
            Ok(handle) => {
                self.outputs.saving.push(Saving { path, handle });
                Ok(())
            }
            Err(e) => {
                let _ = std::fs::remove_file(&path);
                Err(StudioError::Output(format!("no thread to save the replay: {e}")))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use studiocraft_plugin::Plugin;

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
    fn the_profile_s_encoder_settings_reach_the_encoder() {
        let mut p = Profile::default();
        p.output.encoder_settings.insert("x264".into(), Settings::new().with("preset", "fast"));
        p.stream.video_encoder = "nvenc_h264".into();
        p.stream.encoder_settings.insert("nvenc_h264".into(), Settings::new().with("preset", "p3"));
        p.stream.encoder_settings.insert("x264".into(), Settings::new().with("preset", "medium"));

        let rec = plan(&p, OutputKind::Recording);
        assert_eq!((rec.output_id, rec.encoder_id.as_str()), ("file", "x264"));
        assert_eq!(rec.encoder_settings, Settings::new().with("preset", "fast"));
        let stream = plan(&p, OutputKind::Streaming);
        assert_eq!(stream.encoder_settings, Settings::new().with("preset", "p3"), "the stream's own encoder's settings");

        let video = VideoFormat { width: 64, height: 48, fps_num: 30, fps_den: 1, pixel: PixelFormat::Nv12 };
        for (plan, preset) in [(rec, "-preset fast"), (stream, "-preset p3")] {
            let Some(encoder) = studiocraft_outputs::encoders::all().into_iter().find(|e| e.id() == plan.encoder_id) else {
                panic!("no encoder {}", plan.encoder_id)
            };
            let stats = Arc::new(EncoderStats::default());
            let job = plan.job(Arc::new(encoder), video, PathBuf::from("ffmpeg"), stats, Tick::from_units(0, 1));
            let Some(ffmpeg) = job.video_encoder.ffmpeg() else { panic!("{} runs in ffmpeg", plan.encoder_id) };
            let args = ffmpeg.ffmpeg_args(&job.video_encoder_settings, job.rate, 30.0).unwrap().join(" ");
            assert!(args.contains(preset), "{args}");
        }
    }

    #[test]
    fn an_encoder_that_cannot_run_names_the_settings_tab_of_its_output() {
        let x264 = studiocraft_outputs::encoders::all().into_iter().next().unwrap();
        let other = ["libx265".to_string()];
        let listed = ["libx264".to_string()];
        assert!(check_encoder(&x264, &listed, OutputKind::Streaming).is_ok());
        assert!(check_encoder(&x264, &[], OutputKind::Recording).is_ok(), "an unknown list does not rule it out");
        for (kind, tab) in [(OutputKind::Recording, "Settings › Output"), (OutputKind::Streaming, "Settings › Stream")] {
            let Err(StudioError::Output(m)) = check_encoder(&x264, &other, kind) else { panic!("x264 started without libx264") };
            assert!(m.contains("no libx264 encoder") && m.ends_with(tab), "{m}");
        }
    }

    #[test]
    fn a_built_in_encoder_records_and_does_not_stream() {
        struct Builtin(studiocraft_plugin::Codec);
        impl Plugin for Builtin {
            fn id(&self) -> &'static str {
                "builtin"
            }
            fn name(&self) -> &'static str {
                "Built-in"
            }
            fn schema(&self) -> studiocraft_model::SettingsSchema {
                studiocraft_model::SettingsSchema::default()
            }
        }
        impl Encoder for Builtin {
            fn codec(&self) -> studiocraft_plugin::Codec {
                self.0
            }
        }
        let video = Builtin(studiocraft_plugin::Codec::H264);
        let listed = ["libx264".to_string()];
        for offered in [&listed[..], &[]] {
            let Err(StudioError::Output(m)) = check_encoder(&video, offered, OutputKind::Streaming) else { panic!("a built-in encoder started") };
            assert_eq!(m, "Built-in cannot run in a stream; choose another encoder in Settings › Stream");
            assert!(check_encoder(&video, offered, OutputKind::Recording).is_ok(), "recordings run built-in encoders");
        }
        let mut registry = studiocraft_plugin::Registry::new();
        studiocraft_outputs::register_all(&mut registry).unwrap();
        let h264 = registry.encoder("h264").unwrap();
        assert!(check_encoder(h264.as_ref(), &[], OutputKind::Recording).is_ok());
        assert!(check_encoder(h264.as_ref(), &[], OutputKind::Streaming).is_err());
        let audio = Builtin(studiocraft_plugin::Codec::Aac);
        let Err(StudioError::Output(m)) = check_encoder(&audio, &listed, OutputKind::Recording) else { panic!("an audio encoder started as video") };
        assert_eq!(m, "Built-in is not a video encoder; choose another encoder in Settings › Output");
    }

    #[test]
    fn the_replay_buffer_runs_the_built_in_encoder_with_the_profile_s_settings() {
        let mut p = Profile { replay: studiocraft_model::ReplaySettings { seconds: 90, memory_mb: 2048 }, ..Profile::default() };
        p.output.video_encoder = "x264".into();
        p.output.encoder_settings.insert("h264".into(), Settings::new().with("keyframe_s", 1));
        p.output.quality = RecordingQuality::Compact;
        let r = plan(&p, OutputKind::Replay);
        assert_eq!((r.output_id, r.encoder_id.as_str(), r.audio_kbps), ("replay", "h264", 192));
        assert_eq!(r.settings, Settings::new().with("seconds", 90).with("memory_mb", 2048));
        assert_eq!(r.encoder_settings, Settings::new().with("keyframe_s", 1));
        assert_eq!(r.rate, RateControl::Quality(RecordingQuality::Compact.quality_level().unwrap()));
        p.output.quality = RecordingQuality::Lossless;
        assert_eq!(plan(&p, OutputKind::Replay).rate, RateControl::Quality(RecordingQuality::High.quality_level().unwrap()));
        let mut registry = studiocraft_plugin::Registry::new();
        studiocraft_outputs::register_all(&mut registry).unwrap();
        let h264 = registry.encoder("h264").unwrap();
        assert!(check_encoder(h264.as_ref(), &[], OutputKind::Replay).is_ok());
        assert!(registry.output("replay").is_ok());
    }

    #[test]
    fn saved_replays_are_reported_when_their_file_is_written() {
        let dir = std::env::temp_dir().join(format!("studiocraft-replays-{}", std::process::id()));
        let path = replay_path(&dir).unwrap();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("Replay ") && name.ends_with(".mkv"), "{name}");
        assert_ne!(replay_path(&dir).unwrap(), path, "two saves in the same second get two names");
        let mut o = Outputs::default();
        o.saving.push(Saving { path: path.clone(), handle: std::thread::spawn(|| Ok(())) });
        o.saving.push(Saving { path: dir.join("b.mkv"), handle: std::thread::spawn(|| Err(PluginError::Failed("no space left".into()))) });
        let mut notices = Vec::new();
        while o.saving() > 0 {
            notices.extend(o.poll().notices);
            std::thread::yield_now();
        }
        let mut texts: Vec<(String, bool)> = notices.iter().map(|n| (n.text(), n.error)).collect();
        texts.sort();
        assert_eq!(
            texts,
            [(format!("Replay saved to {}", path.display()), false), ("The replay could not be saved: no space left".to_string(), true)]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn replays_being_written_are_limited() {
        let mut o = Outputs::default();
        let (go, wait) = std::sync::mpsc::channel::<()>();
        let wait = Arc::new(Mutex::new(wait));
        for n in 0..MAX_SAVES {
            assert!(o.check_save_room().is_ok(), "save {n}");
            let wait = Arc::clone(&wait);
            let handle = std::thread::spawn(move || {
                let _ = wait.lock().map(|w| w.recv());
                Ok(())
            });
            o.saving.push(Saving { path: PathBuf::from(format!("{n}.mkv")), handle });
        }
        let Err(StudioError::Output(m)) = o.check_save_room() else { panic!("a ninth replay was written at once") };
        assert_eq!(m, "8 replays are already being saved; try again when one is done");
        // One write finishes, and its place is free again once the studio has reported it.
        go.send(()).unwrap();
        while o.saving() == MAX_SAVES {
            let _ = o.poll();
            std::thread::yield_now();
        }
        assert!(o.check_save_room().is_ok());
        for _ in 1..MAX_SAVES {
            go.send(()).unwrap();
        }
        o.stop_all_blocking();
        assert_eq!(o.saving(), 0);
    }

    #[test]
    fn the_configured_folder_wins() {
        assert_eq!(recording_folder(" D:/takes "), PathBuf::from("D:/takes"));
        assert!(!recording_folder("").as_os_str().is_empty());
    }
}
