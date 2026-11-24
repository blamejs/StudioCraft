//! The virtual camera: the program sent to a device that other apps open as a camera
//! ([`studiocraft_platform::virtual_camera`]).
//!
//! A thread takes the newest NV12 frame from the output's queue and writes it to the device. The
//! compositor never waits for it: a frame that arrives while the thread is busy replaces the one
//! waiting, and the frame it replaces is counted as dropped. While no frame comes, the thread
//! checks the queue every [`IDLE`], so a stop is seen within that time. The virtual camera has no
//! sound, so it does not read the job's audio, and it sends raw frames without running the job's
//! video encoder.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

use studiocraft_media::audio::PooledBlock;
use studiocraft_media::queue::Fifo;
use studiocraft_media::stats::EncoderStats;
use studiocraft_media::video::{PixelFormat, PooledFrame, VideoFrame};
use studiocraft_model::{Field, FieldKind, Settings, SettingsSchema};
use studiocraft_platform::PlatformError;
use studiocraft_platform::virtual_camera as device;
use studiocraft_plugin::{Output, OutputInstance, OutputJob, OutputStatus, Plugin, PluginError};

use crate::writer::IDLE;

/// Frames waiting for the device: the newest one, and one that came while it was written.
const QUEUE: usize = 2;
/// How long a stop waits for the thread to end. A write to the device does not block, so the
/// thread ends within [`IDLE`] of the stop once its current write returns.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// Where the thread writes frames: the device, or a stand-in in tests.
pub(crate) trait Camera: Send {
    /// Send the NV12 `frame`, and return the bytes the device took; `Ok(None)` when the device
    /// had no room for it.
    fn write(&mut self, frame: &VideoFrame) -> Result<Option<usize>, PlatformError>;
}

impl Camera for device::VirtualCamera {
    fn write(&mut self, frame: &VideoFrame) -> Result<Option<usize>, PlatformError> {
        device::VirtualCamera::write(self, frame)
    }
}

/// The virtual camera output.
pub struct VirtualCamera;

impl Plugin for VirtualCamera {
    fn id(&self) -> &'static str {
        "vcam"
    }
    fn name(&self) -> &'static str {
        "Virtual Camera"
    }
    fn schema(&self) -> SettingsSchema {
        SettingsSchema::new(vec![Field::new("device", "Device", FieldKind::Text { multiline: false }, "")])
    }
}

impl Output for VirtualCamera {
    /// Open the device named by `device` (a device node), or the first virtual camera device
    /// when it is empty, at the job's size and frame rate.
    fn start(&self, settings: &Settings, job: OutputJob) -> Result<Box<dyn OutputInstance>, PluginError> {
        if job.video.pixel != PixelFormat::Nv12 {
            return Err(PluginError::Failed("the virtual camera takes NV12 frames".into()));
        }
        let v = job.video;
        let path = match settings.get("device") {
            None => "",
            Some(d) => d.as_str().ok_or_else(|| PluginError::Failed(format!("the virtual camera's device must be text, not {d}")))?,
        };
        let camera = device::VirtualCamera::open(path, v.width, v.height, v.fps_num, v.fps_den).map_err(|e| match e {
            PlatformError::Unsupported => PluginError::Unavailable("the virtual camera works on Linux only for now".into()),
            e => PluginError::Failed(format!("the virtual camera could not be started: {e}")),
        })?;
        let info = camera.info();
        let target = if info.name == info.path { info.path.clone() } else { format!("{} ({})", info.name, info.path) };
        Ok(Box::new(Running::spawn(Box::new(camera), job.stats, target)?))
    }
}

/// The output's state, as [`Shared::state`] holds it.
const STARTING: u8 = 0;
const RUNNING: u8 = 1;
const STOPPED: u8 = 2;
const FAILED: u8 = 3;

/// What the thread and the output share, without a lock: the stop request and the state in
/// atomics, and the failure's text in a [`OnceLock`], whose `get` does not wait.
struct Shared {
    stop: AtomicBool,
    state: AtomicU8,
    failure: OnceLock<String>,
}

impl Shared {
    fn set(&self, state: u8) {
        self.state.store(state, Ordering::Release);
    }

    /// Record the failure `e`; the text is stored before the state says the output failed.
    fn fail(&self, e: String) {
        let _ = self.failure.set(e);
        self.set(FAILED);
    }

    fn status(&self) -> OutputStatus {
        match self.state.load(Ordering::Acquire) {
            RUNNING => OutputStatus::Running,
            STOPPED => OutputStatus::Stopped,
            FAILED => OutputStatus::Failed(self.failure.get().cloned().unwrap_or_default()),
            _ => OutputStatus::Starting,
        }
    }
}

/// A running virtual camera.
pub(crate) struct Running {
    video: Arc<Fifo<Arc<PooledFrame>>>,
    audio: Arc<Fifo<PooledBlock>>,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    target: String,
}

impl Running {
    /// Start the thread that writes frames to `camera`, counting them in `stats`.
    pub(crate) fn spawn(camera: Box<dyn Camera>, stats: Arc<EncoderStats>, target: String) -> Result<Running, PluginError> {
        let video = Arc::new(Fifo::new(QUEUE));
        let shared = Arc::new(Shared { stop: AtomicBool::new(false), state: AtomicU8::new(STARTING), failure: OnceLock::new() });
        let (q, s) = (Arc::clone(&video), Arc::clone(&shared));
        let thread = std::thread::Builder::new()
            .name("virtual camera".into())
            .spawn(move || send(camera, &q, &s, &stats))
            .map_err(|e| PluginError::Failed(format!("the virtual camera could not be started: {e}")))?;
        Ok(Running { video, audio: Arc::new(Fifo::new(1)), shared, thread: Some(thread), target })
    }
}

/// The thread: write the newest frame until a stop or a failure. The dropped count is the frames
/// the full queue pushed out plus the frames the thread itself dropped.
fn send(mut camera: Box<dyn Camera>, video: &Fifo<Arc<PooledFrame>>, shared: &Shared, stats: &EncoderStats) {
    let mut started = false;
    let mut dropped = 0u64;
    let count = |dropped: u64| stats.dropped.store(video.dropped() + dropped, Ordering::Relaxed);
    while !shared.stop.load(Ordering::Acquire) {
        let mut newest = None;
        while let Some(f) = video.pop() {
            if newest.replace(f).is_some() {
                dropped += 1;
            }
        }
        count(dropped);
        let Some(frame) = newest else {
            std::thread::sleep(IDLE);
            continue;
        };
        match camera.write(&frame) {
            Ok(Some(sent)) => {
                stats.frames.fetch_add(1, Ordering::Relaxed);
                stats.bytes.fetch_add(u64::try_from(sent).unwrap_or(u64::MAX), Ordering::Relaxed);
                if !started {
                    started = true;
                    shared.set(RUNNING);
                }
            }
            Ok(None) => {
                dropped += 1;
                count(dropped);
            }
            Err(e) => {
                count(dropped);
                shared.fail(e.to_string());
                return;
            }
        }
    }
    count(dropped);
    shared.set(STOPPED);
}

impl OutputInstance for Running {
    fn video(&self) -> Arc<Fifo<Arc<PooledFrame>>> {
        Arc::clone(&self.video)
    }
    fn audio(&self) -> Arc<Fifo<PooledBlock>> {
        Arc::clone(&self.audio)
    }
    fn status(&self) -> OutputStatus {
        self.shared.status()
    }
    fn target(&self) -> String {
        self.target.clone()
    }
    fn stop(&mut self) -> Result<(), PluginError> {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let deadline = std::time::Instant::now() + STOP_TIMEOUT;
            while !t.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(IDLE);
            }
            if !t.is_finished() {
                // The thread is left to end on its own; the device closes when it does.
                return Err(PluginError::Failed("the virtual camera did not stop in time".into()));
            }
            if t.join().is_err() {
                return Err(PluginError::Failed("the virtual camera stopped unexpectedly".into()));
            }
        }
        match self.status() {
            OutputStatus::Failed(e) => Err(PluginError::Failed(e)),
            _ => Ok(()),
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use studiocraft_media::pool::Pool;

    use super::*;

    /// A camera that reports each frame's first byte, and answers each write with what `reply`
    /// gives. Like a YUYV device, it takes two bytes a pixel.
    struct Fake {
        written: mpsc::Sender<u8>,
        reply: Box<dyn FnMut(u8) -> Result<bool, PlatformError> + Send>,
    }

    impl Camera for Fake {
        fn write(&mut self, frame: &VideoFrame) -> Result<Option<usize>, PlatformError> {
            let first = frame.data.first().copied().unwrap_or_default();
            let r = (self.reply)(first);
            let _ = self.written.send(first);
            Ok(r?.then_some(frame.width as usize * frame.height as usize * 2))
        }
    }

    fn camera(reply: impl FnMut(u8) -> Result<bool, PlatformError> + Send + 'static) -> (Box<dyn Camera>, mpsc::Receiver<u8>) {
        let (tx, rx) = mpsc::channel();
        (Box::new(Fake { written: tx, reply: Box::new(reply) }), rx)
    }

    fn frame(pool: &Arc<studiocraft_media::video::FramePool>, value: u8) -> Arc<PooledFrame> {
        let mut f = pool.take();
        assert!(f.reshape(4, 2, PixelFormat::Nv12));
        f.data.fill(value);
        Arc::new(f)
    }

    #[test]
    fn it_waits_for_a_late_first_frame_without_holding_up_a_stop() {
        let (cam, written) = camera(|_| Ok(true));
        let stats = Arc::new(EncoderStats::default());
        let mut out = Running::spawn(cam, Arc::clone(&stats), "test".into()).unwrap();
        // No frame has come: the output waits for one, and says so.
        assert_eq!(out.status(), OutputStatus::Starting);
        assert!(written.try_recv().is_err());
        let pool = Pool::new(4, VideoFrame::default);
        out.video().push(frame(&pool, 7));
        assert_eq!(written.recv(), Ok(7));
        while out.status() == OutputStatus::Starting {
            std::thread::yield_now();
        }
        assert_eq!(out.status(), OutputStatus::Running);
        assert_eq!(stats.frames.load(Ordering::Relaxed), 1);
        // The bytes the device took, in its encoding: 4 x 2 pixels at two bytes each.
        assert_eq!(stats.bytes.load(Ordering::Relaxed), 16);
        out.stop().unwrap();
        assert_eq!(out.status(), OutputStatus::Stopped);
    }

    #[test]
    fn a_stop_ends_an_output_that_never_got_a_frame() {
        let (cam, written) = camera(|_| Ok(true));
        let mut out = Running::spawn(cam, Arc::new(EncoderStats::default()), "test".into()).unwrap();
        out.stop().unwrap();
        assert_eq!(out.status(), OutputStatus::Stopped);
        assert!(written.try_recv().is_err());
    }

    #[test]
    fn frames_that_came_while_the_device_was_busy_give_way_to_the_newest() {
        // The first write says it has begun, then waits until the test has queued three more
        // frames.
        let (entered, begun) = mpsc::channel::<()>();
        let (go, wait) = mpsc::channel::<()>();
        let (cam, written) = camera(move |first| {
            if first == 1 {
                let _ = entered.send(());
                let _ = wait.recv();
            }
            Ok(true)
        });
        let stats = Arc::new(EncoderStats::default());
        let mut out = Running::spawn(cam, Arc::clone(&stats), "test".into()).unwrap();
        let pool = Pool::new(8, VideoFrame::default);
        out.video().push(frame(&pool, 1));
        // Frames 2, 3 and 4 come while frame 1 is written. The full queue drops frame 2, and
        // frame 4 replaces frame 3: both count as dropped.
        begun.recv().unwrap();
        out.video().push(frame(&pool, 2));
        out.video().push(frame(&pool, 3));
        out.video().push(frame(&pool, 4));
        go.send(()).unwrap();
        assert_eq!(written.recv(), Ok(1));
        assert_eq!(written.recv(), Ok(4));
        out.stop().unwrap();
        assert_eq!(stats.frames.load(Ordering::Relaxed), 2);
        assert_eq!(stats.dropped.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn a_frame_the_device_has_no_room_for_is_dropped_and_the_next_one_is_sent() {
        let (cam, written) = camera(|first| Ok(first != 1));
        let stats = Arc::new(EncoderStats::default());
        let mut out = Running::spawn(cam, Arc::clone(&stats), "test".into()).unwrap();
        let pool = Pool::new(4, VideoFrame::default);
        out.video().push(frame(&pool, 1));
        assert_eq!(written.recv(), Ok(1));
        assert_eq!(out.status(), OutputStatus::Starting);
        out.video().push(frame(&pool, 2));
        assert_eq!(written.recv(), Ok(2));
        out.stop().unwrap();
        assert_eq!(stats.dropped.load(Ordering::Relaxed), 1);
        assert_eq!(stats.frames.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_device_error_fails_the_output() {
        let (cam, written) = camera(|_| Err(PlatformError::Failed("unplugged".into())));
        let mut out = Running::spawn(cam, Arc::new(EncoderStats::default()), "test".into()).unwrap();
        let pool = Pool::new(4, VideoFrame::default);
        out.video().push(frame(&pool, 1));
        assert_eq!(written.recv(), Ok(1));
        while out.status() == OutputStatus::Starting {
            std::thread::yield_now();
        }
        assert_eq!(out.status(), OutputStatus::Failed("unplugged".into()));
        let e = out.stop().unwrap_err();
        assert!(e.to_string().contains("unplugged"), "{e}");
    }

    #[test]
    fn a_start_without_a_device_is_an_error_the_user_can_act_on() {
        use studiocraft_plugin::VideoFormat;
        let x264 = crate::encoders::all().into_iter().next().unwrap();
        let job = OutputJob {
            video: VideoFormat { width: 64, height: 48, fps_num: 30, fps_den: 1, pixel: PixelFormat::Nv12 },
            sample_rate: 48_000,
            video_encoder: Arc::new(x264),
            video_encoder_settings: Settings::new(),
            rate: studiocraft_plugin::RateControl::Quality(23),
            audio_bitrate_kbps: 128,
            ffmpeg: std::path::PathBuf::new(),
            stats: Arc::new(EncoderStats::default()),
            start: studiocraft_media::Tick(0),
            captions: None,
        };
        let settings = Settings::new().with("device", "/dev/studiocraft-no-such-camera");
        let Err(e) = VirtualCamera.start(&settings, job.clone()) else { panic!("a camera started without a device") };
        let text = e.to_string();
        assert!(text.contains("virtual camera"), "{text}");
        for bad in [serde_json::json!(7), serde_json::json!(true), serde_json::Value::Null] {
            let settings = Settings::new().with("device", bad.clone());
            let Err(e) = VirtualCamera.start(&settings, job.clone()) else { panic!("a camera started with the device {bad}") };
            assert!(e.to_string().contains("must be text"), "{e}");
        }
        let mut rgba = job;
        rgba.video.pixel = PixelFormat::Rgba8;
        assert!(VirtualCamera.start(&Settings::new(), rgba).is_err());
    }
}
