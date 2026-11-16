//! Display and window capture.

use std::time::{Duration, Instant};

use studiocraft_model::{ChoiceOption, Field, FieldKind, Settings, SettingsSchema};
use studiocraft_platform::display::{self, DisplayCapture as Display};
use studiocraft_platform::window::{self, WindowCapture as Window};
use studiocraft_plugin::{Pace, Plugin, PluginError, Source, SourceCaps, SourceInstance, SourceIo, VideoContent};

use crate::uint;

/// How long a lost display or window waits before it is looked for again.
const REOPEN: Duration = Duration::from_secs(2);

fn rate_field() -> Field {
    Field::new("fps", "Capture rate (frames per second)", FieldKind::Int { min: 1, max: 60 }, 30)
}

fn period(s: &Settings) -> Duration {
    Duration::from_secs_f64(1.0 / f64::from(uint(s, "fps", 30, 1, 60)))
}

/// What both captures do with their OS capture object.
trait Grab {
    fn open(target: &str) -> Result<Self, PluginError>
    where
        Self: Sized;
    fn grab(&mut self, frame: &mut studiocraft_media::video::VideoFrame) -> Result<(), PluginError>;
}

impl Grab for Display {
    fn open(target: &str) -> Result<Self, PluginError> {
        Display::open(target).map_err(|e| PluginError::Unavailable(e.to_string()))
    }
    fn grab(&mut self, frame: &mut studiocraft_media::video::VideoFrame) -> Result<(), PluginError> {
        Display::grab(self, frame).map_err(|e| PluginError::Failed(e.to_string()))
    }
}

impl Grab for Window {
    fn open(target: &str) -> Result<Self, PluginError> {
        Window::open(target).map_err(|e| PluginError::Unavailable(e.to_string()))
    }
    fn grab(&mut self, frame: &mut studiocraft_media::video::VideoFrame) -> Result<(), PluginError> {
        Window::grab(self, frame).map_err(|e| PluginError::Failed(e.to_string()))
    }
}

/// A capture that reopens its target when it is lost.
struct Capture<G: Grab> {
    key: &'static str,
    target: String,
    period: Duration,
    cap: Option<G>,
    retry_at: Instant,
}

impl<G: Grab> Capture<G> {
    fn new(key: &'static str, s: &Settings) -> Self {
        let target = s.str(key).unwrap_or_default().to_string();
        let cap = G::open(&target).ok();
        Capture { key, target, period: period(s), cap, retry_at: Instant::now() + REOPEN }
    }
}

impl<G: Grab> SourceInstance for Capture<G> {
    fn pace(&self) -> Pace {
        Pace::Every(self.period)
    }
    fn produce(&mut self, io: &SourceIo) -> Result<(), PluginError> {
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
        let mut frame = io.video.frame();
        match cap.grab(&mut frame) {
            Ok(()) => {
                frame.pts = io.clock.now();
                io.video.publish(VideoContent::Frame(frame));
                Ok(())
            }
            Err(e) => {
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
        if target != self.target {
            self.target = target.to_string();
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
        let mut options = vec![ChoiceOption::new("", "Primary display")];
        options.extend(display::displays().into_iter().map(|d| ChoiceOption::new(d.id, format!("{}: {} x {}", d.name, d.width, d.height))));
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
        let options = window::windows().into_iter().map(|w| ChoiceOption::new(w.key(), w.label())).collect();
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
        fn grab(&mut self, frame: &mut studiocraft_media::video::VideoFrame) -> Result<(), PluginError> {
            if self.grabbed {
                return Err(PluginError::Failed("the window closed".into()));
            }
            self.grabbed = true;
            frame.reshape(8, 4, studiocraft_media::video::PixelFormat::Rgba8);
            Ok(())
        }
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
