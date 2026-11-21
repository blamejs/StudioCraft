//! The Staged windows source: up to six windows or displays captured at once, one of them
//! presented.
//!
//! Every staged window or display keeps its capture open. The presented one is captured at the
//! source's rate, and each of the others once a second, so a swap shows the new one at once. While
//! the presented window or display cannot be captured (a window closed or is minimized, a display
//! is disconnected, or the slot is empty), the source shows a color or keeps the last picture
//! instead of anything behind it.

use std::time::{Duration, Instant};

use studiocraft_media::video::VideoFrame;
use studiocraft_model::staged::{self, PRESENTED, SLOT_KEYS, SLOTS};
use studiocraft_model::{ChoiceOption, Field, FieldKind, Settings, SettingsSchema};
use studiocraft_platform::PlatformError;
use studiocraft_platform::display::{self, DisplayCapture, DisplayInfo};
use studiocraft_platform::window::{self, WindowCapture as Window, WindowInfo};
use studiocraft_plugin::{Pace, Plugin, PluginError, Source, SourceCaps, SourceInstance, SourceIo, VideoContent};

use crate::capture::{Grab, Grabbed, REOPEN, display_label, period, rate_field};

/// How often a staged window or display that is not presented is captured.
const WARM: Duration = Duration::from_secs(1);

/// Why a staged display cannot be opened.
const NOT_CONNECTED: &str = "the display is not connected";

/// What a slot captures: the window or the display its value names
/// ([`staged::display_id`]).
pub(crate) enum Target<W, D> {
    Window(W),
    Display(D),
}

impl<W: Grab, D: Grab> Grab for Target<W, D> {
    fn open(target: &str) -> Result<Self, PluginError> {
        match staged::display_id(target) {
            Some(id) => D::open(id).map(Target::Display),
            None => W::open(target).map(Target::Window),
        }
    }
    fn grab(&mut self, frame: &mut VideoFrame) -> Grabbed {
        match self {
            Target::Window(w) => w.grab(frame),
            Target::Display(d) => d.grab(frame),
        }
    }
}

/// A staged display. It opens only the display with that id while the display is connected; the
/// Display capture source opens the primary display when its display is missing.
pub(crate) struct Screen(DisplayCapture);

impl Grab for Screen {
    fn open(id: &str) -> Result<Self, PluginError> {
        match DisplayCapture::open_exact(id) {
            Ok(cap) => Ok(Screen(cap)),
            Err(PlatformError::NotFound(_)) => Err(PluginError::Unavailable(NOT_CONNECTED.into())),
            Err(e) => Err(PluginError::Unavailable(e.to_string())),
        }
    }
    fn grab(&mut self, frame: &mut VideoFrame) -> Grabbed {
        self.0.grab(frame).into()
    }
}

/// Whether a display key names a display that can exist: its id is the decimal number the system
/// gives a display. Any window key can name a window.
fn possible(key: &str) -> bool {
    staged::display_id(key).is_none_or(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
}

/// The size of the placeholder color before any picture was shown.
const PLACEHOLDER_SIZE: (u32, u32) = (1920, 1080);

/// The label of each slot's setting.
const SLOT_LABELS: [&str; SLOTS] = ["Window 1", "Window 2", "Window 3", "Window 4", "Window 5", "Window 6"];

/// What the source shows while the presented window or display cannot be captured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Placeholder {
    /// The last picture stays on screen.
    Hold,
    /// A solid color, `[r, g, b, a]`.
    Color([u8; 4]),
}

fn placeholder(s: &Settings) -> Placeholder {
    match s.str("placeholder") {
        Some("hold") => Placeholder::Hold,
        _ => Placeholder::Color(s.color("placeholder_color").unwrap_or([0, 0, 0, 255])),
    }
}

/// One slot: its window or display and that capture.
struct Slot<G> {
    target: String,
    cap: Option<G>,
    retry_at: Instant,
    /// The newest picture: copied at every frame while the slot is presented, captured once a
    /// second while it is not.
    picture: VideoFrame,
    has_picture: bool,
    /// When the slot is next captured while it is not presented.
    warm_at: Instant,
    /// Why the person ended the capture; it is not opened again until the settings change.
    stopped: Option<PluginError>,
    /// Why the capture last failed to open or was lost, reported until it opens again.
    failed: Option<PluginError>,
}

impl<G: Grab> Slot<G> {
    fn new(target: &str, now: Instant) -> Self {
        let (cap, failed) = if target.is_empty() {
            (None, None)
        } else {
            match G::open(target) {
                Ok(cap) => (Some(cap), None),
                Err(e) => (None, Some(e)),
            }
        };
        Slot {
            target: target.to_string(),
            cap,
            retry_at: now + REOPEN,
            picture: VideoFrame::default(),
            has_picture: false,
            warm_at: now,
            stopped: None,
            failed,
        }
    }

    /// Open the capture again when it was lost and the reopen wait is over, or at once when `now_too`.
    fn ensure_open(&mut self, now: Instant, now_too: bool) -> Result<(), PluginError> {
        if self.cap.is_some() {
            return Ok(());
        }
        if let Some(e) = &self.stopped {
            return Err(e.clone());
        }
        if !now_too && now < self.retry_at {
            return Err(self.failed.clone().unwrap_or_else(|| PluginError::Unavailable("waiting for the window".into())));
        }
        self.retry_at = now + REOPEN;
        match G::open(&self.target) {
            Ok(cap) => {
                self.cap = Some(cap);
                self.failed = None;
                Ok(())
            }
            Err(e) => {
                self.failed = Some(e.clone());
                Err(e)
            }
        }
    }

    fn lose(&mut self, now: Instant, e: PluginError) {
        self.cap = None;
        self.retry_at = now + REOPEN;
        self.failed = Some(e);
    }

    /// The person ended the capture (in the system picker): it stays closed until the settings change.
    fn stop(&mut self, e: PluginError) {
        self.cap = None;
        self.stopped = Some(e);
    }

    /// Capture the window or display into the slot's own picture.
    fn warm(&mut self, now: Instant) {
        self.warm_at = now + WARM;
        if self.ensure_open(now, false).is_err() {
            return;
        }
        let Some(cap) = self.cap.as_mut() else { return };
        match cap.grab(&mut self.picture) {
            Grabbed::Picture => self.has_picture = true,
            Grabbed::Unchanged | Grabbed::Waiting => {}
            // The last picture stays, for a swap to the slot while it cannot be captured.
            Grabbed::Lost(e) => self.lose(now, e),
            Grabbed::Stopped(e) => self.stop(e),
        }
    }
}

/// What showing the presented slot gave.
enum Shown {
    Picture,
    /// Nothing new to show; the last picture stays.
    Kept,
    /// The window or display cannot be captured now; the placeholder goes up.
    Placeholder(PluginError),
    Waiting,
}

/// A running Staged windows source.
pub(crate) struct Staged<G: Grab> {
    slots: Vec<Slot<G>>,
    /// The presented slot, 1 to [`SLOTS`].
    presented: usize,
    period: Duration,
    placeholder: Placeholder,
    /// Whether the placeholder is up, so a color is published once and not at every frame.
    showing_placeholder: bool,
    /// The size of the last picture shown, which a placeholder color takes.
    size: (u32, u32),
    /// Whether the next frame is the first or follows a swap, so it never shows the slot shown before.
    fresh: bool,
}

impl<G: Grab> Staged<G> {
    fn new(s: &Settings, now: Instant) -> Self {
        Staged {
            slots: SLOT_KEYS.iter().map(|k| Slot::new(s.str(k).unwrap_or_default(), now)).collect(),
            presented: staged::presented(s),
            period: period(s),
            placeholder: placeholder(s),
            showing_placeholder: false,
            size: PLACEHOLDER_SIZE,
            fresh: true,
        }
    }

    /// Show the presented slot. `swap` is set when the presented slot just changed: then a window
    /// or display that cannot be captured shows its own last picture or nothing, never the picture
    /// of the slot shown before.
    fn show(&mut self, io: &SourceIo, now: Instant, swap: bool) -> Result<(), PluginError> {
        let shown = match self.slots.get_mut(self.presented.saturating_sub(1)) {
            Some(slot) if !slot.target.is_empty() => match slot.ensure_open(now, swap) {
                Err(e) => Shown::Placeholder(e),
                Ok(()) => match slot.cap.as_mut() {
                    None => Shown::Kept,
                    Some(cap) => {
                        let mut frame = io.video.frame();
                        match cap.grab(&mut frame) {
                            Grabbed::Picture => {
                                frame.pts = io.clock.now();
                                self.size = (frame.width, frame.height);
                                slot.warm_at = now + WARM;
                                slot.has_picture =
                                    slot.picture.reshape(frame.width, frame.height, frame.format) && slot.picture.data.len() == frame.data.len();
                                if slot.has_picture {
                                    slot.picture.data.copy_from_slice(&frame.data);
                                }
                                io.video.publish(VideoContent::Frame(frame));
                                Shown::Picture
                            }
                            Grabbed::Unchanged => Shown::Kept,
                            Grabbed::Waiting => Shown::Waiting,
                            Grabbed::Lost(e) => {
                                slot.lose(now, e.clone());
                                Shown::Placeholder(e)
                            }
                            Grabbed::Stopped(e) => {
                                slot.stop(e.clone());
                                Shown::Placeholder(e)
                            }
                        }
                    }
                },
            },
            _ => Shown::Placeholder(PluginError::Unavailable("nothing is staged in the presented slot".into())),
        };
        match shown {
            Shown::Picture => {
                self.showing_placeholder = false;
                Ok(())
            }
            Shown::Kept => {
                if swap {
                    self.show_cached(io);
                }
                Ok(())
            }
            Shown::Waiting => {
                if swap {
                    self.show_placeholder(io, true);
                }
                Err(PluginError::Unavailable("waiting for a choice in the system picker".into()))
            }
            Shown::Placeholder(e) => {
                self.show_placeholder(io, swap);
                Err(e)
            }
        }
    }

    /// The presented slot's last picture, or nothing when it has none.
    fn show_cached(&mut self, io: &SourceIo) {
        let Some(slot) = self.slots.get(self.presented.saturating_sub(1)) else { return };
        if !slot.has_picture || !slot.picture.is_consistent() {
            io.video.publish(VideoContent::Empty);
            return;
        }
        let mut frame = io.video.frame();
        if frame.reshape(slot.picture.width, slot.picture.height, slot.picture.format) && frame.data.len() == slot.picture.data.len() {
            frame.data.copy_from_slice(&slot.picture.data);
            frame.pts = io.clock.now();
            self.size = (frame.width, frame.height);
            io.video.publish(VideoContent::Frame(frame));
        } else {
            io.video.publish(VideoContent::Empty);
        }
    }

    fn show_placeholder(&mut self, io: &SourceIo, swap: bool) {
        match self.placeholder {
            Placeholder::Color(rgba) => {
                if swap || !self.showing_placeholder {
                    let (width, height) = self.size;
                    io.video.publish(VideoContent::Solid { rgba, width, height });
                }
            }
            // While the slot is presented, its last picture stays; after a swap, only its own.
            Placeholder::Hold => {
                if swap {
                    self.show_cached(io);
                }
            }
        }
        self.showing_placeholder = true;
    }

    /// Capture every staged window or display that is not presented and is due.
    fn warm_due(&mut self, now: Instant) {
        let presented = self.presented.saturating_sub(1);
        for (i, slot) in self.slots.iter_mut().enumerate() {
            if i != presented && !slot.target.is_empty() && slot.warm_at <= now {
                slot.warm(now);
            }
        }
    }

    fn produce_at(&mut self, io: &SourceIo, now: Instant) -> Result<(), PluginError> {
        let swap = std::mem::take(&mut self.fresh);
        let result = self.show(io, now, swap);
        self.warm_due(now);
        result
    }

    fn update_at(&mut self, s: &Settings, io: &SourceIo, now: Instant) {
        self.period = period(s);
        let old_placeholder = std::mem::replace(&mut self.placeholder, placeholder(s));
        let mut swap = false;
        for (n, (slot, key)) in self.slots.iter_mut().zip(SLOT_KEYS).enumerate() {
            let target = s.str(key).unwrap_or_default();
            if slot.target != target {
                *slot = Slot::new(target, now);
                swap |= n + 1 == self.presented;
            } else if slot.stopped.take().is_some() {
                // A change to the settings opens a capture the person ended again.
                slot.warm_at = now;
                slot.retry_at = now;
                swap |= n + 1 == self.presented;
            }
        }
        let presented = staged::presented(s);
        if presented != self.presented {
            self.presented = presented;
            swap = true;
        }
        if swap {
            // The host produces a frame straight after an update, and that frame shows the swap.
            self.fresh = true;
        } else if self.showing_placeholder && self.placeholder != old_placeholder {
            self.show_placeholder(io, true);
        }
    }
}

impl<G: Grab> SourceInstance for Staged<G> {
    fn pace(&self) -> Pace {
        Pace::Every(self.period)
    }
    fn produce(&mut self, io: &SourceIo) -> Result<(), PluginError> {
        self.produce_at(io, Instant::now())
    }
    fn update(&mut self, s: &Settings, io: &SourceIo) -> Result<(), PluginError> {
        self.update_at(s, io, Instant::now());
        Ok(())
    }
}

pub struct StagedWindows;

impl Plugin for StagedWindows {
    fn id(&self) -> &'static str {
        staged::KIND
    }
    fn name(&self) -> &'static str {
        "Staged windows"
    }
    fn schema(&self) -> SettingsSchema {
        self.schema_for(&Settings::new())
    }
    /// The open windows, the connected displays, and each staged window or display that is not
    /// open or connected now, so its slot keeps it. A display key whose id is not a number is not
    /// offered, so it is an error to supply one.
    fn schema_for(&self, settings: &Settings) -> SettingsSchema {
        let listed = window::windows();
        let screens = display::displays();
        let mut fields: Vec<Field> = SLOT_KEYS
            .iter()
            .zip(SLOT_LABELS)
            .map(|(key, label)| {
                let mut options = vec![ChoiceOption::new("", "None")];
                options.extend(listed.iter().map(|w| ChoiceOption::new(w.key(), w.label())));
                options.extend(screens.iter().map(|d| ChoiceOption::new(staged::display_key(&d.id), display_label(d))));
                if let Some(saved) = settings.str(key).filter(|k| !k.is_empty() && possible(k) && !options.iter().any(|o| o.value == *k)) {
                    let label = match staged::display_id(saved) {
                        Some(id) => id.to_string(),
                        None => WindowInfo::parse_key(saved).label(),
                    };
                    options.push(ChoiceOption::new(saved, label));
                }
                Field::new(key, label, FieldKind::Choice { options }, "")
            })
            .collect();
        fields.push(Field::new(PRESENTED, "Presented window", FieldKind::Int { min: 1, max: SLOTS as i64 }, 1));
        let shows = vec![ChoiceOption::new("color", "Show a color"), ChoiceOption::new("hold", "Keep the last picture")];
        fields.push(Field::new("placeholder", "While the presented window cannot be shown", FieldKind::Choice { options: shows }, "color"));
        fields.push(Field::new("placeholder_color", "Placeholder color", FieldKind::Color, "#000000ff"));
        fields.push(rate_field());
        SettingsSchema::new(fields)
    }
    /// Changes when a display is connected or disconnected.
    fn lists_stamp(&self) -> Option<u64> {
        Some(stamp(&display::displays()))
    }
}

/// A value for a list of displays that changes when the list does.
fn stamp(list: &[DisplayInfo]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for d in list {
        (&d.id, &d.name, d.width, d.height).hash(&mut h);
    }
    h.finish()
}

impl Source for StagedWindows {
    fn caps(&self) -> SourceCaps {
        SourceCaps { video: true, audio: false }
    }
    fn create(&self, settings: &Settings, _io: &SourceIo) -> Result<Box<dyn SourceInstance>, PluginError> {
        Ok(Box::new(Staged::<Target<Window, Screen>>::new(settings, Instant::now())))
    }
}

#[cfg(test)]
mod tests;
