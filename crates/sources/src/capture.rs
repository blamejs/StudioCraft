//! Display and window capture.

use std::time::{Duration, Instant};

use studiocraft_model::{ChoiceOption, Field, FieldKind, Settings, SettingsSchema};
use studiocraft_platform::PlatformError;
use studiocraft_platform::display::{self, DisplayCapture as Display, DisplayInfo, SharedGrab};
use studiocraft_platform::window::{self, WindowCapture as Window};
use studiocraft_plugin::{Pace, Plugin, PluginError, Source, SourceCaps, SourceInstance, SourceIo, VideoContent};

use crate::uint;

/// How long a lost display or window waits before it is looked for again.
pub(crate) const REOPEN: Duration = Duration::from_secs(2);

/// Whether the desktop's own picker chooses the display or window (Linux).
const PICKER: bool = cfg!(target_os = "linux");

const CHOOSE_IN_PICKER: &str = "Choose in the system picker";

pub(crate) fn rate_field() -> Field {
    Field::new("fps", "Capture rate (frames per second)", FieldKind::Int { min: 1, max: 60 }, 30)
}

/// A display's label in lists: `name: width x height`.
pub(crate) fn display_label(d: &DisplayInfo) -> String {
    format!("{}: {} x {}", d.name, d.width, d.height)
}

pub(crate) fn period(s: &Settings) -> Duration {
    Duration::from_secs_f64(1.0 / f64::from(uint(s, "fps", 30, 1, 60)))
}

/// What a grab gave.
pub(crate) enum Grabbed {
    Picture,
    /// The capture runs and the picture has not changed; the last one stays on screen.
    Unchanged,
    /// The capture waits for a choice in the system picker.
    Waiting,
    /// The person ended the capture; it is not opened again until its settings change.
    Stopped(PluginError),
    /// The target was lost; it is looked for again.
    Lost(PluginError),
}

impl From<Result<(), PlatformError>> for Grabbed {
    fn from(r: Result<(), PlatformError>) -> Self {
        match r {
            Ok(()) => Grabbed::Picture,
            Err(PlatformError::Unchanged) => Grabbed::Unchanged,
            Err(PlatformError::Waiting) => Grabbed::Waiting,
            Err(PlatformError::Stopped(m)) => Grabbed::Stopped(PluginError::Unavailable(m)),
            Err(e) => Grabbed::Lost(PluginError::Failed(e.to_string())),
        }
    }
}

/// What the captures do with their OS capture object.
pub(crate) trait Grab {
    fn open(target: &str) -> Result<Self, PluginError>
    where
        Self: Sized;
    fn grab(&mut self, frame: &mut studiocraft_media::video::VideoFrame) -> Grabbed;
    /// The newest picture in a texture shared with the compositor, for a capture that runs on
    /// the GPU. [`SharedGrab::Memory`] (the default) means [`Grab::grab`] copies it into memory.
    fn grab_shared(&mut self) -> SharedGrab {
        SharedGrab::Memory
    }
}

impl Grab for Display {
    fn open(target: &str) -> Result<Self, PluginError> {
        let mut d = Display::open(target).map_err(|e| PluginError::Unavailable(e.to_string()))?;
        d.share_on_gpu();
        Ok(d)
    }
    fn grab(&mut self, frame: &mut studiocraft_media::video::VideoFrame) -> Grabbed {
        Display::grab(self, frame).into()
    }
    fn grab_shared(&mut self) -> SharedGrab {
        Display::grab_shared(self)
    }
}

impl Grab for Window {
    fn open(target: &str) -> Result<Self, PluginError> {
        let mut w = Window::open(target).map_err(|e| PluginError::Unavailable(e.to_string()))?;
        w.share_on_gpu();
        Ok(w)
    }
    fn grab(&mut self, frame: &mut studiocraft_media::video::VideoFrame) -> Grabbed {
        Window::grab(self, frame).into()
    }
    fn grab_shared(&mut self) -> SharedGrab {
        Window::grab_shared(self)
    }
}

/// A capture that reopens its target when it is lost.
struct Capture<G: Grab> {
    key: &'static str,
    target: String,
    period: Duration,
    cap: Option<G>,
    retry_at: Instant,
    /// Why the person ended the capture, until its settings change.
    stopped: Option<PluginError>,
}

impl<G: Grab> Capture<G> {
    fn new(key: &'static str, s: &Settings) -> Self {
        let target = s.str(key).unwrap_or_default().to_string();
        let cap = G::open(&target).ok();
        Capture { key, target, period: period(s), cap, retry_at: Instant::now() + REOPEN, stopped: None }
    }
}

impl<G: Grab> SourceInstance for Capture<G> {
    fn pace(&self) -> Pace {
        Pace::Every(self.period)
    }
    fn produce(&mut self, io: &SourceIo) -> Result<(), PluginError> {
        if let Some(e) = &self.stopped {
            return Err(e.clone());
        }
        if self.cap.is_none() {
            if Instant::now() < self.retry_at {
                return Err(PluginError::Unavailable("waiting for the capture target".into()));
            }
            self.retry_at = Instant::now() + REOPEN;
            match G::open(&self.target) {
                Ok(c) => self.cap = Some(c),
                Err(e) => {
                    io.video.publish(VideoContent::Empty);
                    return Err(e);
                }
            }
        }
        let Some(cap) = self.cap.as_mut() else { return Ok(()) };
        match cap.grab_shared() {
            SharedGrab::Frame(f) => {
                io.video.publish(VideoContent::Shared(f));
                return Ok(());
            }
            SharedGrab::Unchanged => return Ok(()),
            SharedGrab::Memory => {}
        }
        let mut frame = io.video.frame();
        match cap.grab(&mut frame) {
            Grabbed::Picture => {
                frame.pts = io.clock.now();
                io.video.publish(VideoContent::Frame(frame));
                Ok(())
            }
            Grabbed::Unchanged => Ok(()),
            Grabbed::Waiting => Err(PluginError::Unavailable("waiting for a choice in the system picker".into())),
            Grabbed::Stopped(e) => {
                self.cap = None;
                self.stopped = Some(e.clone());
                io.video.publish(VideoContent::Empty);
                Err(e)
            }
            Grabbed::Lost(e) => {
                self.cap = None;
                self.retry_at = Instant::now() + REOPEN;
                // The last picture of a closed window or a lost display is not shown.
                io.video.publish(VideoContent::Empty);
                Err(e)
            }
        }
    }
    fn update(&mut self, s: &Settings, io: &SourceIo) -> Result<(), PluginError> {
        self.period = period(s);
        let target = s.str(self.key).unwrap_or_default();
        if target != self.target || self.stopped.is_some() {
            self.target = target.to_string();
            self.stopped = None;
            self.cap = G::open(&self.target).ok();
            // The picture of the target captured before is not shown as the new target's.
            io.video.publish(VideoContent::Empty);
        }
        Ok(())
    }
}

pub struct DisplayCapture;

impl Plugin for DisplayCapture {
    fn id(&self) -> &'static str {
        "display_capture"
    }
    fn name(&self) -> &'static str {
        "Display capture"
    }
    fn schema(&self) -> SettingsSchema {
        let mut options = vec![ChoiceOption::new("", if PICKER { CHOOSE_IN_PICKER } else { "Primary display" })];
        // Where the picker chooses the display, the listed displays are not offered.
        if !PICKER {
            options.extend(display::displays().iter().map(|d| ChoiceOption::new(d.id.clone(), display_label(d))));
        }
        SettingsSchema::new(vec![Field::new("display", "Display", FieldKind::Choice { options }, ""), rate_field()])
    }
}

impl Source for DisplayCapture {
    fn caps(&self) -> SourceCaps {
        SourceCaps { video: true, audio: false }
    }
    fn create(&self, settings: &Settings, _io: &SourceIo) -> Result<Box<dyn SourceInstance>, PluginError> {
        Ok(Box::new(Capture::<Display>::new("display", settings)))
    }
}

pub struct WindowCapture;

impl Plugin for WindowCapture {
    fn id(&self) -> &'static str {
        "window_capture"
    }
    fn name(&self) -> &'static str {
        "Window capture"
    }
    fn schema(&self) -> SettingsSchema {
        let mut options: Vec<ChoiceOption> = Vec::new();
        if PICKER {
            options.push(ChoiceOption::new("", CHOOSE_IN_PICKER));
        }
        options.extend(window::windows().into_iter().map(|w| ChoiceOption::new(w.key(), w.label())));
        SettingsSchema::new(vec![Field::new("window", "Window", FieldKind::Choice { options }, ""), rate_field()])
    }
}

impl Source for WindowCapture {
    fn caps(&self) -> SourceCaps {
        SourceCaps { video: true, audio: false }
    }
    fn create(&self, settings: &Settings, _io: &SourceIo) -> Result<Box<dyn SourceInstance>, PluginError> {
        Ok(Box::new(Capture::<Window>::new("window", settings)))
    }
}

#[cfg(test)]
mod tests {
    use studiocraft_media::clock::MasterClock;

    use super::*;

    #[test]
    fn display_capture_produces_frames_where_a_display_exists() {
        let io = SourceIo::new(MasterClock::new());
        let mut i = DisplayCapture.create(&DisplayCapture.schema().defaults(), &io).unwrap();
        assert_eq!(i.pace(), Pace::Every(Duration::from_secs_f64(1.0 / 30.0)));
        // Build machines without a display report an error instead of a frame.
        if i.produce(&io).is_ok() {
            assert!(io.video.size().is_some());
        }
    }

    /// A capture target that gives one picture and is then lost.
    struct LostAfterOne {
        grabbed: bool,
    }

    impl Grab for LostAfterOne {
        fn open(_target: &str) -> Result<Self, PluginError> {
            Ok(LostAfterOne { grabbed: false })
        }
        fn grab(&mut self, frame: &mut studiocraft_media::video::VideoFrame) -> Grabbed {
            if self.grabbed {
                return Grabbed::Lost(PluginError::Failed("the window closed".into()));
            }
            self.grabbed = true;
            frame.reshape(8, 4, studiocraft_media::video::PixelFormat::Rgba8);
            Grabbed::Picture
        }
    }

    /// A capture through a system picker: it waits, gives one picture, keeps it while nothing
    /// changes, and then ends as `END` says. Counts how often it was opened.
    struct Picked<const END: u8>(u8);

    thread_local! {
        static OPENED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    fn opened() -> usize {
        OPENED.with(|o| o.get())
    }

    impl<const END: u8> Grab for Picked<END> {
        fn open(_target: &str) -> Result<Self, PluginError> {
            OPENED.with(|o| o.set(o.get() + 1));
            Ok(Picked(0))
        }
        fn grab(&mut self, frame: &mut studiocraft_media::video::VideoFrame) -> Grabbed {
            self.0 += 1;
            let r = match self.0 {
                1 => Err(PlatformError::Waiting),
                2 => {
                    frame.reshape(8, 4, studiocraft_media::video::PixelFormat::Rgba8);
                    Ok(())
                }
                3 => Err(PlatformError::Unchanged),
                _ if END == 0 => Err(PlatformError::Stopped("sharing stopped".into())),
                _ => Err(PlatformError::Failed("the screen capture failed".into())),
            };
            r.into()
        }
    }

    #[test]
    fn a_capture_through_the_picker_waits_keeps_its_picture_and_stays_stopped() {
        let io = SourceIo::new(MasterClock::new());
        let before = opened();
        let mut c = Capture::<Picked<0>>::new("display", &Settings::new());
        assert_eq!(c.produce(&io), Err(PluginError::Unavailable("waiting for a choice in the system picker".into())));
        assert!(io.video.take().is_none(), "nothing is shown while the picker is open");
        c.produce(&io).unwrap();
        assert!(matches!(io.video.take(), Some(VideoContent::Frame(_))));
        c.produce(&io).unwrap();
        assert!(io.video.take().is_none(), "an unchanged picture is not published again");
        assert_eq!(io.video.size(), Some((8, 4)), "the last picture stays on screen");
        let stopped = Err(PluginError::Unavailable("sharing stopped".into()));
        assert_eq!(c.produce(&io), stopped);
        assert!(matches!(io.video.take(), Some(VideoContent::Empty)));
        // A stopped capture does not open the picker again by itself.
        assert_eq!(c.produce(&io), stopped);
        assert_eq!(opened(), before + 1);
        // A change to its settings does.
        c.update(&Settings::new().with("fps", 10), &io).unwrap();
        assert_eq!(opened(), before + 2);
        assert!(c.produce(&io).is_err(), "the new capture waits for the picker");
        assert!(c.stopped.is_none());
    }

    #[test]
    fn a_failed_picker_capture_is_tried_again() {
        let io = SourceIo::new(MasterClock::new());
        let mut c = Capture::<Picked<1>>::new("display", &Settings::new());
        for _ in 0..3 {
            let _ = c.produce(&io);
        }
        assert_eq!(c.produce(&io), Err(PluginError::Failed("the screen capture failed".into())));
        assert!(c.cap.is_none() && c.stopped.is_none(), "a failure is retried after the reopen wait");
    }

    /// A capture on the GPU: a shared picture, an unchanged screen, then the compositor's refusal,
    /// after which pictures are copied through memory.
    struct OnGpu {
        ring: std::sync::Arc<craft_gpu_interop::SharedRing>,
        calls: u64,
    }

    impl Grab for OnGpu {
        fn open(_target: &str) -> Result<Self, PluginError> {
            Ok(OnGpu { ring: craft_gpu_interop::SharedRing::detached(8, 4), calls: 0 })
        }
        fn grab(&mut self, frame: &mut studiocraft_media::video::VideoFrame) -> Grabbed {
            frame.reshape(8, 4, studiocraft_media::video::PixelFormat::Rgba8);
            Grabbed::Picture
        }
        fn grab_shared(&mut self) -> SharedGrab {
            self.calls += 1;
            if self.ring.fallback().is_some() {
                return SharedGrab::Memory;
            }
            match self.calls {
                1 => SharedGrab::Frame(craft_gpu_interop::SharedFrame::new(std::sync::Arc::clone(&self.ring), 0, 1).unwrap()),
                _ => SharedGrab::Unchanged,
            }
        }
    }

    #[test]
    fn a_gpu_capture_publishes_shared_frames_until_the_compositor_refuses_them() {
        let io = SourceIo::new(MasterClock::new());
        let mut c = Capture::<OnGpu>::new("display", &Settings::new());
        c.produce(&io).unwrap();
        let Some(VideoContent::Shared(f)) = io.video.take() else { panic!("a shared picture is published") };
        assert_eq!(io.video.size(), Some((8, 4)));
        c.produce(&io).unwrap();
        assert!(io.video.take().is_none(), "an unchanged screen publishes nothing");
        f.ring().refuse(craft_gpu_interop::Fallback::UnsupportedBackend);
        c.produce(&io).unwrap();
        assert!(matches!(io.video.take(), Some(VideoContent::Frame(_))), "the picture is copied through memory");
    }

    #[test]
    fn a_lost_target_clears_the_picture() {
        let io = SourceIo::new(MasterClock::new());
        let mut c = Capture::<LostAfterOne>::new("window", &Settings::new());
        c.produce(&io).unwrap();
        assert!(matches!(io.video.take(), Some(VideoContent::Frame(_))));
        assert!(c.produce(&io).is_err());
        assert!(matches!(io.video.take(), Some(VideoContent::Empty)), "the last picture is not kept on screen");
        assert_eq!(io.video.size(), None);
    }

    #[test]
    fn a_new_target_clears_the_picture() {
        let io = SourceIo::new(MasterClock::new());
        let mut c = Capture::<LostAfterOne>::new("window", &Settings::new());
        c.produce(&io).unwrap();
        assert!(matches!(io.video.take(), Some(VideoContent::Frame(_))));
        c.update(&Settings::new().with("window", "another"), &io).unwrap();
        assert!(matches!(io.video.take(), Some(VideoContent::Empty)), "the earlier target's picture is not kept on screen");
    }

    #[test]
    fn a_missing_window_is_an_error_not_a_crash() {
        let io = SourceIo::new(MasterClock::new());
        let s = Settings::new().with("window", "no-such-app.exe\u{1f}No such window 8d1f");
        let mut i = WindowCapture.create(&s, &io).unwrap();
        assert!(i.produce(&io).is_err());
    }
}
