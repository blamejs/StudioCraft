//! Starting and stopping the recording, the stream and the replay buffer, and saving replays.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use craft_hwenc::{
    Cancel, Candidate, Choice, Error as HwError, FfmpegProber, Input, Os, Outcome, PROBE_HEIGHT, PROBE_WIDTH, Preference, ProbeCache, Prober, Target,
    Vendor,
};
use studiocraft_encode::{Nvenc, SessionError};
use studiocraft_engine::mixer::MixerCmd;
use studiocraft_media::Tick;
use studiocraft_media::audio::SAMPLE_RATE;
use studiocraft_media::queue::Fifo;
use studiocraft_media::stats::EncoderStats;
use studiocraft_media::video::{PixelFormat, PooledFrame};
use studiocraft_model::{Profile, RecordingFormat, RecordingQuality, Settings};
use studiocraft_outputs::encoders::HARDWARE;
use studiocraft_outputs::replay::MAX_SAVES;
use studiocraft_platform::ffmpeg;
use studiocraft_plugin::{
    Availability, Encoder, EncoderEnv, EncoderKind, OutputInstance, OutputJob, OutputStatus, Plugin, PluginError, RateControl, VideoFormat,
    VideoParams, VideoSession,
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
    /// The hardware encoder it runs, chosen by the `hardware` choice or by name.
    hardware: Option<HardwareRun>,
}

/// A hardware encoder an output runs, chosen through the `hardware` encoder or by name.
#[derive(Clone, Debug, PartialEq)]
struct HardwareRun {
    candidate: &'static Candidate,
    /// The ffmpeg program it runs in.
    program: PathBuf,
    /// Whether the automatic choice picked it.
    auto: bool,
}

/// The longest one test encode of a hardware encoder may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// The notice when the hardware encoder tests could not run at all.
const TESTS_FAILED: &str = "The hardware encoder tests could not run: {error}";

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
    /// The hardware encoders' test results for this session.
    probes: Arc<ProbeCache>,
    /// Held by a running test, so tests for two outputs do not hold an encoder at once.
    probing: Arc<Mutex<()>>,
    /// How often the graphics device changed; a test for an earlier device is not used.
    device_changes: u64,
    /// Outputs waiting for their hardware encoder tests, which run on their own thread.
    testing: HashMap<OutputKind, Testing>,
    /// Canceled tests whose threads may still run; the next tests wait for them, so two
    /// ffmpeg tests do not hold an encoder at once.
    canceled: Vec<JoinHandle<Result<Resolved, StudioError>>>,
    /// Hardware choices whose tests finished, for the start that follows.
    resolved: HashMap<OutputKind, Resolved>,
}

/// What [`resolve_hardware`] gives: the encoder id, the hardware encoder when it is one, and a
/// notice.
type Resolved = (String, Option<HardwareRun>, Option<Notice>);

/// Hardware encoder tests running for an output that is about to start.
struct Testing {
    cancel: Cancel,
    handle: JoinHandle<Result<Resolved, StudioError>>,
    started: Instant,
    /// The profile the tests ran for; a result for another profile is not used.
    profile: Profile,
    /// [`Outputs::device_changes`] when the tests started.
    device: u64,
    /// Whether another output held a hardware encoder when the tests started.
    beside_encoder: bool,
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
        if let Some(t) = self.testing.get(&kind) {
            // Waiting for the hardware encoder tests: the next toggle cancels the start.
            return Some(OutputState {
                active: false,
                stopping: false,
                status: OutputStatus::Starting,
                target: String::new(),
                elapsed: t.started.elapsed(),
                bytes: 0,
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
        let failed: Vec<(OutputKind, String, Option<HardwareRun>)> = self
            .active
            .iter()
            .filter_map(|(k, a)| match a.instance.status() {
                OutputStatus::Failed(e) => Some((*k, e, a.hardware.clone())),
                _ => None,
            })
            .collect();
        for (kind, e, hardware) in failed {
            let template = match kind {
                OutputKind::Recording => "The recording stopped: {error}",
                OutputKind::Streaming => "The stream stopped: {error}",
                OutputKind::Replay => "The replay buffer stopped: {error}",
            };
            ev.notices.push(Notice::new(template, vec![("error", e.to_string())], true));
            if let Some(hw) = hardware.filter(|hw| encoder_failed(hw.candidate, &e)) {
                self.probes.mark_failed(&hw.program, hw.candidate, e.clone());
                if hw.auto {
                    let args = vec![("encoder", hw.candidate.name.to_string())];
                    ev.notices.push(Notice::new("{encoder} stopped working; the next start tries another encoder", args, true));
                }
            }
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

    /// Forget the hardware encoders' test results after the graphics device changed; the next
    /// start tests them again.
    pub(crate) fn forget_hardware_tests(&mut self) {
        // Tests still running for the old device write to the old cache, and their outputs
        // are tested again.
        self.probes = Arc::default();
        self.device_changes += 1;
    }

    /// Finish everything before the app closes.
    pub(crate) fn stop_all_blocking(&mut self) {
        // A canceled test stops its ffmpeg at once.
        for t in self.testing.values() {
            t.cancel.cancel();
        }
        for (_, t) in self.testing.drain() {
            let _ = t.handle.join();
        }
        for h in self.canceled.drain(..) {
            let _ = h.join();
        }
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

/// Whether an output's failure `error` came from the hardware encoder `candidate`: ffmpeg names the
/// encoder in its messages, or the error is one a missing device gives. A full session is not the
/// encoder's fault: it can have a free session at the next start.
fn encoder_failed(candidate: &Candidate, error: &str) -> bool {
    match craft_hwenc::classify(error) {
        craft_hwenc::Outcome::Busy(_) => false,
        craft_hwenc::Outcome::NoDevice(_) => true,
        // A built-in encoder's messages start with its name, which contains the candidate's.
        _ => error.contains(candidate.ffmpeg_codec) || error.contains(candidate.name),
    }
}

/// Tests the hardware encoders for an MKV recording: an encoder that also runs in this process
/// (NVENC H.264 and HEVC) is tested by opening it there and encoding one frame, and the others
/// are tested in the ffmpeg program.
struct BuiltInFirst {
    ffmpeg: FfmpegProber,
    fps: (u32, u32),
    rate: RateControl,
}

impl BuiltInFirst {
    fn test(&self, encoder: Nvenc) -> Outcome {
        let params = VideoParams { width: PROBE_WIDTH, height: PROBE_HEIGHT, fps_num: self.fps.0, fps_den: self.fps.1, rate: self.rate };
        let mut session = match encoder.try_session(&params) {
            Ok(s) => s,
            // Not kept in the test cache: the next start tests again.
            Err(SessionError::Busy(why)) => return Outcome::Busy(why),
            Err(SessionError::Other(PluginError::Unavailable(why))) => return Outcome::NoDevice(why),
            Err(SessionError::Other(e)) => return Outcome::Failed(e.to_string()),
        };
        let gray = [vec![16u8; (PROBE_WIDTH * PROBE_HEIGHT) as usize], vec![128u8; (PROBE_WIDTH * PROBE_HEIGHT / 2) as usize]].concat();
        let mut packets = Vec::new();
        if let Err(e) = session.encode(&gray, 0, &mut packets) {
            return Outcome::Failed(e.to_string());
        }
        if let Err(e) = session.flush(&mut packets) {
            return Outcome::Failed(e.to_string());
        }
        if packets.is_empty() { Outcome::Failed(format!("{} returned no packet for its test frame", encoder.name())) } else { Outcome::Works }
    }
}

impl Prober for BuiltInFirst {
    fn program(&self) -> &Path {
        self.ffmpeg.program()
    }
    fn probe(&self, candidate: &'static Candidate, input: Input, cancel: &Cancel) -> Outcome {
        match Nvenc::for_candidate(candidate.id).filter(|_| input == Input::Nv12) {
            Some(_) if cancel.is_canceled() => Outcome::Canceled,
            Some(encoder) => self.test(encoder),
            // Without ffmpeg the encoders that run in this process are still tested.
            None => match self.ffmpeg.probe(candidate, input, cancel) {
                Outcome::FfmpegMissing(why) => Outcome::NoDevice(why),
                o => o,
            },
        }
    }
    fn args(&self, candidate: &'static Candidate, input: Input) -> Vec<String> {
        match Nvenc::for_candidate(candidate.id).filter(|_| input == Input::Nv12) {
            // Kept apart from the ffmpeg encoder's results in the cache.
            Some(encoder) => vec![encoder.id().to_string(), format!("{:?}", self.rate), format!("{}/{}", self.fps.0, self.fps.1)],
            None => self.ffmpeg.args(candidate, input),
        }
    }
}

/// Whether the `hardware` choice of output `kind` runs NVENC in this process: an MKV recording
/// that is not lossless, since the built-in encoders have no lossless mode.
fn runs_built_in(p: &Profile, kind: OutputKind, rate: RateControl) -> bool {
    kind == OutputKind::Recording && p.output.format == RecordingFormat::Mkv && rate != RateControl::Lossless
}

/// `resolved` with a hardware encoder that also runs in this process replaced by that built-in
/// encoder.
fn built_in_twin(resolved: Resolved) -> Resolved {
    match resolved {
        (id, Some(run), notice) => match Nvenc::for_candidate(&id) {
            Some(encoder) => (encoder.id().to_string(), Some(run), notice),
            None => (id, Some(run), notice),
        },
        other => other,
    }
}

/// What the `hardware` encoder runs: the encoder id, the hardware encoder when it is one, and a
/// notice when the automatic choice fell back to software. `preference` is the profile's
/// hardware encoder setting and `vendor` the graphics adapter's PCI vendor id. A stream takes
/// H.264 only; a recording takes the named encoder's codec, and the automatic choice uses H.264.
/// A `lossless` recording only uses an encoder that can run losslessly.
fn resolve_hardware(
    preference: &str,
    vendor: u32,
    stream: bool,
    lossless: bool,
    prober: &dyn Prober,
    cache: &ProbeCache,
    cancel: &Cancel,
) -> Result<(String, Option<HardwareRun>, Option<Notice>), StudioError> {
    let preference = Preference::parse(preference);
    let auto = preference == Preference::Auto;
    let codec = match &preference {
        Preference::Named(id) if !stream => craft_hwenc::candidate(id).map_or(craft_hwenc::Codec::H264, |c| c.codec),
        _ => craft_hwenc::Codec::H264,
    };
    let target = Target { vendor: Vendor::from_pci_id(vendor), os: Os::current(), codec, input: Input::Nv12, lossless };
    match craft_hwenc::resolve(&preference, target, prober, cache, cancel) {
        Ok(Choice::Hardware(c)) => Ok((c.id.to_string(), Some(HardwareRun { candidate: c, program: prober.program().to_path_buf(), auto }), None)),
        Ok(Choice::Software { tried }) => {
            for (c, outcome) in &tried {
                log::info!("{} was not used: {outcome}", c.name);
            }
            let notice = (auto && !tried.is_empty()).then(|| {
                Notice::new("No hardware encoder passed its test; using {encoder}", vec![("encoder", "Software (x264)".to_string())], false)
            });
            Ok((SOFTWARE_ENCODER.to_string(), None, notice))
        }
        Err(HwError::Canceled) => Err(StudioError::Output("the hardware encoder tests were canceled".into())),
        Err(e) => Err(StudioError::Output(format!("{e}; choose another hardware encoder in Settings › Video"))),
    }
}

/// The encoder the `hardware` choice falls back to.
const SOFTWARE_ENCODER: &str = "x264";

/// The saved settings of encoder `id` for the output `kind`.
fn encoder_settings(p: &Profile, kind: OutputKind, id: &str) -> Settings {
    let saved = if kind == OutputKind::Streaming { &p.stream.encoder_settings } else { &p.output.encoder_settings };
    saved.get(id).cloned().unwrap_or_default()
}

impl Studio {
    pub(crate) fn toggle_output(&mut self, kind: OutputKind) -> Result<(), StudioError> {
        if self.outputs.active.contains_key(&kind) || self.outputs.testing.contains_key(&kind) {
            self.stop_output(kind);
            Ok(())
        } else {
            self.start_output(kind)
        }
    }

    /// Whether `kind` waits for its hardware encoder tests.
    #[cfg(test)]
    pub(crate) fn testing_hardware(&self, kind: OutputKind) -> bool {
        self.outputs.testing.contains_key(&kind)
    }

    /// Start the hardware encoder tests for `kind` on their own thread, each test with the
    /// arguments the output will run the encoder with.
    /// Test the hardware encoders for `kind`'s start: the `hardware` choice's, or `named` alone.
    fn test_hardware(&mut self, kind: OutputKind, planned: &Plan, program: PathBuf, offered: Vec<String>, named: Option<&'static str>) {
        let v = self.profile.video.sanitized();
        let fps = f64::from(v.fps_num) / f64::from(v.fps_den.max(1));
        let registry = Arc::clone(self.engine.registry());
        let args: HashMap<&'static str, Vec<String>> = craft_hwenc::CANDIDATES
            .iter()
            .filter_map(|c| {
                let encoder = registry.encoder(c.id).ok()?;
                let a = encoder.ffmpeg()?.ffmpeg_args(&encoder_settings(&self.profile, kind, c.id), planned.rate, fps).ok()?;
                Some((c.id, a))
            })
            .collect();
        let ffmpeg = FfmpegProber::new(program, offered).with_timeout(PROBE_TIMEOUT).with_encode_args(Box::new(move |c, _| args.get(c.id).cloned()));
        let vendor = self.gpu.as_ref().and_then(|g| g.adapter()).map_or(0, |a| a.vendor);
        let stream = kind == OutputKind::Streaming;
        let lossless = planned.rate == RateControl::Lossless;
        let built_in = runs_built_in(&self.profile, kind, planned.rate);
        let prober: Box<dyn Prober + Send> =
            if built_in { Box::new(BuiltInFirst { ffmpeg, fps: (v.fps_num, v.fps_den.max(1)), rate: planned.rate }) } else { Box::new(ffmpeg) };
        let preference = named.map_or_else(|| self.profile.output.hardware_encoder.clone(), String::from);
        // A test passed before another output took an encoder says nothing about a second session.
        let encoding = self.outputs.active.values().any(|a| a.hardware.is_some());
        let cache = if encoding { Arc::new(self.outputs.probes.without_outcomes()) } else { Arc::clone(&self.outputs.probes) };
        let cancel = Cancel::new();
        let c = cancel.clone();
        let earlier = std::mem::take(&mut self.outputs.canceled);
        let probing = Arc::clone(&self.outputs.probing);
        let handle = std::thread::Builder::new().name("hardware-encoder-tests".into()).spawn(move || {
            for h in earlier {
                let _ = h.join();
            }
            let _one_at_a_time = probing.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let resolved = resolve_hardware(&preference, vendor, stream, lossless, prober.as_ref(), &cache, &c);
            if built_in { resolved.map(built_in_twin) } else { resolved }
        });
        match handle {
            Ok(handle) => {
                let device = self.outputs.device_changes;
                let t = Testing { cancel, handle, started: Instant::now(), profile: self.profile.clone(), device, beside_encoder: encoding };
                self.outputs.testing.insert(kind, t);
            }
            Err(e) => self.notices.push(Notice::new(TESTS_FAILED, vec![("error", e.to_string())], true)),
        }
    }

    /// Start each output whose hardware encoder tests ended, or show why it could not start. A
    /// test thread that panicked gives [`TESTS_FAILED`].
    pub(crate) fn finish_hardware_tests(&mut self) {
        self.outputs.canceled.retain(|h| !h.is_finished());
        let done: Vec<OutputKind> = self.outputs.testing.iter().filter(|(_, t)| t.handle.is_finished()).map(|(k, _)| *k).collect();
        for kind in done {
            let Some(t) = self.outputs.testing.remove(&kind) else { continue };
            let canceled = t.cancel.is_canceled();
            let encoding = self.outputs.active.values().any(|a| a.hardware.is_some());
            if !canceled && (t.profile != self.profile || t.device != self.outputs.device_changes || (encoding && !t.beside_encoder)) {
                // The settings or the graphics device changed while the tests ran, or another
                // output took an encoder since: test again.
                if let Err(e) = self.start_output(kind) {
                    self.notices.push(Notice::plain(e.to_string(), true));
                }
                continue;
            }
            match t.handle.join() {
                Ok(Ok(resolved)) if !canceled => {
                    self.outputs.resolved.insert(kind, resolved);
                    if let Err(e) = self.start_output(kind) {
                        self.outputs.resolved.remove(&kind);
                        self.notices.push(Notice::plain(e.to_string(), true));
                    }
                }
                Ok(Ok(_)) => {}
                Ok(Err(e)) => {
                    if !canceled {
                        self.notices.push(Notice::plain(e.to_string(), true));
                    }
                }
                Err(_) => self.notices.push(Notice::new(TESTS_FAILED, vec![("error", "the test thread stopped unexpectedly".to_string())], true)),
            }
        }
    }

    pub(crate) fn stop_output(&mut self, kind: OutputKind) {
        if let Some(t) = self.outputs.testing.remove(&kind) {
            // The thread ends at its next check of the flag; its result is dropped.
            t.cancel.cancel();
            self.outputs.canceled.push(t.handle);
        }
        if let Some(id) = self.outputs.stop(kind, false) {
            self.mixer.send(MixerCmd::RemoveOutput { id });
            self.update_encode_target();
        }
    }

    pub(crate) fn start_output(&mut self, kind: OutputKind) -> Result<(), StudioError> {
        if self.outputs.active.contains_key(&kind) || self.outputs.testing.contains_key(&kind) {
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
        let mut planned = plan(&self.profile, kind);
        let registry = Arc::clone(self.engine.registry());
        let mut encoder = registry.encoder(&planned.encoder_id)?;
        let mut hardware = None;
        let program = if encoder.ffmpeg().is_some() || encoder.id() == HARDWARE {
            let located = ffmpeg::locate(&self.profile.output.ffmpeg_path).map_err(|e| StudioError::Output(e.to_string()));
            // The `hardware` choice of an MKV recording can still run NVENC without ffmpeg.
            let (program, missing) = match located {
                Ok(program) => (program, None),
                Err(e) if encoder.id() == HARDWARE && runs_built_in(&self.profile, kind, planned.rate) => (PathBuf::new(), Some(e)),
                Err(e) => return Err(e),
            };
            let offered = match missing {
                Some(_) => Vec::new(),
                None => self.outputs.encoders.entry(program.clone()).or_insert_with(|| ffmpeg::encoders(&program)).clone(),
            };
            // A VA-API encoder chosen by name is tested too, to find the render node it encodes on.
            let vaapi = craft_hwenc::candidate(encoder.id()).filter(|c| c.api == craft_hwenc::Api::Vaapi).map(|c| c.id);
            if encoder.id() == HARDWARE || vaapi.is_some() {
                let Some((id, run, notice)) = self.outputs.resolved.remove(&kind) else {
                    // The tests can take seconds each, so they run on their own thread, and
                    // `finish_hardware_tests` starts the output when they end.
                    self.test_hardware(kind, &planned, program, offered, vaapi);
                    return Ok(());
                };
                planned.encoder_settings = encoder_settings(&self.profile, kind, &id);
                encoder = registry.encoder(&id)?;
                planned.encoder_id = id;
                hardware = run;
                self.notices.extend(notice);
            }
            if let Some(e) = missing
                && encoder.ffmpeg().is_some()
            {
                return Err(e);
            }
            check_encoder(encoder.as_ref(), &offered, kind)?;
            if hardware.is_none()
                && let Some(candidate) = craft_hwenc::candidate(encoder.id())
            {
                // Chosen by name: a failure while it runs still counts against it.
                hardware = Some(HardwareRun { candidate, program: program.clone(), auto: false });
            }
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
        self.outputs.active.insert(kind, Active { id, instance, stats, started: Instant::now(), target, hardware });
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
    use studiocraft_media::audio::PooledBlock;
    use studiocraft_plugin::Plugin;

    use super::*;

    /// A prober that answers every test with one outcome and counts the tests.
    struct FakeProber(craft_hwenc::Outcome, std::sync::atomic::AtomicUsize);

    impl Prober for FakeProber {
        fn program(&self) -> &Path {
            Path::new("fake-ffmpeg")
        }
        fn probe(&self, _: &'static Candidate, _: Input, cancel: &Cancel) -> craft_hwenc::Outcome {
            self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.0 == craft_hwenc::Outcome::Canceled {
                cancel.cancel();
            }
            self.0.clone()
        }
    }

    fn fake(outcome: craft_hwenc::Outcome) -> FakeProber {
        FakeProber(outcome, std::sync::atomic::AtomicUsize::new(0))
    }

    #[test]
    fn the_hardware_choice_takes_a_working_encoder_or_falls_back_to_x264_with_a_notice() {
        let cache = ProbeCache::new();
        let works = fake(craft_hwenc::Outcome::Works);
        let (id, run, notice) = resolve_hardware("auto", 0x10de, false, false, &works, &cache, &Cancel::new()).unwrap();
        let first = craft_hwenc::auto_order(Vendor::Nvidia, Os::current(), craft_hwenc::Codec::H264);
        match first.first() {
            Some(c) => {
                assert_eq!(id, c.id);
                assert_eq!(run, Some(HardwareRun { candidate: c, program: PathBuf::from("fake-ffmpeg"), auto: true }));
                assert!(notice.is_none());
            }
            None => assert_eq!(id, "x264", "no hardware API on this system"),
        }
        let none = fake(craft_hwenc::Outcome::NoDevice("no device".into()));
        let (id, run, notice) = resolve_hardware("auto", 0x10de, false, false, &none, &ProbeCache::new(), &Cancel::new()).unwrap();
        assert_eq!((id.as_str(), run), ("x264", None));
        if !first.is_empty() {
            assert_eq!(notice.map(|n| n.template), Some("No hardware encoder passed its test; using {encoder}"));
        }
        let (id, _, notice) = resolve_hardware("software", 0x10de, false, false, &works, &ProbeCache::new(), &Cancel::new()).unwrap();
        assert_eq!(id, "x264");
        assert!(notice.is_none(), "choosing software is not a fallback");
    }

    #[test]
    fn a_named_hardware_encoder_that_fails_or_a_canceled_test_stops_the_start_with_a_clear_error() {
        let Some(c) = craft_hwenc::auto_order(Vendor::Nvidia, Os::current(), craft_hwenc::Codec::H264).first().copied() else { return };
        let busy = fake(craft_hwenc::Outcome::Busy("incompatible client key".into()));
        let Err(StudioError::Output(e)) = resolve_hardware(c.id, 0x10de, false, false, &busy, &ProbeCache::new(), &Cancel::new()) else {
            panic!("an error expected")
        };
        assert!(
            e.starts_with(&format!("{} did not pass its test encode", c.name)) && e.ends_with("choose another hardware encoder in Settings › Video"),
            "{e}"
        );
        let canceled = fake(craft_hwenc::Outcome::Canceled);
        let Err(StudioError::Output(e)) = resolve_hardware("auto", 0x10de, false, false, &canceled, &ProbeCache::new(), &Cancel::new()) else {
            panic!("an error expected")
        };
        assert_eq!(e, "the hardware encoder tests were canceled");
        assert_eq!(canceled.1.load(std::sync::atomic::Ordering::SeqCst), 1, "no test runs after the cancel");
    }

    #[test]
    fn lossless_recordings_and_streams_only_take_encoders_that_can_do_them() {
        let works = fake(craft_hwenc::Outcome::Works);
        // An AMD adapter puts AMF or VA-API first, but neither runs losslessly.
        let (id, run, _) = resolve_hardware("auto", 0x1002, false, true, &works, &ProbeCache::new(), &Cancel::new()).unwrap();
        match run {
            Some(run) => assert!(run.candidate.lossless() && run.candidate.id == id, "{id}"),
            None => assert_eq!(id, "x264"),
        }
        let Some(hevc) = craft_hwenc::candidate("nvenc_hevc").filter(|c| c.runs_on(Os::current())) else { return };
        let (id, _, _) = resolve_hardware(hevc.id, 0x10de, false, false, &works, &ProbeCache::new(), &Cancel::new()).unwrap();
        assert_eq!(id, "nvenc_hevc", "a recording takes HEVC");
        let Err(StudioError::Output(e)) = resolve_hardware(hevc.id, 0x10de, true, false, &works, &ProbeCache::new(), &Cancel::new()) else {
            panic!("a stream takes H.264 only")
        };
        assert!(e.starts_with("NVIDIA NVENC HEVC writes another codec"), "{e}");
        let amf = craft_hwenc::candidate("amf_h264").filter(|c| c.runs_on(Os::current()));
        if let Some(amf) = amf {
            let Err(StudioError::Output(e)) = resolve_hardware(amf.id, 0x1002, false, true, &works, &ProbeCache::new(), &Cancel::new()) else {
                panic!("AMF has no lossless mode")
            };
            assert!(e.starts_with("AMD AMF H.264 cannot record losslessly"), "{e}");
        }
    }

    #[test]
    fn only_failures_from_the_encoder_count_against_it() {
        let nvenc = craft_hwenc::candidate("nvenc_h264").unwrap();
        assert!(encoder_failed(nvenc, "ffmpeg stopped: [h264_nvenc @ 0000021] Failed locking bitstream buffer"));
        assert!(encoder_failed(nvenc, "ffmpeg stopped: Cannot load nvcuda.dll"));
        assert!(!encoder_failed(nvenc, "ffmpeg stopped: Connection reset by peer"), "a network error is not the encoder's");
        let busy = "ffmpeg stopped: [h264_nvenc @ 0000021] OpenEncodeSessionEx failed: incompatible client key (21)";
        assert!(!encoder_failed(nvenc, busy), "a full session is tried again at the next start");
        assert!(encoder_failed(nvenc, "NVIDIA NVENC H.264 (built in) stopped: encoding a picture failed (8)"), "the built-in encoder's failure");
    }

    #[test]
    fn an_mkv_recording_tests_nvenc_in_this_process_and_runs_the_built_in_encoder() {
        let run = |id: &'static str| HardwareRun { candidate: craft_hwenc::candidate(id).unwrap(), program: PathBuf::from("ffmpeg"), auto: true };
        assert_eq!(built_in_twin(("nvenc_h264".into(), Some(run("nvenc_h264")), None)).0, "nvenc_native_h264");
        assert_eq!(built_in_twin(("nvenc_hevc".into(), Some(run("nvenc_hevc")), None)).0, "nvenc_native_hevc");
        assert_eq!(built_in_twin(("amf_h264".into(), Some(run("amf_h264")), None)).0, "amf_h264", "no built-in twin");
        assert_eq!(built_in_twin(("x264".into(), None, None)).0, "x264");

        // The ffmpeg program does not exist: only the in-process test can say anything else.
        let ffmpeg = FfmpegProber::new("missing-ffmpeg-for-the-test", Vec::new());
        let p = BuiltInFirst { ffmpeg, fps: (30, 1), rate: RateControl::Quality(23) };
        let nvenc = craft_hwenc::candidate("nvenc_h264").unwrap();
        let amf = craft_hwenc::candidate("amf_h264").unwrap();
        match p.probe(nvenc, Input::Nv12, &Cancel::new()) {
            Outcome::Works => {}
            Outcome::NoDevice(why) => assert!(why.starts_with("NVIDIA NVENC H.264 (built in) cannot run on this system"), "{why}"),
            other => panic!("{other:?}"),
        }
        // AMF is tested in ffmpeg; a missing ffmpeg does not stop the tests before NVENC.
        assert!(matches!(p.probe(amf, Input::Nv12, &Cancel::new()), Outcome::NoDevice(why) if why.contains("was not found")));
        let canceled = Cancel::new();
        canceled.cancel();
        assert_eq!(p.probe(nvenc, Input::Nv12, &canceled), Outcome::Canceled);
        assert_ne!(p.args(nvenc, Input::Nv12), p.ffmpeg.args(nvenc, Input::Nv12), "kept apart in the cache");
        assert_eq!(p.args(amf, Input::Nv12), p.ffmpeg.args(amf, Input::Nv12));
        let ffmpeg = FfmpegProber::new("missing-ffmpeg-for-the-test", Vec::new());
        let at_60 = BuiltInFirst { ffmpeg, fps: (60, 1), rate: RateControl::Quality(23) };
        assert_ne!(at_60.args(nvenc, Input::Nv12), p.args(nvenc, Input::Nv12), "another frame rate is tested again");
    }

    /// An output that failed while it ran.
    struct FailedOutput;

    impl OutputInstance for FailedOutput {
        fn video(&self) -> Arc<Fifo<Arc<PooledFrame>>> {
            Arc::new(Fifo::new(1))
        }
        fn audio(&self) -> Arc<Fifo<PooledBlock>> {
            Arc::new(Fifo::new(1))
        }
        fn status(&self) -> OutputStatus {
            OutputStatus::Failed("ffmpeg stopped: [h264_mf @ 01] could not set output type".into())
        }
        fn target(&self) -> String {
            String::new()
        }
        fn stop(&mut self) -> Result<(), PluginError> {
            // An error, as from a failed ffmpeg: its notice was shown while it ran.
            Err(PluginError::Failed("ffmpeg stopped".into()))
        }
    }

    #[test]
    fn a_new_graphics_device_forgets_the_hardware_encoder_tests() {
        let mut o = Outputs::default();
        let nvenc = craft_hwenc::candidate("nvenc_h264").unwrap();
        let prober = FfmpegProber::new("ffmpeg", Vec::new());
        o.probes.mark_failed(Path::new("ffmpeg"), nvenc, "no device");
        let old = Arc::clone(&o.probes);
        o.forget_hardware_tests();
        assert_eq!(o.probes.get(&prober, nvenc, craft_hwenc::Input::Nv12), None);
        // A test still running for the old device writes to the old cache.
        old.mark_failed(Path::new("ffmpeg"), nvenc, "old device");
        assert_eq!(o.probes.get(&prober, nvenc, craft_hwenc::Input::Nv12), None);
        assert_eq!(o.device_changes, 1, "tests started before the change are run again");
    }

    #[test]
    fn shutting_down_cancels_and_waits_for_the_hardware_encoder_tests() {
        let mut o = Outputs::default();
        let cancel = Cancel::new();
        let seen = cancel.clone();
        let ended = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done = Arc::clone(&ended);
        let handle = std::thread::spawn(move || {
            // Stands for a test encode that runs until it is canceled.
            for _ in 0..10_000_000 {
                if seen.is_canceled() {
                    done.store(true, std::sync::atomic::Ordering::SeqCst);
                    return Err(StudioError::Output("canceled".into()));
                }
                std::thread::yield_now();
            }
            Err(StudioError::Output("never canceled".into()))
        });
        let t = Testing { cancel, handle, started: Instant::now(), profile: Profile::default(), device: 0, beside_encoder: false };
        o.testing.insert(OutputKind::Recording, t);
        // A test canceled earlier, still finishing.
        let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let f = Arc::clone(&finished);
        o.canceled.push(std::thread::spawn(move || {
            for _ in 0..100_000 {
                std::thread::yield_now();
            }
            f.store(true, std::sync::atomic::Ordering::SeqCst);
            Err(StudioError::Output("canceled".into()))
        }));
        o.stop_all_blocking();
        assert!(ended.load(std::sync::atomic::Ordering::SeqCst), "the test ended through its cancel before the shutdown returned");
        assert!(finished.load(std::sync::atomic::Ordering::SeqCst), "the shutdown waited for the canceled test");
        assert!(o.canceled.is_empty());
        assert!(o.testing.is_empty());
        assert!(o.state(OutputKind::Recording).is_none());
    }

    #[test]
    fn a_hardware_encoder_that_fails_while_recording_is_skipped_for_the_session() {
        let mf = craft_hwenc::candidate("mf_h264").unwrap();
        let mut o = Outputs::default();
        let program = PathBuf::from("fake-ffmpeg");
        let hardware = Some(HardwareRun { candidate: mf, program: program.clone(), auto: true });
        let active = Active {
            id: 1,
            instance: Box::new(FailedOutput),
            stats: Arc::new(EncoderStats::default()),
            started: Instant::now(),
            target: String::new(),
            hardware,
        };
        o.active.insert(OutputKind::Recording, active);
        let ev = o.poll();
        let templates: Vec<_> = ev.notices.iter().map(|n| n.template).collect();
        assert_eq!(templates, ["The recording stopped: {error}", "{encoder} stopped working; the next start tries another encoder"]);
        let prober = fake(craft_hwenc::Outcome::Works);
        assert_eq!(prober.program(), program);
        assert!(matches!(o.probes.get(&prober, mf, Input::Nv12), Some(craft_hwenc::Outcome::Failed(_))));
        assert!(o.active.is_empty());
    }

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
