//! Camera capture.

use std::sync::Arc;
use std::time::{Duration, Instant};

use studiocraft_media::video::VideoFrame;
use studiocraft_model::{ChoiceOption, Field, FieldKind, Settings, SettingsSchema};
use studiocraft_platform::PlatformError;
use studiocraft_platform::camera::{self, CameraCapture, CameraFormat, CameraInfo, Watch, best_match};
use studiocraft_plugin::{Pace, Plugin, PluginError, Source, SourceCaps, SourceInstance, SourceIo, VideoContent};

/// How long a camera that failed waits before it is opened again.
const REOPEN: Duration = Duration::from_secs(2);

/// How long a streaming camera may send nothing before it counts as lost.
const STALL: Duration = Duration::from_secs(5);

/// How often the source looks for a new frame when no format is chosen.
const DEFAULT_FPS: u32 = 60;

/// The format setting, `None` for the camera's default.
fn format_of(s: &Settings) -> Option<CameraFormat> {
    s.str("format").and_then(CameraFormat::parse)
}

/// What the source does with its camera; [`CameraCapture`] in the app.
trait Camera: Sized {
    fn open(device: &str, format: Option<CameraFormat>) -> Result<Self, PlatformError>;
    fn grab(&mut self, frame: &mut VideoFrame) -> Result<(), PlatformError>;
    /// The format the camera is open in, when it differs from the one asked for.
    fn other_format(&self) -> Option<CameraFormat>;
    /// What tells when a camera is plugged in or out, where the system reports it.
    fn watch() -> Option<Arc<Watch>>;
    /// The listed camera the capture chose, once it has.
    fn camera(&self) -> Option<CameraInfo>;
}

impl Camera for CameraCapture {
    fn watch() -> Option<Arc<Watch>> {
        camera::watch()
    }
    fn camera(&self) -> Option<CameraInfo> {
        CameraCapture::camera(self).cloned()
    }
    fn open(device: &str, format: Option<CameraFormat>) -> Result<Self, PlatformError> {
        CameraCapture::open(device, format)
    }
    fn grab(&mut self, frame: &mut VideoFrame) -> Result<(), PlatformError> {
        CameraCapture::grab(self, frame)
    }
    fn other_format(&self) -> Option<CameraFormat> {
        CameraCapture::other_format(self)
    }
}

/// The note the source shows while its camera is open in another format than its own: another
/// source opened the camera first, and both show it in that format. The source keeps working.
const SHARED_FORMAT: &str = "the camera is already open in another format";

/// Why the source shows nothing while the system does not list its camera.
const NOT_CONNECTED: &str = "the camera is not connected";

/// A camera that is opened again while it fails. Where the system reports cameras plugged in or
/// out, an unplugged camera is closed at once and opened when it is plugged in again.
struct Instance<C: Camera> {
    device: String,
    format: Option<CameraFormat>,
    cap: Option<C>,
    retry_at: Instant,
    /// When the last frame arrived, or when the camera started sending.
    last_frame: Instant,
    /// Why the camera is closed, reported until it opens again.
    error: Option<PluginError>,
    watch: Option<Arc<Watch>>,
    /// The watch's generation `listed` was read at; `None` to read it again.
    seen: Option<u64>,
    /// Whether the watch lists the camera: the one the open capture chose, or while the camera is
    /// closed or the capture has not chosen yet, one that matches the setting. `None` when the
    /// watch does not know.
    listed: Option<bool>,
}

impl<C: Camera> Instance<C> {
    fn new(s: &Settings, now: Instant) -> Self {
        let mut i = Instance {
            device: String::new(),
            format: None,
            cap: None,
            retry_at: now,
            last_frame: now,
            error: None,
            watch: C::watch(),
            seen: None,
            listed: None,
        };
        i.set(s, now);
        i
    }

    /// Read again whether the watch lists the camera, when the cameras changed since the last
    /// read or `seen` was cleared. Otherwise only an atomic is read. Gives whether a closed source
    /// just found a camera that matches its setting listed. An open capture's camera that is
    /// unplugged counts as not listed even when another camera matches the setting.
    fn look(&mut self) -> bool {
        let Some(watch) = &self.watch else { return false };
        let generation = watch.generation();
        if self.seen == Some(generation) {
            return false;
        }
        self.seen = Some(generation);
        let before = self.listed;
        self.listed = watch.cameras().map(|list| match self.cap.as_ref().and_then(C::camera) {
            Some(chosen) => list.contains(&chosen),
            None => best_match(&list, &CameraInfo::parse_key(&self.device)).is_some(),
        });
        // A list that was not known in between does not hide that the camera came back.
        self.cap.is_none() && before != Some(true) && self.listed == Some(true)
    }

    /// The camera opened; the next frame reads the watch again.
    fn opened(&mut self, c: C) {
        self.cap = Some(c);
        self.error = None;
        self.seen = None;
    }

    /// Take the settings and open the camera they name.
    fn set(&mut self, s: &Settings, now: Instant) {
        self.device = s.str("device").unwrap_or_default().to_string();
        self.format = format_of(s);
        self.retry_at = now + REOPEN;
        self.last_frame = now;
        self.seen = None;
        self.listed = None;
        self.look();
        if self.listed == Some(false) {
            self.cap = None;
            self.error = Some(PluginError::Unavailable(NOT_CONNECTED.into()));
            return;
        }
        match C::open(&self.device, self.format) {
            Ok(c) => self.opened(c),
            Err(e) => {
                self.cap = None;
                self.error = Some(PluginError::Unavailable(e.to_string()));
            }
        }
    }

    fn lost(&mut self, io: &SourceIo, now: Instant, e: PluginError) -> Result<(), PluginError> {
        self.cap = None;
        // Closed, the source looks for any camera that matches the setting.
        self.seen = None;
        self.retry_at = now + REOPEN;
        self.error = Some(e.clone());
        // The last picture of a camera that went away is not kept on screen.
        io.video.publish(VideoContent::Empty);
        Err(e)
    }

    fn produce_at(&mut self, io: &SourceIo, now: Instant) -> Result<(), PluginError> {
        if self.look() {
            // Plugged in again: open it now rather than at the next try.
            self.retry_at = now;
        }
        if self.listed == Some(false) {
            // Unplugged: closed at once, and not tried again until the system lists it. A closed
            // camera's picture was cleared when it closed.
            if self.cap.is_some() {
                return self.lost(io, now, PluginError::Unavailable(NOT_CONNECTED.into()));
            }
            if !matches!(&self.error, Some(PluginError::Unavailable(why)) if why == NOT_CONNECTED) {
                self.error = Some(PluginError::Unavailable(NOT_CONNECTED.into()));
            }
            return Err(self.error.clone().unwrap_or_else(|| PluginError::Unavailable(NOT_CONNECTED.into())));
        }
        if self.cap.is_none() {
            if now < self.retry_at {
                return Err(self.error.clone().unwrap_or_else(|| PluginError::Unavailable("waiting to open the camera again".into())));
            }
            self.retry_at = now + REOPEN;
            match C::open(&self.device, self.format) {
                Ok(c) => {
                    self.opened(c);
                    self.last_frame = now;
                }
                Err(e) => return self.lost(io, now, PluginError::Unavailable(e.to_string())),
            }
        }
        let Some(cap) = self.cap.as_mut() else { return Ok(()) };
        let mut frame = io.video.frame();
        match cap.grab(&mut frame) {
            Ok(()) => {
                self.last_frame = now;
                frame.pts = io.clock.now();
                io.video.publish(VideoContent::Frame(frame));
                Ok(())
            }
            // The camera is opening, or has not sent its first picture yet.
            Err(PlatformError::Waiting) if now.saturating_duration_since(self.last_frame) < STALL => {
                Err(PluginError::Unavailable("waiting for the camera".into()))
            }
            Err(PlatformError::Waiting) => self.lost(io, now, PluginError::Failed("the camera sent no picture".into())),
            Err(PlatformError::Unchanged) if now.saturating_duration_since(self.last_frame) < STALL => Ok(()),
            Err(PlatformError::Unchanged) => self.lost(io, now, PluginError::Failed("the camera stopped sending pictures".into())),
            Err(e) => self.lost(io, now, PluginError::Failed(e.to_string())),
        }
    }
}

impl<C: Camera> SourceInstance for Instance<C> {
    fn pace(&self) -> Pace {
        // A camera shared in another format sends at that format's rate.
        let format = self.cap.as_ref().and_then(C::other_format).or(self.format);
        let fps = format.map_or(DEFAULT_FPS, |f| f.fps.clamp(1, 240));
        Pace::Every(Duration::from_secs_f64(1.0 / f64::from(fps)))
    }
    /// The camera shows the picture in the format another source opened it in.
    fn notice(&self) -> Option<&'static str> {
        self.cap.as_ref().and_then(C::other_format).map(|_| SHARED_FORMAT)
    }
    fn produce(&mut self, io: &SourceIo) -> Result<(), PluginError> {
        self.produce_at(io, Instant::now())
    }
    fn update(&mut self, s: &Settings, io: &SourceIo) -> Result<(), PluginError> {
        if s.str("device").unwrap_or_default() != self.device || format_of(s) != self.format {
            // The earlier camera or format closes before the new one opens.
            self.cap = None;
            self.set(s, Instant::now());
            io.video.publish(VideoContent::Empty);
        }
        Ok(())
    }
}

/// The settings with `list` as the cameras and `formats` as the chosen camera's formats.
fn schema(list: &[CameraInfo], formats: &[CameraFormat]) -> SettingsSchema {
    let mut devices = vec![ChoiceOption::new("", "Default")];
    devices.extend(list.iter().map(|c| ChoiceOption::new(c.key(), c.name.clone())));
    let mut modes = vec![ChoiceOption::new("", "Default")];
    modes.extend(formats.iter().map(|f| ChoiceOption::new(f.key(), f.label())));
    SettingsSchema::new(vec![
        Field::new("device", "Device", FieldKind::Choice { options: devices }, ""),
        Field::new("format", "Format", FieldKind::Choice { options: modes }, ""),
    ])
}

/// A video capture device: a webcam, a capture card or a camera's video output.
pub struct CameraSource;

impl Plugin for CameraSource {
    fn id(&self) -> &'static str {
        "camera"
    }
    fn name(&self) -> &'static str {
        "Camera"
    }
    fn schema(&self) -> SettingsSchema {
        self.schema_for(&Settings::new())
    }
    /// The formats listed are those of the camera `settings` choose.
    fn schema_for(&self, settings: &Settings) -> SettingsSchema {
        let list = camera::cameras();
        let want = CameraInfo::parse_key(settings.str("device").unwrap_or_default());
        let formats = best_match(&list, &want).and_then(|i| list.get(i)).map(camera::formats).unwrap_or_default();
        schema(&list, &formats)
    }
    /// Changes when a camera is plugged in or out. Listing the cameras does not open them.
    fn lists_stamp(&self) -> Option<u64> {
        Some(watched_stamp(camera::watch().as_deref(), camera::cameras))
    }
}

/// [`stamp`] of the cameras `watch` lists, or of the ones `list` gives where the watch does not
/// know them. A known list is read without asking the system.
fn watched_stamp(watch: Option<&Watch>, list: impl FnOnce() -> Vec<CameraInfo>) -> u64 {
    match watch.and_then(Watch::cameras) {
        Some(watched) => stamp(&watched),
        None => stamp(&list()),
    }
}

/// A value for a list of cameras that changes when the list does.
fn stamp(list: &[CameraInfo]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for c in list {
        c.key().hash(&mut h);
    }
    h.finish()
}

impl Source for CameraSource {
    fn caps(&self) -> SourceCaps {
        SourceCaps { video: true, audio: false }
    }
    fn create(&self, settings: &Settings, _io: &SourceIo) -> Result<Box<dyn SourceInstance>, PluginError> {
        Ok(Box::new(Instance::<CameraCapture>::new(settings, Instant::now())))
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::sync::{Mutex, mpsc};

    use studiocraft_media::clock::MasterClock;
    use studiocraft_media::video::PixelFormat;
    use studiocraft_platform::camera::{CameraBackend, CameraEvents, CameraPixel, CameraStream};

    use super::*;

    thread_local! {
        /// What the next grabs give, and every open (device, format).
        static SCRIPT: RefCell<Vec<Result<(), PlatformError>>> = const { RefCell::new(Vec::new()) };
        static OPENED: RefCell<Vec<(String, Option<CameraFormat>)>> = const { RefCell::new(Vec::new()) };
        static OPEN_FAILS: RefCell<bool> = const { RefCell::new(false) };
        /// The format another source opened the camera in.
        static OTHER: RefCell<Option<CameraFormat>> = const { RefCell::new(None) };
        /// What tells the sources of this test when a camera is plugged in or out.
        static WATCH: RefCell<Option<Arc<Watch>>> = const { RefCell::new(None) };
    }

    fn script(steps: Vec<Result<(), PlatformError>>) {
        SCRIPT.with(|s| *s.borrow_mut() = steps.into_iter().rev().collect());
    }

    fn opened() -> Vec<(String, Option<CameraFormat>)> {
        OPENED.with(|o| o.borrow().clone())
    }

    /// A camera that plays the script; an empty script gives unchanged pictures.
    struct Scripted {
        /// The camera of the test's watch it chose, as a capture's thread chooses one.
        chosen: Option<CameraInfo>,
    }

    impl Camera for Scripted {
        fn open(device: &str, format: Option<CameraFormat>) -> Result<Self, PlatformError> {
            OPENED.with(|o| o.borrow_mut().push((device.to_string(), format)));
            if OPEN_FAILS.with(|f| *f.borrow()) {
                return Err(PlatformError::Failed("the camera could not be opened (in use)".into()));
            }
            let list = Self::watch().and_then(|w| w.cameras());
            let chosen = list.and_then(|list| best_match(&list, &CameraInfo::parse_key(device)).and_then(|i| list.get(i)).cloned());
            Ok(Scripted { chosen })
        }
        fn camera(&self) -> Option<CameraInfo> {
            self.chosen.clone()
        }
        fn grab(&mut self, frame: &mut VideoFrame) -> Result<(), PlatformError> {
            let step = SCRIPT.with(|s| s.borrow_mut().pop()).unwrap_or(Err(PlatformError::Unchanged));
            if step.is_ok() {
                frame.reshape(4, 2, PixelFormat::Rgba8);
            }
            step
        }
        fn other_format(&self) -> Option<CameraFormat> {
            OTHER.with(|o| *o.borrow())
        }
        fn watch() -> Option<Arc<Watch>> {
            WATCH.with(|w| w.borrow().clone())
        }
    }

    /// The cameras a test plugs in and out, as the system lists them; `None` when they cannot be
    /// listed.
    struct Plugs(Mutex<Option<Vec<CameraInfo>>>);

    impl CameraBackend for Plugs {
        fn cameras(&self) -> Result<Vec<CameraInfo>, PlatformError> {
            self.0.lock().unwrap().clone().ok_or(PlatformError::Failed("the cameras could not be listed".into()))
        }
        fn formats(&self, _camera: &CameraInfo) -> Result<Vec<CameraFormat>, PlatformError> {
            Err(PlatformError::Unsupported)
        }
        fn open(&self, _camera: &CameraInfo, _format: CameraFormat) -> Result<Box<dyn CameraStream>, PlatformError> {
            Err(PlatformError::Unsupported)
        }
    }

    /// The system's notices of cameras plugged in or out; dropping the sender ends them.
    struct Told(mpsc::Receiver<()>);

    impl CameraEvents for Told {
        fn wait(&mut self) -> Result<(), PlatformError> {
            self.0.recv().map_err(|_| PlatformError::Failed("stopped".into()))
        }
    }

    /// A system that reports cameras plugged in or out, with `list` plugged in. The sources this
    /// test makes use it.
    struct System {
        plugs: Arc<Plugs>,
        watch: Arc<Watch>,
        tell: mpsc::Sender<()>,
    }

    impl System {
        fn new(list: Vec<CameraInfo>) -> System {
            let plugs = Arc::new(Plugs(Mutex::new(Some(list))));
            let (tell, told) = mpsc::channel();
            let backend: Arc<dyn CameraBackend> = plugs.clone();
            let watch = camera::watch_with(backend, move || Ok(Box::new(Told(told)) as Box<dyn CameraEvents>));
            while watch.generation() == 0 {
                std::thread::yield_now();
            }
            WATCH.with(|w| *w.borrow_mut() = Some(Arc::clone(&watch)));
            System { plugs, watch, tell }
        }

        /// Plug in `list` and wait until the watch has handled the notice.
        fn plug(&self, list: Vec<CameraInfo>) {
            self.list(Some(list));
        }

        /// Make the next listing give `list`, or fail for `None`, and wait until the watch has
        /// handled the notice.
        fn list(&self, list: Option<Vec<CameraInfo>>) {
            *self.plugs.0.lock().unwrap() = list;
            let before = self.watch.handled();
            self.tell.send(()).unwrap();
            while self.watch.handled() == before {
                std::thread::yield_now();
            }
        }

        /// End the notices and wait until the watch has stopped.
        fn stop(&mut self) {
            self.tell = mpsc::channel().0;
            while self.watch.live() {
                std::thread::yield_now();
            }
        }
    }

    impl Drop for System {
        fn drop(&mut self) {
            WATCH.with(|w| *w.borrow_mut() = None);
        }
    }

    fn desk() -> CameraInfo {
        CameraInfo { name: "Desk".into(), id: "1".into() }
    }

    const HD: CameraFormat = CameraFormat { width: 1280, height: 720, fps: 30, pixel: CameraPixel::Mjpeg };

    fn settings() -> Settings {
        Settings::new().with("device", "Desk\u{1f}1").with("format", HD.key())
    }

    #[test]
    fn frames_are_shown_and_an_unchanged_picture_stays() {
        let io = SourceIo::new(MasterClock::new());
        let t = Instant::now();
        script(vec![Err(PlatformError::Waiting), Ok(()), Err(PlatformError::Unchanged)]);
        let mut c = Instance::<Scripted>::new(&settings(), t);
        assert_eq!(opened().last(), Some(&("Desk\u{1f}1".to_string(), Some(HD))));
        assert_eq!(c.pace(), Pace::Every(Duration::from_secs_f64(1.0 / 30.0)), "polled at the format's rate");
        assert_eq!(c.produce_at(&io, t), Err(PluginError::Unavailable("waiting for the camera".into())));
        c.produce_at(&io, t).unwrap();
        assert!(matches!(io.video.take(), Some(VideoContent::Frame(_))));
        c.produce_at(&io, t + Duration::from_secs(1)).unwrap();
        assert!(io.video.take().is_none());
        assert_eq!(io.video.size(), Some((4, 2)), "the last picture stays on screen");
    }

    #[test]
    fn a_camera_that_never_sends_its_first_picture_is_lost_and_opened_again() {
        let io = SourceIo::new(MasterClock::new());
        let t = Instant::now();
        script(vec![Err(PlatformError::Waiting), Err(PlatformError::Waiting)]);
        let mut c = Instance::<Scripted>::new(&settings(), t);
        assert_eq!(c.produce_at(&io, t + STALL - Duration::from_millis(1)), Err(PluginError::Unavailable("waiting for the camera".into())));
        assert_eq!(c.produce_at(&io, t + STALL), Err(PluginError::Failed("the camera sent no picture".into())));
        assert!(c.cap.is_none());
        let before = opened().len();
        script(vec![Ok(())]);
        c.produce_at(&io, t + STALL + REOPEN).unwrap();
        assert_eq!(opened().len(), before + 1);
    }

    #[test]
    fn a_camera_that_stops_sending_is_lost_and_opened_again() {
        let io = SourceIo::new(MasterClock::new());
        let t = Instant::now();
        script(vec![Ok(())]);
        let mut c = Instance::<Scripted>::new(&settings(), t);
        c.produce_at(&io, t).unwrap();
        let _ = io.video.take();
        assert!(c.produce_at(&io, t + STALL - Duration::from_millis(1)).is_ok(), "a short gap is not a loss");
        assert_eq!(c.produce_at(&io, t + STALL), Err(PluginError::Failed("the camera stopped sending pictures".into())));
        assert!(matches!(io.video.take(), Some(VideoContent::Empty)), "the last picture is cleared");
        let before = opened().len();
        let later = t + STALL;
        let waiting = c.produce_at(&io, later + REOPEN - Duration::from_millis(1));
        assert_eq!(waiting, Err(PluginError::Failed("the camera stopped sending pictures".into())), "the reason stays shown");
        assert_eq!(opened().len(), before, "not opened again before the wait");
        script(vec![Ok(())]);
        c.produce_at(&io, later + REOPEN).unwrap();
        assert_eq!(opened().len(), before + 1, "opened again after the wait");
        assert!(matches!(io.video.take(), Some(VideoContent::Frame(_))));
    }

    #[test]
    fn a_failed_or_busy_camera_is_tried_again_while_the_source_runs() {
        let io = SourceIo::new(MasterClock::new());
        let t = Instant::now();
        script(vec![Err(PlatformError::Failed("the camera stopped (unplugged)".into()))]);
        let mut c = Instance::<Scripted>::new(&settings(), t);
        assert_eq!(c.produce_at(&io, t), Err(PluginError::Failed("the camera stopped (unplugged)".into())));
        assert!(c.cap.is_none());
        OPEN_FAILS.with(|f| *f.borrow_mut() = true);
        let busy = c.produce_at(&io, t + REOPEN);
        OPEN_FAILS.with(|f| *f.borrow_mut() = false);
        assert_eq!(busy, Err(PluginError::Unavailable("the camera could not be opened (in use)".into())));
        assert!(matches!(io.video.take(), Some(VideoContent::Empty)));
        script(vec![Ok(())]);
        c.produce_at(&io, t + REOPEN + REOPEN).unwrap();
        assert!(matches!(io.video.take(), Some(VideoContent::Frame(_))));
    }

    #[test]
    fn a_camera_shared_in_another_format_shows_its_pictures_and_says_so() {
        let io = SourceIo::new(MasterClock::new());
        let t = Instant::now();
        let sd = CameraFormat { width: 640, height: 480, fps: 15, pixel: CameraPixel::Yuyv };
        OTHER.with(|o| *o.borrow_mut() = Some(sd));
        script(vec![Ok(()), Err(PlatformError::Unchanged)]);
        let mut c = Instance::<Scripted>::new(&settings(), t);
        let note = Some("the camera is already open in another format");
        assert_eq!(c.produce_at(&io, t), Ok(()), "the source is working, not failing");
        assert_eq!(c.notice(), note);
        assert!(matches!(io.video.take(), Some(VideoContent::Frame(_))), "the picture is shown");
        assert_eq!(c.produce_at(&io, t + Duration::from_secs(1)), Ok(()));
        assert_eq!(c.notice(), note, "and the note stays while the picture does");
        assert!(io.video.take().is_none());
        assert_eq!(c.pace(), Pace::Every(Duration::from_secs_f64(1.0 / 15.0)), "polled at the rate the camera sends");
        OTHER.with(|o| *o.borrow_mut() = None);
        assert_eq!(c.produce_at(&io, t + Duration::from_secs(2)), Ok(()));
        assert_eq!(c.notice(), None);
        assert_eq!(c.pace(), Pace::Every(Duration::from_secs_f64(1.0 / 30.0)));
    }

    #[test]
    fn a_new_device_or_format_opens_the_camera_again_and_clears_the_picture() {
        let io = SourceIo::new(MasterClock::new());
        script(vec![Ok(())]);
        let mut c = Instance::<Scripted>::new(&settings(), Instant::now());
        c.produce_at(&io, Instant::now()).unwrap();
        let _ = io.video.take();
        let before = opened().len();
        c.update(&settings(), &io).unwrap();
        assert_eq!(opened().len(), before, "the same settings keep the camera");
        assert!(io.video.take().is_none());
        c.update(&settings().with("format", ""), &io).unwrap();
        assert_eq!(opened().last(), Some(&("Desk\u{1f}1".to_string(), None)));
        assert!(matches!(io.video.take(), Some(VideoContent::Empty)));
        assert_eq!(c.pace(), Pace::Every(Duration::from_secs_f64(1.0 / 60.0)));
        c.update(&settings().with("device", "Room\u{1f}2"), &io).unwrap();
        assert_eq!(opened().last(), Some(&("Room\u{1f}2".to_string(), Some(HD))));
    }

    #[test]
    fn the_schema_lists_cameras_and_the_chosen_cameras_formats() {
        let list = [CameraInfo { name: "Desk".into(), id: "1".into() }];
        let s = schema(&list, &[HD]);
        let FieldKind::Choice { options } = &s.fields[0].kind else { panic!("device is a choice") };
        assert_eq!(options[1], ChoiceOption::new("Desk\u{1f}1", "Desk"));
        let FieldKind::Choice { options } = &s.fields[1].kind else { panic!("format is a choice") };
        assert_eq!(options[1], ChoiceOption::new("1280x720@30:MJPG", "1280 x 720, 30 fps, MJPEG"));
        assert!(s.problems(&Settings::new().with("format", "1280x720@30:MJPG")).is_empty());
        assert_eq!(s.problems(&Settings::new().with("format", "640x480@30:MJPG")).len(), 1, "a format the camera does not offer");
        // A saved camera that is not connected keeps its value.
        assert_eq!(s.normalize(&Settings::new().with("device", "Gone\u{1f}9")).str("device"), Some("Gone\u{1f}9"));
    }

    #[test]
    fn the_lists_stamp_changes_when_a_camera_is_plugged_in_or_out() {
        let desk = CameraInfo { name: "Desk".into(), id: "1".into() };
        let room = CameraInfo { name: "Room".into(), id: "2".into() };
        let both = stamp(&[desk.clone(), room.clone()]);
        assert_eq!(both, stamp(&[desk.clone(), room.clone()]));
        assert_ne!(both, stamp(std::slice::from_ref(&desk)), "one unplugged");
        assert_ne!(stamp(&[]), stamp(std::slice::from_ref(&desk)), "the first plugged in");
        assert_ne!(both, stamp(&[desk, CameraInfo { name: "Room".into(), id: "3".into() }]), "another camera of the same name");
    }

    #[test]
    fn an_unplugged_camera_closes_at_once_and_opens_as_soon_as_it_is_plugged_in() {
        let system = System::new(vec![desk()]);
        let io = SourceIo::new(MasterClock::new());
        let t = Instant::now();
        script(vec![Ok(())]);
        let mut c = Instance::<Scripted>::new(&settings(), t);
        c.produce_at(&io, t).unwrap();
        assert!(matches!(io.video.take(), Some(VideoContent::Frame(_))));
        system.plug(Vec::new());
        let gone = t + Duration::from_millis(10);
        let not_connected = Err(PluginError::Unavailable("the camera is not connected".into()));
        assert_eq!(c.produce_at(&io, gone), not_connected, "closed at the next frame, well before the 5 s with no picture");
        assert!(c.cap.is_none());
        assert!(matches!(io.video.take(), Some(VideoContent::Empty)), "the last picture is cleared");
        let before = opened().len();
        assert_eq!(c.produce_at(&io, gone + REOPEN + REOPEN), not_connected, "the reason stays shown");
        assert_eq!(opened().len(), before, "a camera the system does not list is not tried every 2 s");
        system.plug(vec![desk()]);
        script(vec![Ok(())]);
        let back = gone + REOPEN + REOPEN + Duration::from_millis(10);
        c.produce_at(&io, back).unwrap();
        assert_eq!(opened().len(), before + 1, "opened at the next frame, without waiting for the next try");
        assert!(matches!(io.video.take(), Some(VideoContent::Frame(_))));
    }

    #[test]
    fn a_camera_plugged_in_after_a_failed_listing_opens_at_once() {
        let system = System::new(vec![desk()]);
        let io = SourceIo::new(MasterClock::new());
        let t = Instant::now();
        script(vec![Ok(())]);
        let mut c = Instance::<Scripted>::new(&settings(), t);
        c.produce_at(&io, t).unwrap();
        system.plug(Vec::new());
        assert!(c.produce_at(&io, t).is_err());
        system.list(None);
        let before = opened().len();
        assert!(c.produce_at(&io, t + Duration::from_millis(1)).is_err(), "waits for the next try while the list is not known");
        assert_eq!(opened().len(), before);
        system.plug(vec![desk()]);
        script(vec![Ok(())]);
        c.produce_at(&io, t + Duration::from_millis(2)).unwrap();
        assert_eq!(opened().len(), before + 1, "opened at the next frame once the list holds it again");
    }

    #[test]
    fn an_unplugged_camera_is_closed_while_another_camera_of_its_name_stays() {
        let spare = CameraInfo { name: "Desk".into(), id: "7".into() };
        let system = System::new(vec![desk(), spare.clone()]);
        let io = SourceIo::new(MasterClock::new());
        let t = Instant::now();
        script(vec![Ok(())]);
        let mut c = Instance::<Scripted>::new(&settings(), t);
        c.produce_at(&io, t).unwrap();
        let _ = io.video.take();
        system.plug(vec![spare]);
        let not_connected = Err(PluginError::Unavailable("the camera is not connected".into()));
        assert_eq!(c.produce_at(&io, t), not_connected, "the camera the source showed is gone");
        assert!(matches!(io.video.take(), Some(VideoContent::Empty)));
        let before = opened().len();
        script(vec![Ok(())]);
        c.produce_at(&io, t + Duration::from_millis(1)).unwrap();
        assert_eq!(opened().len(), before + 1, "the other camera of that name opens at the next frame");
        assert!(matches!(io.video.take(), Some(VideoContent::Frame(_))));
        // A camera plugged in that the setting would also match leaves the open camera alone.
        system.plug(vec![CameraInfo { name: "Desk".into(), id: "7".into() }, desk()]);
        assert_eq!(c.produce_at(&io, t + Duration::from_millis(2)), Ok(()));
        assert_eq!(opened().len(), before + 1);
    }

    #[test]
    fn a_failed_listing_does_not_change_which_camera_an_open_source_shows() {
        let room = CameraInfo { name: "Room".into(), id: "2".into() };
        let system = System::new(vec![desk(), room.clone()]);
        let io = SourceIo::new(MasterClock::new());
        let t = Instant::now();
        script(vec![Ok(())]);
        let mut c = Instance::<Scripted>::new(&Settings::new(), t);
        c.produce_at(&io, t).unwrap();
        let _ = io.video.take();
        system.list(None);
        assert_eq!(c.produce_at(&io, t), Ok(()), "a list that is not known closes nothing");
        // Listed again with another camera first, which Default would choose now.
        system.plug(vec![room.clone(), desk()]);
        assert_eq!(c.produce_at(&io, t), Ok(()));
        system.plug(vec![room]);
        assert_eq!(c.produce_at(&io, t), Err(PluginError::Unavailable("the camera is not connected".into())), "the camera it showed is gone");
    }

    #[test]
    fn a_camera_plugged_in_while_its_source_waits_to_try_again_opens_at_once() {
        let system = System::new(Vec::new());
        let io = SourceIo::new(MasterClock::new());
        let t = Instant::now();
        let before = opened().len();
        let mut c = Instance::<Scripted>::new(&settings(), t);
        assert_eq!(opened().len(), before, "a saved camera that is not plugged in is not opened");
        assert_eq!(c.produce_at(&io, t), Err(PluginError::Unavailable("the camera is not connected".into())));
        // Another camera changes nothing for this source.
        system.plug(vec![CameraInfo { name: "Room".into(), id: "2".into() }]);
        assert_eq!(c.produce_at(&io, t), Err(PluginError::Unavailable("the camera is not connected".into())));
        assert_eq!(opened().len(), before);
        system.plug(vec![desk()]);
        script(vec![Ok(())]);
        c.produce_at(&io, t + Duration::from_millis(1)).unwrap();
        assert_eq!(opened().len(), before + 1);
        assert!(matches!(io.video.take(), Some(VideoContent::Frame(_))));
    }

    #[test]
    fn a_source_whose_camera_notices_stop_opens_the_camera_every_2_seconds() {
        let mut system = System::new(Vec::new());
        let io = SourceIo::new(MasterClock::new());
        let t = Instant::now();
        let mut c = Instance::<Scripted>::new(&settings(), t);
        let before = opened().len();
        system.stop();
        assert!(c.produce_at(&io, t).is_err());
        assert_eq!(opened().len(), before, "not before the next try");
        script(vec![Ok(())]);
        c.produce_at(&io, t + REOPEN).unwrap();
        assert_eq!(opened().len(), before + 1, "tried again on the timer, as where the system sends no notices");
    }

    #[test]
    fn the_lists_stamp_reads_a_known_list_without_asking_the_system() {
        let system = System::new(vec![desk()]);
        let asked = || -> Vec<CameraInfo> { panic!("the system was asked") };
        assert_eq!(watched_stamp(Some(&system.watch), asked), stamp(&[desk()]));
        system.plug(Vec::new());
        assert_eq!(watched_stamp(Some(&system.watch), asked), stamp(&[]), "unplugged");
        assert_eq!(watched_stamp(None, || vec![desk()]), stamp(&[desk()]), "without a watch the system is asked");
    }

    #[test]
    fn a_hostile_format_setting_opens_the_default_format() {
        let io = SourceIo::new(MasterClock::new());
        let c = Instance::<Scripted>::new(&Settings::new().with("format", "99999999999x0@-1:XX"), Instant::now());
        assert_eq!(opened().last(), Some(&(String::new(), None)));
        assert_eq!(c.pace(), Pace::Every(Duration::from_secs_f64(1.0 / 60.0)));
        let _ = io;
    }

    #[test]
    fn the_real_source_starts_without_a_camera() {
        let io = SourceIo::new(MasterClock::new());
        let mut i = CameraSource.create(&CameraSource.schema().defaults(), &io).unwrap();
        // Build machines have no camera: produce reports an error instead of a frame.
        if i.produce(&io).is_ok() {
            let _ = io.video.size();
        }
    }
}
