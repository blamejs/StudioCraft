use std::cell::RefCell;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use studiocraft_media::clock::MasterClock;
use studiocraft_media::video::{PixelFormat, VideoFrame};
use studiocraft_model::{FieldKind, Settings};
use studiocraft_plugin::{Plugin, PluginError, SourceIo, VideoContent};

use super::{Staged, StagedWindows, WARM};
use crate::capture::{Grab, Grabbed};

thread_local! {
    /// The windows that exist: each one's picture value, `None` once it closed.
    static WORLD: RefCell<HashMap<String, Option<u8>>> = RefCell::new(HashMap::new());
    /// How often each window was captured.
    static GRABS: RefCell<HashMap<String, usize>> = RefCell::new(HashMap::new());
}

fn window(name: &str, value: Option<u8>) {
    WORLD.with(|w| w.borrow_mut().insert(name.to_string(), value));
}

fn grabs(name: &str) -> usize {
    GRABS.with(|g| g.borrow().get(name).copied().unwrap_or(0))
}

/// The value of a window whose capture waits for a choice in the system picker.
const WAITING: u8 = 255;
/// The value of a window whose capture the person ended in the system picker.
const STOPPED: u8 = 254;

/// A window whose picture is a 4 x 2 frame filled with its value.
struct Fake(String);

impl Grab for Fake {
    fn open(target: &str) -> Result<Self, PluginError> {
        match WORLD.with(|w| w.borrow().get(target).copied()) {
            Some(Some(_)) => Ok(Fake(target.to_string())),
            _ => Err(PluginError::Unavailable(format!("no window {target}"))),
        }
    }
    fn grab(&mut self, frame: &mut VideoFrame) -> Grabbed {
        GRABS.with(|g| *g.borrow_mut().entry(self.0.clone()).or_insert(0) += 1);
        match WORLD.with(|w| w.borrow().get(&self.0).copied().flatten()) {
            Some(WAITING) => Grabbed::Waiting,
            Some(STOPPED) => Grabbed::Stopped(PluginError::Unavailable("sharing stopped".into())),
            Some(v) => {
                frame.reshape(4, 2, PixelFormat::Rgba8);
                frame.data.fill(v);
                Grabbed::Picture
            }
            None => Grabbed::Lost(PluginError::Failed("the window closed".into())),
        }
    }
}

fn staging(a: &str, b: &str) -> Settings {
    Settings::new().with("window1", a).with("window2", b).with("presented", 1).with("placeholder_color", "#010203ff")
}

fn io() -> SourceIo {
    SourceIo::new(MasterClock::new())
}

/// The value of the picture just published, `None` for anything else.
fn shown(io: &SourceIo) -> Option<u8> {
    match io.video.take() {
        Some(VideoContent::Frame(f)) => f.data.first().copied(),
        _ => None,
    }
}

#[test]
fn a_swap_shows_the_other_window_on_the_next_frame() {
    window("sw-a", Some(10));
    window("sw-b", Some(20));
    let io = io();
    let t = Instant::now();
    let mut s = Staged::<Fake>::new(&staging("sw-a", "sw-b"), t);
    s.produce_at(&io, t).unwrap();
    assert_eq!(shown(&io), Some(10));
    // The host produces a frame straight after each update.
    s.update_at(&staging("sw-a", "sw-b").with("presented", 2), &io, t);
    assert!(io.video.take().is_none(), "the update itself captures nothing");
    s.produce_at(&io, t).unwrap();
    assert_eq!(shown(&io), Some(20), "the swap shows the new window on the next frame");
    assert_eq!(grabs("sw-b"), 2, "one capture when it was staged, one for the swap");
    s.update_at(&staging("sw-a", "sw-b").with("presented", 1), &io, t);
    s.produce_at(&io, t).unwrap();
    assert_eq!(shown(&io), Some(10));
}

#[test]
fn a_staged_window_that_is_not_presented_is_captured_once_a_second() {
    window("warm-a", Some(1));
    window("warm-b", Some(2));
    let io = io();
    let t = Instant::now();
    let mut s = Staged::<Fake>::new(&staging("warm-a", "warm-b"), t);
    for ms in (0..1000).step_by(100) {
        s.produce_at(&io, t + Duration::from_millis(ms)).unwrap();
    }
    assert_eq!(grabs("warm-a"), 10, "the presented window is captured at every frame");
    assert_eq!(grabs("warm-b"), 1);
    s.produce_at(&io, t + WARM).unwrap();
    assert_eq!(grabs("warm-b"), 2);
    // A swap back to a window shows its own last picture when it cannot be captured now.
    window("warm-b", None);
    let hold = staging("warm-a", "warm-b").with("placeholder", "hold");
    s.update_at(&hold, &io, t + WARM);
    s.update_at(&hold.clone().with("presented", 2), &io, t + WARM);
    assert!(s.produce_at(&io, t + WARM).is_err());
    assert_eq!(shown(&io), Some(2), "the window's last picture, not the window shown before");
}

#[test]
fn every_background_window_is_captured_once_a_second_at_the_lowest_rate() {
    for (i, name) in ["slow-1", "slow-2", "slow-3", "slow-4", "slow-5", "slow-6"].iter().enumerate() {
        window(name, Some(u8::try_from(i).unwrap_or(0) + 1));
    }
    let io = io();
    let t = Instant::now();
    let all = Settings::new()
        .with("window1", "slow-1")
        .with("window2", "slow-2")
        .with("window3", "slow-3")
        .with("window4", "slow-4")
        .with("window5", "slow-5")
        .with("window6", "slow-6")
        .with("presented", 1);
    let mut s = Staged::<Fake>::new(&all, t);
    // One frame a second.
    s.produce_at(&io, t).unwrap();
    s.produce_at(&io, t + WARM).unwrap();
    for name in ["slow-2", "slow-3", "slow-4", "slow-5", "slow-6"] {
        assert_eq!(grabs(name), 2, "{name}");
    }
}

#[test]
fn a_capture_the_person_ended_waits_for_a_settings_change() {
    window("stop-a", Some(4));
    window("stop-b", Some(STOPPED));
    let io = io();
    let t = Instant::now();
    let mut s = Staged::<Fake>::new(&staging("stop-a", "stop-b"), t);
    s.produce_at(&io, t).unwrap();
    assert_eq!(grabs("stop-b"), 1);
    window("stop-b", Some(8));
    s.produce_at(&io, t + Duration::from_secs(10)).unwrap();
    assert_eq!(grabs("stop-b"), 1, "an ended capture is not opened again on its own");
    s.update_at(&staging("stop-a", "stop-b").with("fps", 10), &io, t + Duration::from_secs(10));
    s.produce_at(&io, t + Duration::from_secs(10)).unwrap();
    assert_eq!(grabs("stop-b"), 2, "a settings change opens it again");
}

#[test]
fn every_window_keeps_its_last_picture_after_it_closes() {
    window("keep-a", Some(11));
    window("keep-b", Some(22));
    let io = io();
    let t = Instant::now();
    let hold = staging("keep-a", "keep-b").with("placeholder", "hold");
    let mut s = Staged::<Fake>::new(&hold, t);
    s.produce_at(&io, t).unwrap();
    assert_eq!(shown(&io), Some(11));
    // The window in the background closes; its next background capture fails.
    window("keep-b", None);
    s.produce_at(&io, t + WARM).unwrap();
    s.update_at(&hold.clone().with("presented", 2), &io, t + WARM);
    assert!(s.produce_at(&io, t + WARM).is_err());
    assert_eq!(shown(&io), Some(22), "the background window's last picture");
    // The window presented before closes too; its picture was kept while it was presented.
    window("keep-a", None);
    s.update_at(&hold.clone().with("presented", 1), &io, t + WARM);
    assert!(s.produce_at(&io, t + WARM).is_err());
    assert_eq!(shown(&io), Some(11), "the presented window's last picture");
}

#[test]
fn a_swap_to_a_capture_that_waits_takes_the_window_before_down() {
    window("wait-a", Some(3));
    window("wait-b", Some(WAITING));
    let io = io();
    let t = Instant::now();
    let mut s = Staged::<Fake>::new(&staging("wait-a", "wait-b"), t);
    s.produce_at(&io, t).unwrap();
    assert_eq!(shown(&io), Some(3));
    s.update_at(&staging("wait-a", "wait-b").with("presented", 2), &io, t);
    let waiting = Err(PluginError::Unavailable("waiting for a choice in the system picker".into()));
    assert_eq!(s.produce_at(&io, t), waiting);
    assert!(matches!(io.video.take(), Some(VideoContent::Solid { rgba: [1, 2, 3, 255], .. })));
    assert_eq!(s.produce_at(&io, t), waiting);
    assert!(io.video.take().is_none(), "the placeholder is published once");
}

#[test]
fn a_closed_presented_window_shows_the_placeholder_color_once() {
    window("color-a", Some(5));
    let io = io();
    let t = Instant::now();
    let mut s = Staged::<Fake>::new(&staging("color-a", ""), t);
    s.produce_at(&io, t).unwrap();
    assert_eq!(shown(&io), Some(5));
    window("color-a", None);
    assert!(s.produce_at(&io, t).is_err());
    assert!(matches!(io.video.take(), Some(VideoContent::Solid { rgba: [1, 2, 3, 255], width: 4, height: 2 })), "the color takes the picture's size");
    assert!(s.produce_at(&io, t + Duration::from_millis(100)).is_err());
    assert!(io.video.take().is_none(), "the color is published once");
    // The window opens again after the reopen wait and is shown.
    window("color-a", Some(6));
    s.produce_at(&io, t + Duration::from_secs(3)).unwrap();
    assert_eq!(shown(&io), Some(6));
}

#[test]
fn hold_keeps_the_last_picture_of_a_closed_window() {
    window("hold-a", Some(7));
    let io = io();
    let t = Instant::now();
    let mut s = Staged::<Fake>::new(&staging("hold-a", "").with("placeholder", "hold"), t);
    s.produce_at(&io, t).unwrap();
    assert_eq!(shown(&io), Some(7));
    window("hold-a", None);
    assert!(s.produce_at(&io, t).is_err());
    assert!(io.video.take().is_none(), "nothing replaces the last picture");
}

#[test]
fn a_swap_to_a_missing_window_never_shows_the_window_before() {
    window("miss-a", Some(9));
    let io = io();
    let t = Instant::now();
    let hold = staging("miss-a", "miss-nowhere").with("placeholder", "hold");
    let mut s = Staged::<Fake>::new(&hold, t);
    s.produce_at(&io, t).unwrap();
    assert_eq!(shown(&io), Some(9));
    s.update_at(&hold.clone().with("presented", 2), &io, t);
    assert!(s.produce_at(&io, t).is_err());
    assert!(matches!(io.video.take(), Some(VideoContent::Empty)), "the window shown before is taken down");
    s.update_at(&hold.clone().with("presented", 2).with("placeholder", "color"), &io, t);
    assert!(matches!(io.video.take(), Some(VideoContent::Solid { rgba: [1, 2, 3, 255], .. })), "a new placeholder goes up at once");
}

#[test]
fn a_window_that_cannot_be_opened_reports_why_at_every_frame() {
    let io = io();
    let t = Instant::now();
    let mut s = Staged::<Fake>::new(&staging("why-nowhere", ""), t);
    let why = Err(PluginError::Unavailable("no window why-nowhere".into()));
    assert_eq!(s.produce_at(&io, t), why);
    assert_eq!(s.produce_at(&io, t + Duration::from_millis(100)), why, "between attempts to open it");
    window("why-closing", Some(1));
    let mut s = Staged::<Fake>::new(&staging("why-closing", ""), t);
    s.produce_at(&io, t).unwrap();
    window("why-closing", None);
    let closed = Err(PluginError::Failed("the window closed".into()));
    assert_eq!(s.produce_at(&io, t), closed);
    assert_eq!(s.produce_at(&io, t + Duration::from_millis(100)), closed, "between attempts to open it again");
}

#[test]
fn an_empty_presented_slot_shows_the_placeholder() {
    let io = io();
    let t = Instant::now();
    let mut s = Staged::<Fake>::new(&staging("", "").with("presented", 3), t);
    assert_eq!(s.produce_at(&io, t), Err(PluginError::Unavailable("no window is staged in the presented slot".into())));
    assert!(matches!(io.video.take(), Some(VideoContent::Solid { rgba: [1, 2, 3, 255], width: 1920, height: 1080 })));
    let mut s = Staged::<Fake>::new(&staging("", "").with("placeholder", "hold"), t);
    assert!(s.produce_at(&io, t).is_err());
    assert!(matches!(io.video.take(), Some(VideoContent::Empty)), "nothing was shown, so nothing is kept");
}

#[test]
fn a_staged_window_that_is_not_open_stays_in_its_slot() {
    let saved = "gone-app.exe\u{1f}A window that closed 41c7";
    let schema = StagedWindows.schema_for(&Settings::new().with("window2", saved));
    let options = |key: &str| match schema.fields.iter().find(|f| f.key == key).map(|f| &f.kind) {
        Some(FieldKind::Choice { options }) => options.clone(),
        other => panic!("{key}: {other:?}"),
    };
    assert!(options("window2").iter().any(|o| o.value == saved && o.label == "[gone-app.exe] A window that closed 41c7"));
    assert!(!options("window1").iter().any(|o| o.value == saved));
    assert_eq!(options("window1").first().map(|o| o.label.as_str()), Some("None"));
    let defaults = StagedWindows.schema().defaults();
    assert_eq!(defaults.i64("presented"), Some(1));
    assert_eq!(defaults.str("placeholder"), Some("color"));
}
