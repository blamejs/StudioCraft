use std::cell::RefCell;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use studiocraft_media::clock::MasterClock;
use studiocraft_media::video::{PixelFormat, VideoFrame};
use studiocraft_model::staged::display_key;
use studiocraft_model::{FieldKind, Settings};
use studiocraft_platform::display::DisplayInfo;
use studiocraft_plugin::{Plugin, PluginError, SourceIo, VideoContent};

use super::{Staged, StagedWindows, Target, WARM, stamp};
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

thread_local! {
    /// The displays that are connected: each one's picture value, `None` once it was disconnected.
    static SCREENS: RefCell<HashMap<String, Option<u8>>> = RefCell::new(HashMap::new());
    /// How often each display was opened and captured.
    static SCREEN_OPENS: RefCell<HashMap<String, usize>> = RefCell::new(HashMap::new());
    static SCREEN_GRABS: RefCell<HashMap<String, usize>> = RefCell::new(HashMap::new());
}

fn screen(id: &str, value: Option<u8>) {
    SCREENS.with(|s| s.borrow_mut().insert(id.to_string(), value));
}

fn screen_opens(id: &str) -> usize {
    SCREEN_OPENS.with(|o| o.borrow().get(id).copied().unwrap_or(0))
}

fn screen_grabs(id: &str) -> usize {
    SCREEN_GRABS.with(|g| g.borrow().get(id).copied().unwrap_or(0))
}

/// A display whose picture is a 2 x 2 frame filled with its value. It opens with the display id,
/// without the mark.
struct FakeScreen(String);

impl Grab for FakeScreen {
    fn open(id: &str) -> Result<Self, PluginError> {
        SCREEN_OPENS.with(|o| *o.borrow_mut().entry(id.to_string()).or_insert(0) += 1);
        match SCREENS.with(|s| s.borrow().get(id).copied()) {
            Some(Some(_)) => Ok(FakeScreen(id.to_string())),
            _ => Err(PluginError::Unavailable(format!("no display {id}"))),
        }
    }
    fn grab(&mut self, frame: &mut VideoFrame) -> Grabbed {
        SCREEN_GRABS.with(|g| *g.borrow_mut().entry(self.0.clone()).or_insert(0) += 1);
        match SCREENS.with(|s| s.borrow().get(&self.0).copied().flatten()) {
            Some(v) => {
                frame.reshape(2, 2, PixelFormat::Rgba8);
                frame.data.fill(v);
                Grabbed::Picture
            }
            None => Grabbed::Lost(PluginError::Failed("the display was disconnected".into())),
        }
    }
}

/// Windows and displays in the slots.
type Both = Target<Fake, FakeScreen>;

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
    // A newer picture of the presented window, within the same second.
    let soon = t + WARM + Duration::from_millis(100);
    window("keep-a", Some(12));
    s.produce_at(&io, soon).unwrap();
    s.update_at(&hold.clone().with("presented", 2), &io, soon);
    assert!(s.produce_at(&io, soon).is_err());
    assert_eq!(shown(&io), Some(22), "the background window's last picture");
    // The window presented before closes too; its picture was kept while it was presented.
    window("keep-a", None);
    s.update_at(&hold.clone().with("presented", 1), &io, soon);
    assert!(s.produce_at(&io, soon).is_err());
    assert_eq!(shown(&io), Some(12), "the presented window's last picture");
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

#[test]
fn windows_and_displays_share_the_slots() {
    window("mix-a", Some(31));
    screen("301", Some(32));
    let io = io();
    let t = Instant::now();
    let s = staging("mix-a", &display_key("301"));
    let mut st = Staged::<Both>::new(&s, t);
    st.produce_at(&io, t).unwrap();
    assert_eq!(shown(&io), Some(31), "the window in slot 1 is presented");
    assert_eq!(screen_opens("301"), 1, "the display in slot 2 is opened as a display");
    assert_eq!(screen_grabs("301"), 1, "and captured in the background");
    assert_eq!(grabs("\u{1e}301"), 0, "a display key is never opened as a window");
}

#[test]
fn a_swap_to_a_display_shows_it_on_the_next_frame_and_back() {
    window("swd-a", Some(41));
    screen("401", Some(42));
    let io = io();
    let t = Instant::now();
    let s = staging("swd-a", &display_key("401"));
    let mut st = Staged::<Both>::new(&s, t);
    st.produce_at(&io, t).unwrap();
    assert_eq!(shown(&io), Some(41));
    st.update_at(&s.clone().with("presented", 2), &io, t);
    st.produce_at(&io, t).unwrap();
    let picture = match io.video.take() {
        Some(VideoContent::Frame(f)) => Some((f.width, f.height, f.data.first().copied())),
        _ => None,
    };
    assert_eq!(picture, Some((2, 2, Some(42))), "the display's picture");
    assert_eq!(screen_grabs("401"), 2, "one capture when it was staged, one for the swap");
    assert_eq!(screen_opens("401"), 1, "the swap does not open the display again");
    st.update_at(&s.clone().with("presented", 1), &io, t);
    st.produce_at(&io, t).unwrap();
    assert_eq!(shown(&io), Some(41));
}

#[test]
fn a_disconnected_display_keeps_its_slot_and_shows_the_placeholder() {
    screen("501", Some(51));
    let io = io();
    let t = Instant::now();
    let s = staging(&display_key("501"), "");
    let mut st = Staged::<Both>::new(&s, t);
    st.produce_at(&io, t).unwrap();
    assert_eq!(shown(&io), Some(51));
    // The display is unplugged.
    screen("501", None);
    let gone = Err(PluginError::Failed("the display was disconnected".into()));
    assert_eq!(st.produce_at(&io, t), gone);
    assert!(matches!(io.video.take(), Some(VideoContent::Solid { rgba: [1, 2, 3, 255], width: 2, height: 2 })), "the color takes the picture's size");
    assert_eq!(st.produce_at(&io, t + Duration::from_millis(100)), gone, "it reports why between attempts");
    assert!(io.video.take().is_none(), "the color is published once");
    // It is looked for again every 2 seconds, and not before.
    assert_eq!(st.produce_at(&io, t + Duration::from_secs(1)), Err(PluginError::Failed("the display was disconnected".into())));
    assert_eq!(screen_opens("501"), 1);
    let not_there = Err(PluginError::Unavailable("no display 501".into()));
    assert_eq!(st.produce_at(&io, t + Duration::from_secs(2)), not_there);
    assert_eq!(screen_opens("501"), 2);
    assert_eq!(st.slots.first().map(|slot| slot.target.as_str()), Some("\u{1e}501"), "the slot keeps its display");
    // Plugged in again, it is shown at the next attempt.
    screen("501", Some(52));
    st.produce_at(&io, t + Duration::from_secs(4)).unwrap();
    assert_eq!(shown(&io), Some(52));
    // A display that was never connected stays in its slot and the placeholder stays up.
    let s = staging("", &display_key("502")).with("presented", 2);
    let mut st = Staged::<Both>::new(&s, t);
    assert_eq!(st.produce_at(&io, t), Err(PluginError::Unavailable("no display 502".into())));
    assert!(matches!(io.video.take(), Some(VideoContent::Solid { rgba: [1, 2, 3, 255], .. })));
}

#[test]
fn a_display_that_is_not_connected_stays_offered_in_its_slot() {
    let saved = display_key("4294967295");
    let schema = StagedWindows.schema_for(&Settings::new().with("window3", saved.clone()));
    let options = |key: &str| match schema.fields.iter().find(|f| f.key == key).map(|f| &f.kind) {
        Some(FieldKind::Choice { options }) => options.clone(),
        other => panic!("{key}: {other:?}"),
    };
    assert!(options("window3").iter().any(|o| o.value == saved && o.label == "4294967295"));
    assert!(!options("window1").iter().any(|o| o.value == saved));
    // A display key that cannot name a display is not offered, so supplying it is an error.
    for bad in [display_key(""), display_key("primary"), display_key("-1")] {
        let s = Settings::new().with("window1", bad.clone());
        let schema = StagedWindows.schema_for(&s);
        assert!(!schema.problems(&s).is_empty(), "{bad:?}");
    }
    let s = Settings::new().with("window1", saved.clone()).with("window2", "an-app.exe\u{1f}Not open");
    assert!(StagedWindows.schema_for(&s).problems(&s).is_empty());
}

#[test]
fn the_lists_stamp_changes_when_a_display_is_connected_or_disconnected() {
    let display = |id: &str| DisplayInfo { id: id.into(), name: "Display".into(), width: 1920, height: 1080, primary: id == "1" };
    let both = stamp(&[display("1"), display("2")]);
    assert_eq!(both, stamp(&[display("1"), display("2")]));
    assert_ne!(both, stamp(&[display("1")]), "one disconnected");
    assert_ne!(stamp(&[]), stamp(&[display("1")]), "the first connected");
    assert_ne!(both, stamp(&[display("1"), display("3")]), "another display of the same name");
    assert!(StagedWindows.lists_stamp().is_some(), "an open Properties window looks for displays");
}

#[test]
fn saved_window_slots_still_open_as_windows() {
    // The slot values a collection saved before displays could be staged.
    window("old-app.exe\u{1f}Old window", Some(61));
    window("A title only", Some(62));
    let opened = || SCREEN_OPENS.with(|o| o.borrow().values().sum::<usize>());
    let before = opened();
    let io = io();
    let t = Instant::now();
    let s = staging("old-app.exe\u{1f}Old window", "A title only");
    let mut st = Staged::<Both>::new(&s, t);
    st.produce_at(&io, t).unwrap();
    assert_eq!(shown(&io), Some(61));
    assert_eq!(grabs("A title only"), 1);
    st.update_at(&s.clone().with("presented", 2), &io, t);
    st.produce_at(&io, t).unwrap();
    assert_eq!(shown(&io), Some(62));
    assert_eq!(opened(), before, "no display was opened");
    let schema = StagedWindows.schema_for(&s);
    assert!(schema.problems(&s).is_empty());
    let labels: Vec<String> = ["window1", "window2"]
        .iter()
        .filter_map(|k| match schema.fields.iter().find(|f| f.key == *k).map(|f| &f.kind) {
            Some(FieldKind::Choice { options }) => options.iter().find(|o| o.value.as_str() == s.str(k)).map(|o| o.label.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(labels, ["[old-app.exe] Old window", "[] A title only"]);
}

#[test]
fn a_background_display_is_captured_once_a_second_into_the_same_picture() {
    window("bgd-a", Some(71));
    screen("701", Some(72));
    let io = io();
    let t = Instant::now();
    let mut st = Staged::<Both>::new(&staging("bgd-a", &display_key("701")), t);
    for ms in (0..1000).step_by(100) {
        st.produce_at(&io, t + Duration::from_millis(ms)).unwrap();
    }
    assert_eq!(grabs("bgd-a"), 10, "the presented window is captured at every frame");
    assert_eq!(screen_grabs("701"), 1);
    let first = st.slots.get(1).map(|slot| (slot.picture.data.as_ptr(), slot.picture.data.capacity()));
    st.produce_at(&io, t + WARM).unwrap();
    assert_eq!(screen_grabs("701"), 2);
    let second = st.slots.get(1).map(|slot| (slot.picture.data.as_ptr(), slot.picture.data.capacity()));
    assert_eq!(first, second, "a background capture reuses the slot's picture");
}
