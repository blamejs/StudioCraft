//! Camera capture.

use std::time::{Duration, Instant};

use studiocraft_media::video::VideoFrame;
use studiocraft_model::{ChoiceOption, Field, FieldKind, Settings, SettingsSchema};
use studiocraft_platform::PlatformError;
use studiocraft_platform::camera::{self, CameraCapture, CameraFormat, CameraInfo, best_match};
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
}

impl Camera for CameraCapture {
    fn open(device: &str, format: Option<CameraFormat>) -> Result<Self, PlatformError> {
        CameraCapture::open(device, format)
    }
    fn grab(&mut self, frame: &mut VideoFrame) -> Result<(), PlatformError> {
        CameraCapture::grab(self, frame)
    }
}

/// A camera that is opened again while it fails.
struct Instance<C: Camera> {
    device: String,
    format: Option<CameraFormat>,
    cap: Option<C>,
    retry_at: Instant,
    /// When the last frame arrived, or when the camera started sending.
    last_frame: Instant,
    /// Why the camera is closed, reported until it opens again.
    error: Option<PluginError>,
}

impl<C: Camera> Instance<C> {
    fn new(s: &Settings, now: Instant) -> Self {
        let mut i = Instance { device: String::new(), format: None, cap: None, retry_at: now, last_frame: now, error: None };
        i.set(s, now);
        i
    }

    /// Take the settings and open the camera they name.
    fn set(&mut self, s: &Settings, now: Instant) {
        self.device = s.str("device").unwrap_or_default().to_string();
        self.format = format_of(s);
        self.retry_at = now + REOPEN;
        self.last_frame = now;
        match C::open(&self.device, self.format) {
            Ok(c) => {
                self.cap = Some(c);
                self.error = None;
            }
            Err(e) => {
                self.cap = None;
                self.error = Some(PluginError::Unavailable(e.to_string()));
            }
        }
    }

    fn lost(&mut self, io: &SourceIo, now: Instant, e: PluginError) -> Result<(), PluginError> {
        self.cap = None;
        self.retry_at = now + REOPEN;
        self.error = Some(e.clone());
        // The last picture of a camera that went away is not kept on screen.
        io.video.publish(VideoContent::Empty);
        Err(e)
    }

    fn produce_at(&mut self, io: &SourceIo, now: Instant) -> Result<(), PluginError> {
        if self.cap.is_none() {
            if now < self.retry_at {
                return Err(self.error.clone().unwrap_or_else(|| PluginError::Unavailable("waiting to open the camera again".into())));
            }
            self.retry_at = now + REOPEN;
            match C::open(&self.device, self.format) {
                Ok(c) => {
                    self.cap = Some(c);
                    self.last_frame = now;
                    self.error = None;
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
        let fps = self.format.map_or(DEFAULT_FPS, |f| f.fps.clamp(1, 240));
        Pace::Every(Duration::from_secs_f64(1.0 / f64::from(fps)))
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
        Some(stamp(&camera::cameras()))
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

    use studiocraft_media::clock::MasterClock;
    use studiocraft_media::video::PixelFormat;
    use studiocraft_platform::camera::CameraPixel;

    use super::*;

    thread_local! {
        /// What the next grabs give, and every open (device, format).
        static SCRIPT: RefCell<Vec<Result<(), PlatformError>>> = const { RefCell::new(Vec::new()) };
        static OPENED: RefCell<Vec<(String, Option<CameraFormat>)>> = const { RefCell::new(Vec::new()) };
        static OPEN_FAILS: RefCell<bool> = const { RefCell::new(false) };
    }

    fn script(steps: Vec<Result<(), PlatformError>>) {
        SCRIPT.with(|s| *s.borrow_mut() = steps.into_iter().rev().collect());
    }

    fn opened() -> Vec<(String, Option<CameraFormat>)> {
        OPENED.with(|o| o.borrow().clone())
    }

    /// A camera that plays the script; an empty script gives unchanged pictures.
    struct Scripted;

    impl Camera for Scripted {
        fn open(device: &str, format: Option<CameraFormat>) -> Result<Self, PlatformError> {
            OPENED.with(|o| o.borrow_mut().push((device.to_string(), format)));
            if OPEN_FAILS.with(|f| *f.borrow()) { Err(PlatformError::Failed("the camera could not be opened (in use)".into())) } else { Ok(Scripted) }
        }
        fn grab(&mut self, frame: &mut VideoFrame) -> Result<(), PlatformError> {
            let step = SCRIPT.with(|s| s.borrow_mut().pop()).unwrap_or(Err(PlatformError::Unchanged));
            if step.is_ok() {
                frame.reshape(4, 2, PixelFormat::Rgba8);
            }
            step
        }
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
