//! StudioCraft's files in its configuration folder: the preferences (`config.toml`, through
//! craft-settings), the scene collections (`scenes/<name>.json`) and the profiles
//! (`profiles/<name>.json`). Without a folder (tests, `--no-prefs`) everything is kept in memory.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Instant;

use craft_keymap::Overrides;
use craft_settings::{Location, SettingsStore};
use serde::{Deserialize, Serialize};
use studiocraft_model::format::{MAX_FILE_BYTES, read_collection, read_profile, write_collection, write_profile};
use studiocraft_model::{Profile, SceneCollection};
use studiocraft_render::device::Preference;

use crate::i18n::Msg;

/// Which docks are shown, where each docked one sits (under the canvas or in a column beside it),
/// their order, and the sizes set by dragging the docks' edges. A size of 0 is not set and uses the
/// default in [`crate::strip`]; Docks › Reset Docks sets them all back to 0, the order back to
/// Scenes, Sources, Audio Mixer, Application Audio, Health, and every dock back under the canvas.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Docks {
    pub scenes: bool,
    pub sources: bool,
    pub mixer: bool,
    /// The Application Audio dock. It is hidden until Docks › Application Audio shows it.
    pub app_audio: bool,
    /// The Health dock. It is hidden until Docks › Health or the Live layout shows it.
    pub health: bool,
    pub transitions: bool,
    pub controls: bool,
    /// The dock strip's height in points.
    pub height: f32,
    /// The widths of the Scenes, Sources, Audio Mixer, Application Audio and Health docks in
    /// points; the last dock in the strip takes the rest.
    pub scenes_width: f32,
    pub sources_width: f32,
    pub mixer_width: f32,
    pub app_audio_width: f32,
    pub health_width: f32,
    /// The docks from left to right by index (0 Scenes, 1 Sources, 2 Audio Mixer, 3 Application
    /// Audio, 4 Health). Read it with [`Docks::order`].
    #[serde(deserialize_with = "lenient_order")]
    pub order: [u8; DOCK_COUNT],
    /// The docks shown in a window of their own instead of in the strip, by index. A saved list
    /// with fewer entries (from before Application Audio) reads the missing ones as their defaults.
    #[serde(deserialize_with = "lenient_len")]
    pub floating: [bool; DOCK_COUNT],
    /// Where each floating dock's window was last moved to, as its top-left corner in points, by
    /// index. A place is used only when `window_placed` is set for that dock.
    #[serde(deserialize_with = "lenient_len")]
    pub window_pos: [[f32; 2]; DOCK_COUNT],
    /// Whether each dock's window has a saved place; without one the window takes its default
    /// place.
    #[serde(deserialize_with = "lenient_len")]
    pub window_placed: [bool; DOCK_COUNT],
    /// The size of each floating dock's window with its frame, in points, by index; `[0, 0]` is
    /// not set and gives the window its default size.
    #[serde(deserialize_with = "lenient_len")]
    pub window_size: [[f32; 2]; DOCK_COUNT],
    /// Where each docked dock sits, by index, as a [`Region`] code: 0 under the canvas, 1 the left
    /// column, 2 the right column. Read it with [`Docks::region`]; an unknown code reads as under
    /// the canvas.
    #[serde(deserialize_with = "lenient_len")]
    pub region: [u8; DOCK_COUNT],
    /// The widths of the left and right columns in points; 0 is not set and uses the default in
    /// [`crate::strip`].
    pub left_width: f32,
    pub right_width: f32,
}

/// How many docks there are: Scenes, Sources, Audio Mixer, Application Audio and Health.
pub const DOCK_COUNT: usize = 5;

/// The docks' order by default.
pub const DOCK_ORDER: [u8; DOCK_COUNT] = [0, 1, 2, 3, 4];

/// Where a docked dock sits in the main window: under the canvas (the dock strip), or in the
/// column left or right of the canvas.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Region {
    Bottom,
    Left,
    Right,
}

impl Region {
    pub const ALL: [Region; 3] = [Region::Bottom, Region::Left, Region::Right];

    /// The code saved in [`Docks::region`].
    pub fn code(self) -> u8 {
        match self {
            Region::Bottom => 0,
            Region::Left => 1,
            Region::Right => 2,
        }
    }

    /// The region a saved code names; an unknown code is under the canvas.
    pub fn from_code(code: u8) -> Region {
        match code {
            1 => Region::Left,
            2 => Region::Right,
            _ => Region::Bottom,
        }
    }

    /// The region's name in `dock.region` and `ui.inspect`.
    pub fn name(self) -> &'static str {
        match self {
            Region::Bottom => "bottom",
            Region::Left => "left",
            Region::Right => "right",
        }
    }

    /// The region named `name` in `dock.region`.
    pub fn parse(name: &str) -> Option<Region> {
        Region::ALL.into_iter().find(|r| r.name() == name)
    }
}

impl Default for Docks {
    fn default() -> Self {
        Self {
            scenes: true,
            sources: true,
            mixer: true,
            app_audio: false,
            health: false,
            transitions: true,
            controls: true,
            height: 0.0,
            scenes_width: 0.0,
            sources_width: 0.0,
            mixer_width: 0.0,
            app_audio_width: 0.0,
            health_width: 0.0,
            order: DOCK_ORDER,
            floating: [false; DOCK_COUNT],
            window_pos: [[0.0; 2]; DOCK_COUNT],
            window_placed: [false; DOCK_COUNT],
            window_size: [[0.0; 2]; DOCK_COUNT],
            region: [0; DOCK_COUNT],
            left_width: 0.0,
            right_width: 0.0,
        }
    }
}

/// Reads a list of [`DOCK_COUNT`] values by dock index. A shorter list, saved before a dock was
/// added, reads the missing entries as their defaults; entries past the last dock are skipped
/// without being stored. An entry of the wrong type is an error, as in a list of the full length.
fn lenient_len<'de, D, T, const N: usize>(d: D) -> Result<[T; N], D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default + Copy,
{
    use serde::de::{IgnoredAny, SeqAccess, Visitor};

    struct List<T, const N: usize>(std::marker::PhantomData<T>);

    impl<'de, T: Deserialize<'de> + Default + Copy, const N: usize> Visitor<'de> for List<T, N> {
        type Value = [T; N];
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a list with a value for each dock")
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<[T; N], A::Error> {
            let mut out = [T::default(); N];
            for o in &mut out {
                match seq.next_element::<T>()? {
                    Some(v) => *o = v,
                    None => return Ok(out),
                }
            }
            while seq.next_element::<IgnoredAny>()?.is_some() {}
            Ok(out)
        }
    }

    d.deserialize_seq(List::<T, N>(std::marker::PhantomData))
}

/// Reads a saved order. Only the first [`DOCK_COUNT`] entries of a list are kept, and the rest are
/// skipped without being stored. An entry that is not a whole number from 0 to 255, or a value
/// that is not a list, reads as unknown ([`u8::MAX`]), which [`Docks::order`] replaces with the
/// default order.
fn lenient_order<'de, D: serde::Deserializer<'de>>(d: D) -> Result<[u8; DOCK_COUNT], D::Error> {
    use serde::de::{self, IgnoredAny, MapAccess, SeqAccess, Visitor};

    /// One value read as a dock index, or [`u8::MAX`] when it is not one.
    struct Slot(u8);

    impl<'de> Deserialize<'de> for Slot {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            d.deserialize_any(Any(|v: Option<i128>| Slot(v.and_then(|v| u8::try_from(v).ok()).unwrap_or(u8::MAX))))
        }
    }

    /// Takes any value: `f` gets a whole number as `Some`, and anything else as `None` after it
    /// has been skipped.
    struct Any<F>(F);

    impl<'de, T, F: FnOnce(Option<i128>) -> T> Visitor<'de> for Any<F> {
        type Value = T;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("any value")
        }
        fn visit_i64<E: de::Error>(self, v: i64) -> Result<T, E> {
            Ok((self.0)(Some(i128::from(v))))
        }
        fn visit_u64<E: de::Error>(self, v: u64) -> Result<T, E> {
            Ok((self.0)(Some(i128::from(v))))
        }
        fn visit_bool<E: de::Error>(self, _: bool) -> Result<T, E> {
            Ok((self.0)(None))
        }
        fn visit_f64<E: de::Error>(self, _: f64) -> Result<T, E> {
            Ok((self.0)(None))
        }
        fn visit_str<E: de::Error>(self, _: &str) -> Result<T, E> {
            Ok((self.0)(None))
        }
        fn visit_bytes<E: de::Error>(self, _: &[u8]) -> Result<T, E> {
            Ok((self.0)(None))
        }
        fn visit_unit<E: de::Error>(self) -> Result<T, E> {
            Ok((self.0)(None))
        }
        fn visit_none<E: de::Error>(self) -> Result<T, E> {
            Ok((self.0)(None))
        }
        fn visit_some<D: serde::Deserializer<'de>>(self, d: D) -> Result<T, D::Error> {
            IgnoredAny::deserialize(d)?;
            Ok((self.0)(None))
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<T, A::Error> {
            while seq.next_element::<IgnoredAny>()?.is_some() {}
            Ok((self.0)(None))
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<T, A::Error> {
            while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
            Ok((self.0)(None))
        }
    }

    /// The order: a list read entry by entry, or anything else skipped.
    struct Order;

    impl<'de> Visitor<'de> for Order {
        type Value = [u8; DOCK_COUNT];
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a list of dock indexes")
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<[u8; DOCK_COUNT], A::Error> {
            let mut out = [u8::MAX; DOCK_COUNT];
            for o in &mut out {
                match seq.next_element::<Slot>()? {
                    Some(Slot(v)) => *o = v,
                    None => return Ok(out),
                }
            }
            while seq.next_element::<IgnoredAny>()?.is_some() {}
            Ok(out)
        }
        fn visit_bool<E: de::Error>(self, _: bool) -> Result<[u8; DOCK_COUNT], E> {
            Ok([u8::MAX; DOCK_COUNT])
        }
        fn visit_i64<E: de::Error>(self, _: i64) -> Result<[u8; DOCK_COUNT], E> {
            Ok([u8::MAX; DOCK_COUNT])
        }
        fn visit_u64<E: de::Error>(self, _: u64) -> Result<[u8; DOCK_COUNT], E> {
            Ok([u8::MAX; DOCK_COUNT])
        }
        fn visit_f64<E: de::Error>(self, _: f64) -> Result<[u8; DOCK_COUNT], E> {
            Ok([u8::MAX; DOCK_COUNT])
        }
        fn visit_str<E: de::Error>(self, _: &str) -> Result<[u8; DOCK_COUNT], E> {
            Ok([u8::MAX; DOCK_COUNT])
        }
        fn visit_bytes<E: de::Error>(self, _: &[u8]) -> Result<[u8; DOCK_COUNT], E> {
            Ok([u8::MAX; DOCK_COUNT])
        }
        fn visit_unit<E: de::Error>(self) -> Result<[u8; DOCK_COUNT], E> {
            Ok([u8::MAX; DOCK_COUNT])
        }
        fn visit_none<E: de::Error>(self) -> Result<[u8; DOCK_COUNT], E> {
            Ok([u8::MAX; DOCK_COUNT])
        }
        fn visit_some<D: serde::Deserializer<'de>>(self, d: D) -> Result<[u8; DOCK_COUNT], D::Error> {
            d.deserialize_any(Order)
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<[u8; DOCK_COUNT], A::Error> {
            while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
            Ok([u8::MAX; DOCK_COUNT])
        }
    }

    d.deserialize_any(Order)
}

impl Docks {
    /// The docks from left to right. A saved order that leaves a dock out, names one twice or
    /// names an unknown one keeps its usable part, followed by the docks it misses in their
    /// default order.
    pub fn order(&self) -> [u8; DOCK_COUNT] {
        let mut out = DOCK_ORDER;
        let mut n = 0;
        for i in self.order.iter().chain(DOCK_ORDER.iter()) {
            if n < out.len() && usize::from(*i) < DOCK_ORDER.len() && !out[..n].contains(i) {
                out[n] = *i;
                n += 1;
            }
        }
        out
    }

    /// Whether dock `i` is shown.
    pub fn is_shown(&self, i: u8) -> bool {
        match i {
            0 => self.scenes,
            1 => self.sources,
            2 => self.mixer,
            3 => self.app_audio,
            4 => self.health,
            _ => false,
        }
    }

    /// Whether dock `i` is shown in a window of its own.
    pub fn is_floating(&self, i: u8) -> bool {
        self.floating.get(usize::from(i)).copied().unwrap_or(false)
    }

    /// Whether dock `i` is shown in the main window rather than in a window of its own.
    pub fn is_docked(&self, i: u8) -> bool {
        self.is_shown(i) && !self.is_floating(i)
    }

    /// The region dock `i` sits in while it is docked.
    pub fn region(&self, i: u8) -> Region {
        self.region.get(usize::from(i)).copied().map_or(Region::Bottom, Region::from_code)
    }

    /// Put dock `i` in `region`; false when `i` is not a dock or it is there already.
    pub fn set_region(&mut self, i: u8, region: Region) -> bool {
        let Some(r) = self.region.get_mut(usize::from(i)) else { return false };
        let was = Region::from_code(*r);
        *r = region.code();
        was != region
    }

    /// The region dock `i` is drawn in: its own, or under the canvas while the columns are
    /// `collapsed` into the strip.
    pub fn drawn_region(&self, i: u8, collapsed: bool) -> Region {
        if collapsed { Region::Bottom } else { self.region(i) }
    }

    /// The docked docks drawn in `region`, in order (left to right under the canvas, top to bottom
    /// in a column). While the columns are `collapsed`, every docked dock is drawn under the canvas.
    pub fn shown_in(&self, region: Region, collapsed: bool) -> Vec<u8> {
        self.order().into_iter().filter(|i| self.is_docked(*i) && self.drawn_region(*i, collapsed) == region).collect()
    }

    /// The docks shown in windows of their own, in the strip's order.
    pub fn floating_shown(&self) -> Vec<u8> {
        self.order().into_iter().filter(|i| self.is_shown(*i) && self.is_floating(*i)).collect()
    }

    /// Where dock `i`'s window was last moved to, when a place was saved and it is a usable point.
    pub fn window_pos(&self, i: u8) -> Option<[f32; 2]> {
        let k = usize::from(i);
        let p = *self.window_pos.get(k)?;
        (self.window_placed.get(k) == Some(&true) && p.iter().all(|v| v.is_finite())).then_some(p)
    }

    /// Save `at` as dock `i`'s window place and `size` as its size; false when `i` is not a dock
    /// or nothing changed.
    pub fn place_window(&mut self, i: usize, at: [f32; 2], size: [f32; 2]) -> bool {
        let (Some(p), Some(placed), Some(s)) = (self.window_pos.get_mut(i), self.window_placed.get_mut(i), self.window_size.get_mut(i)) else {
            return false;
        };
        if *placed && *p == at && *s == size {
            return false;
        }
        (*p, *placed, *s) = (at, true, size);
        true
    }

    /// The size dock `i`'s window was last given, when both sides are usable (finite and above
    /// zero).
    pub fn window_size(&self, i: u8) -> Option<[f32; 2]> {
        let s = *self.window_size.get(usize::from(i))?;
        s.iter().all(|v| crate::strip::usable(*v).is_some()).then_some(s)
    }

    /// The saved widths by dock index.
    pub fn widths(&self) -> [f32; DOCK_COUNT] {
        [self.scenes_width, self.sources_width, self.mixer_width, self.app_audio_width, self.health_width]
    }

    /// The widths of the left and right columns as saved.
    pub fn column_widths(&self) -> [f32; 2] {
        [self.left_width, self.right_width]
    }

    /// Move dock `i` past the nearest dock drawn in the same region before it (`left`: left under
    /// the canvas, above in a column) or after it, with the columns `collapsed` into the strip or
    /// not. False when `i` is not docked or no dock in its region is on that side.
    pub fn shift(&mut self, i: u8, left: bool, collapsed: bool) -> bool {
        if !self.is_docked(i) {
            return false;
        }
        let region = self.drawn_region(i, collapsed);
        let near = |d: u8| self.is_docked(d) && self.drawn_region(d, collapsed) == region;
        let mut order = self.order();
        let Some(at) = order.iter().position(|d| *d == i) else { return false };
        let to = if left { (0..at).rev().find(|k| near(order[*k])) } else { (at + 1..order.len()).find(|k| near(order[*k])) };
        let Some(to) = to else { return false };
        if left {
            order[to..=at].rotate_right(1);
        } else {
            order[at..=to].rotate_left(1);
        }
        self.order = order;
        true
    }
}

/// The most dock layouts Docks › Save Layout… keeps.
pub const MAX_DOCK_LAYOUTS: usize = 32;

/// A dock layout saved with Docks › Save Layout…: every dock setting under a name.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DockLayout {
    pub name: String,
    pub docks: Docks,
}

/// The built-in dock layouts, by their English names. They are listed in Docks › Layouts and
/// applied with `dock.layout_apply` like saved ones, but can't be saved over or removed.
pub const BUILT_IN_LAYOUTS: [&str; 4] = ["Set up", "Live", "Review", "Classic"];

/// The default widths of the left column in the Set up and Live layouts.
const SET_UP_COLUMN: f32 = 256.0;
const LIVE_COLUMN: f32 = 268.0;
/// The width of the right column, which holds the Health dock, in the Live layout.
const LIVE_RAIL: f32 = 292.0;

/// The built-in layout named `name`, ignoring case:
///
/// - Set up: Scenes and Sources in a 256 point left column, the Audio Mixer under the canvas.
/// - Live: Sources and the Audio Mixer in a 268 point left column, Scenes under the canvas, and
///   Health in a 292 point right column.
/// - Review: Scenes in the left column, the Audio Mixer and Application Audio under the canvas,
///   Sources hidden.
/// - Classic: the docks as they are after Reset Docks, every one under the canvas.
///
/// Every built-in layout keeps the transition and output controls shown and no dock floating.
pub fn built_in_layout(name: &str) -> Option<DockLayout> {
    let k = BUILT_IN_LAYOUTS.iter().position(|n| n.eq_ignore_ascii_case(name))?;
    let (bottom, left, right) = (Region::Bottom.code(), Region::Left.code(), Region::Right.code());
    let classic = Docks::default();
    let docks = match k {
        0 => Docks { region: [left, left, bottom, bottom, bottom], left_width: SET_UP_COLUMN, ..classic },
        1 => Docks { health: true, region: [bottom, left, left, bottom, right], left_width: LIVE_COLUMN, right_width: LIVE_RAIL, ..classic },
        2 => Docks { sources: false, app_audio: true, region: [left, left, bottom, bottom, bottom], left_width: SET_UP_COLUMN, ..classic },
        _ => classic,
    };
    Some(DockLayout { name: BUILT_IN_LAYOUTS.get(k).copied().unwrap_or_default().to_string(), docks })
}

/// Whether `name` is a built-in layout's name, ignoring case.
pub fn is_built_in_layout(name: &str) -> bool {
    BUILT_IN_LAYOUTS.iter().any(|n| n.eq_ignore_ascii_case(name))
}

/// Saved layouts named like a built-in one, saved before the built-in layouts existed, are kept
/// under the name with " (saved)" added, or " (saved 2)" and on when that is taken too.
fn rename_built_in_names(layouts: &mut [DockLayout]) {
    for k in 0..layouts.len() {
        if !layouts.get(k).is_some_and(|l| is_built_in_layout(&l.name)) {
            continue;
        }
        let base = layouts.get(k).map(|l| l.name.clone()).unwrap_or_default();
        let taken = |n: &str, layouts: &[DockLayout]| is_built_in_layout(n) || layouts.iter().any(|l| l.name.eq_ignore_ascii_case(n));
        let free = (1..=MAX_DOCK_LAYOUTS + 1)
            .map(|n| if n == 1 { format!("{base} (saved)") } else { format!("{base} (saved {n})") })
            .find(|n| !taken(n, layouts));
        if let (Some(free), Some(l)) = (free, layouts.get_mut(k)) {
            l.name = free;
        }
    }
}

/// Reads at most [`MAX_DOCK_LAYOUTS`] layouts and skips the rest. A layout named like a built-in
/// one is renamed (see [`rename_built_in_names`]).
fn bounded_layouts<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<DockLayout>, D::Error> {
    let mut out = bounded_layout_list(d)?;
    rename_built_in_names(&mut out);
    Ok(out)
}

fn bounded_layout_list<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<DockLayout>, D::Error> {
    use serde::de::{IgnoredAny, SeqAccess, Visitor};

    struct Layouts;

    impl<'de> Visitor<'de> for Layouts {
        type Value = Vec<DockLayout>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a list of dock layouts")
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut out = Vec::new();
            while out.len() < MAX_DOCK_LAYOUTS {
                match seq.next_element::<DockLayout>()? {
                    Some(l) => out.push(l),
                    None => return Ok(out),
                }
            }
            while seq.next_element::<IgnoredAny>()?.is_some() {}
            Ok(out)
        }
    }

    d.deserialize_seq(Layouts)
}

/// The preferences in `config.toml`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    /// The scene collection open last.
    pub collection: String,
    /// The profile in use last.
    pub profile: String,
    pub docks: Docks,
    /// The saved dock layouts, in the order they were first saved.
    #[serde(deserialize_with = "bounded_layouts")]
    pub dock_layouts: Vec<DockLayout>,
    pub stats: bool,
    /// View › Scopes is open.
    pub scopes: bool,
    /// The scope the Scopes window shows: `waveform`, `parade`, `histogram`, `vectorscopeYuv` or
    /// `vectorscopeHls`. Another value reads as `waveform`.
    pub scope_kind: String,
    pub status_bar: bool,
    /// Items in the preview can't be moved or resized with the mouse.
    pub lock_preview: bool,
    /// The Scenes dock shows its scenes as a grid of tiles with their pictures, and not as a list.
    pub scene_grid: bool,
    /// Shortcuts changed from their defaults, saved as a list of `{ command, keys }` entries. A
    /// table with one entry per command, as version 5 and earlier saved it, still reads.
    #[serde(with = "craft_keymap::keymap::overrides_list")]
    pub hotkeys: Overrides,
    /// The interface language: `auto` (the system's) or a language code. Read it with
    /// [`Prefs::language`].
    pub language: String,
    /// Look for a newer release when the app starts.
    pub check_updates: bool,
    /// Keep the desktop's title bar where the app would draw its own. Read when the window
    /// opens, so a change takes effect at the next start.
    pub system_title_bar: bool,
    /// The graphics adapter the window draws with (Settings › Video › Hardware acceleration).
    /// Read when the window opens. A value this build cannot read is the automatic choice.
    pub graphics_adapter: Preference,
    /// When the window's graphics device was last lost, in seconds since 1970, oldest first (at
    /// most [`crate::hardware::KEPT_LOSSES`] are kept, and a longer list reads as its last ones). A
    /// value this build cannot read is empty.
    #[serde(deserialize_with = "bounded_losses")]
    pub graphics_losses: Vec<u64>,
    /// For each profile whose canvas follows a display, that display. Read it with
    /// [`Prefs::canvas_display`].
    #[serde(deserialize_with = "bounded_canvas_displays")]
    pub canvas_displays: Vec<CanvasDisplay>,
}

/// The most profiles whose canvas follows a display; the earliest are forgotten first.
pub const MAX_CANVAS_DISPLAYS: usize = 64;

/// The display a profile's canvas size came from (Canvas Size from Display, or Settings › Video ›
/// Match display). While that profile is in use and its canvas is still `width` × `height`, the
/// canvas takes the display's new size when the display's resolution or scaling changes. Setting
/// the canvas size another way ends this.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CanvasDisplay {
    pub profile: String,
    /// The display's id in `video.displays`. An id can change when the app starts again or the
    /// display is connected again; the display is then found by `name`, when exactly one listed
    /// display has that name.
    pub id: String,
    pub name: String,
    pub width: u32,
    pub height: u32,
}

/// Reads at most [`MAX_CANVAS_DISPLAYS`] links and skips the rest.
fn bounded_canvas_displays<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<CanvasDisplay>, D::Error> {
    use serde::de::{IgnoredAny, SeqAccess, Visitor};

    struct Links;

    impl<'de> Visitor<'de> for Links {
        type Value = Vec<CanvasDisplay>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a list of displays canvases follow")
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut out = Vec::new();
            while out.len() < MAX_CANVAS_DISPLAYS {
                match seq.next_element::<CanvasDisplay>()? {
                    Some(l) => out.push(l),
                    None => return Ok(out),
                }
            }
            while seq.next_element::<IgnoredAny>()?.is_some() {}
            Ok(out)
        }
    }

    d.deserialize_seq(Links)
}

/// The last [`crate::hardware::KEPT_LOSSES`] entries of a list of loss times, without holding the
/// rest.
fn bounded_losses<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<u64>, D::Error> {
    use serde::de::{SeqAccess, Visitor};

    struct Losses;

    impl<'de> Visitor<'de> for Losses {
        type Value = Vec<u64>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a list of times")
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut out = std::collections::VecDeque::with_capacity(crate::hardware::KEPT_LOSSES + 1);
            while let Some(t) = seq.next_element::<u64>()? {
                out.push_back(t);
                if out.len() > crate::hardware::KEPT_LOSSES {
                    out.pop_front();
                }
            }
            Ok(out.into())
        }
    }

    d.deserialize_seq(Losses)
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            collection: DEFAULT_NAME.into(),
            profile: DEFAULT_NAME.into(),
            docks: Docks::default(),
            dock_layouts: Vec::new(),
            stats: false,
            scopes: false,
            scope_kind: "waveform".into(),
            status_bar: true,
            lock_preview: false,
            scene_grid: false,
            hotkeys: Overrides::new(),
            language: crate::i18n::AUTO.into(),
            check_updates: true,
            system_title_bar: false,
            graphics_adapter: Preference::Auto,
            graphics_losses: Vec::new(),
            canvas_displays: Vec::new(),
        }
    }
}

impl Prefs {
    /// The Language setting: `auto`, or the code of a language this build offers. A code this
    /// build does not offer reads as `auto`.
    pub fn language(&self) -> &str {
        crate::i18n::normalize(&self.language)
    }

    /// The display the canvas of the profile `profile` follows.
    pub fn canvas_display(&self, profile: &str) -> Option<&CanvasDisplay> {
        self.canvas_displays.iter().find(|l| l.profile == profile)
    }

    /// Make the canvas of `link.profile` follow `link`, in place of the display it followed.
    pub fn set_canvas_display(&mut self, link: CanvasDisplay) {
        self.forget_canvas_display(&link.profile);
        if self.canvas_displays.len() >= MAX_CANVAS_DISPLAYS {
            self.canvas_displays.remove(0);
        }
        self.canvas_displays.push(link);
    }

    /// The canvas of the profile `profile` follows no display.
    pub fn forget_canvas_display(&mut self, profile: &str) {
        self.canvas_displays.retain(|l| l.profile != profile);
    }

    /// The names of the saved dock layouts.
    pub fn dock_layout_names(&self) -> Vec<String> {
        self.dock_layouts.iter().map(|l| l.name.clone()).collect()
    }

    /// The built-in or saved dock layout named `name`, ignoring case.
    pub fn dock_layout(&self, name: &str) -> Option<DockLayout> {
        built_in_layout(name).or_else(|| self.dock_layouts.iter().find(|l| l.name.eq_ignore_ascii_case(name)).cloned())
    }

    /// Save the current docks as the layout `name`, replacing a layout with that name (ignoring
    /// case). Returns whether one was replaced. A built-in layout's name is refused.
    pub fn save_dock_layout(&mut self, name: &str) -> Result<bool, Msg> {
        check_name(name)?;
        if is_built_in_layout(name) {
            return Err(tmsg!("`{name}` is a built-in layout; choose another name", name = name));
        }
        let docks = self.docks;
        if let Some(l) = self.dock_layouts.iter_mut().find(|l| l.name.eq_ignore_ascii_case(name)) {
            *l = DockLayout { name: name.to_string(), docks };
            return Ok(true);
        }
        if self.dock_layouts.len() >= MAX_DOCK_LAYOUTS {
            return Err(tmsg!("there are already {count} dock layouts; remove one first", count = MAX_DOCK_LAYOUTS));
        }
        self.dock_layouts.push(DockLayout { name: name.to_string(), docks });
        Ok(false)
    }

    /// Remove the saved layout `name`, ignoring case. A built-in layout's name and a name no saved
    /// layout has are errors.
    pub fn remove_dock_layout(&mut self, name: &str) -> Result<(), Msg> {
        if is_built_in_layout(name) {
            return Err(tmsg!("`{name}` is a built-in layout and can't be removed", name = name));
        }
        let before = self.dock_layouts.len();
        self.dock_layouts.retain(|l| !l.name.eq_ignore_ascii_case(name));
        if self.dock_layouts.len() == before {
            return Err(tmsg!("there is no dock layout named `{name}`", name = name));
        }
        Ok(())
    }
}

impl craft_settings::Settings for Prefs {
    const APP_ID: &'static str = "studiocraft";
    /// Version 2 adds `docks.mixer_width` and `docks.order`, version 3 adds `docks.floating`,
    /// `docks.window_pos`, `docks.window_placed` and `docks.window_size`, version 4 adds
    /// `dock_layouts`, version 5 adds `docks.app_audio` and `docks.app_audio_width` and a fourth
    /// entry, for Application Audio, in each list kept by dock index, version 6 saves `hotkeys`
    /// as a list of `{ command, keys }` entries instead of a table, and version 7 adds
    /// `docks.region`, `docks.left_width` and `docks.right_width`, in the docks and in each saved
    /// layout, and version 8 adds `docks.health` and `docks.health_width` and a fifth entry, for
    /// Health, in each list kept by dock index, in the docks and in each saved layout.
    const SCHEMA_VERSION: u32 = 8;

    /// Each version up to 5, and 6 to 8: an older file lacks the keys its next version adds, so the
    /// docks keep their saved settings and read the new keys as their defaults (every dock but
    /// Application Audio and Health shown, every dock under the canvas, no saved window places or
    /// sizes, no saved layouts, no column widths). Lists kept by dock index that are short read the
    /// missing entries as their defaults. The file is left as it is. Version 5 to 6: a `hotkeys`
    /// table is rewritten as the list, with the same entries.
    fn migrate(from: u32, doc: &mut craft_settings::DocumentMut) -> Result<(), craft_settings::MigrateError> {
        if from == 5 && doc.get("hotkeys").is_some_and(|h| h.is_table_like()) {
            let (shortcuts, _) = craft_settings::document::deserialize_sections::<Shortcuts>(doc);
            doc.remove("hotkeys");
            craft_settings::document::merge_into(doc, &shortcuts).map_err(|e| craft_settings::MigrateError::new(from, e.to_string()))?;
        }
        Ok(())
    }

    /// When another instance saved the file meanwhile, the shortcut list merges entry by entry,
    /// by command: a shortcut either instance reset stays reset, and a shortcut only the other
    /// instance changed keeps its change. Everything else merges key by key.
    fn merge_rule(path: &[&str]) -> craft_settings::MergeRule {
        match path {
            ["hotkeys"] => craft_settings::MergeRule::Entries {
                key: |entry| entry.as_inline_table().and_then(|t| t.get("command")).and_then(|c| c.as_str()).map(str::to_owned),
            },
            _ => craft_settings::MergeRule::ByKey,
        }
    }
}

/// The `hotkeys` key alone, for the version 5 to 6 migration: it reads the table and writes the
/// list without touching the rest of the file.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct Shortcuts {
    #[serde(with = "craft_keymap::keymap::overrides_list")]
    hotkeys: Overrides,
}

impl craft_settings::Settings for Shortcuts {
    const APP_ID: &'static str = <Prefs as craft_settings::Settings>::APP_ID;
    const SCHEMA_VERSION: u32 = <Prefs as craft_settings::Settings>::SCHEMA_VERSION;
}

/// The name of the first collection and profile.
pub const DEFAULT_NAME: &str = "Untitled";

const SCENES: &str = "scenes";
const PROFILES: &str = "profiles";

enum Files {
    Dir(PathBuf),
    Memory(BTreeMap<(String, String), String>),
}

pub struct Store {
    prefs: SettingsStore<Prefs>,
    files: Files,
    /// How many profiles were saved since the store was opened.
    profile_saves: u64,
}

/// Why a collection or profile name can't be used as a file name.
pub fn check_name(name: &str) -> Result<(), Msg> {
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5",
        "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed.chars().count() > 100 {
        return Err(tmsg!("a name needs 1 to 100 characters"));
    }
    if trimmed != name || name.ends_with('.') {
        return Err(tmsg!("a name can't start or end with a space, or end with a period"));
    }
    if let Some(c) = name.chars().find(|c| c.is_control() || "<>:\"/\\|?*".contains(*c)) {
        return Err(tmsg!("a name can't contain {character}", character = format!("{c:?}")));
    }
    // Windows reserves a device name followed by any extension too (`CON.txt`).
    let stem = name.split('.').next().unwrap_or(name).trim_end();
    if RESERVED.iter().any(|r| r.eq_ignore_ascii_case(stem)) {
        return Err(tmsg!("`{name}` is reserved by the system", name = name));
    }
    Ok(())
}

/// The text of the file at `path`. A file larger than the saved formats accept is refused before
/// it is read into memory.
fn read_bounded(path: &Path) -> Result<String, String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let max = u64::try_from(MAX_FILE_BYTES).unwrap_or(u64::MAX);
    let too_large = || format!("a saved file can be at most {} MB", MAX_FILE_BYTES >> 20);
    if file.metadata().map_err(|e| e.to_string())?.len() > max {
        return Err(too_large());
    }
    let mut text = String::new();
    // The file can grow between the size check and the read; read at most one byte past the cap.
    file.take(max.saturating_add(1)).read_to_string(&mut text).map_err(|e| e.to_string())?;
    if text.len() > MAX_FILE_BYTES {
        return Err(too_large());
    }
    Ok(text)
}

impl Store {
    /// The store in the configuration folder craft-settings chooses (`STUDIOCRAFT_CONFIG_DIR`
    /// overrides it), and problems found loading the preferences.
    pub fn open(location: Location) -> (Store, Vec<String>) {
        let (prefs, report) = SettingsStore::<Prefs>::open(location);
        let mut problems: Vec<String> = report.section_errors.iter().map(|e| format!("{e:?}")).collect();
        if let Some(moved) = &report.broken_moved_to {
            problems.push(format!("the damaged settings file was moved to {}", moved.display()));
        }
        let files = match prefs.dir() {
            Some(dir) => Files::Dir(dir.to_path_buf()),
            None => Files::Memory(BTreeMap::new()),
        };
        (Store { prefs, files, profile_saves: 0 }, problems)
    }

    /// A store that reads and writes nothing.
    pub fn memory() -> Store {
        Store::open(Location::Memory).0
    }

    pub fn dir(&self) -> Option<&Path> {
        match &self.files {
            Files::Dir(d) => Some(d),
            Files::Memory(_) => None,
        }
    }

    pub fn prefs(&self) -> &Prefs {
        self.prefs.get()
    }

    pub fn update(&mut self, f: impl FnOnce(&mut Prefs)) {
        self.prefs.update(f);
    }

    /// Save the preferences when they changed a moment ago; when to call again.
    pub fn tick(&mut self) -> Option<Instant> {
        self.prefs.tick(Instant::now()).next_deadline
    }

    pub fn flush(&mut self) -> Result<(), String> {
        self.prefs.flush().map_err(|e| e.to_string())
    }

    fn list(&self, kind: &str) -> Vec<String> {
        let mut names: Vec<String> = match &self.files {
            Files::Dir(d) => std::fs::read_dir(d.join(kind))
                .map(|rd| {
                    rd.filter_map(Result::ok)
                        .filter_map(|e| {
                            let p = e.path();
                            (p.extension().is_some_and(|x| x == "json")).then(|| p.file_stem().map(|s| s.to_string_lossy().into_owned())).flatten()
                        })
                        .collect()
                })
                .unwrap_or_default(),
            Files::Memory(m) => m.keys().filter(|(k, _)| k == kind).map(|(_, n)| n.clone()).collect(),
        };
        names.sort_by_key(|n| n.to_lowercase());
        names
    }

    fn read(&self, kind: &str, name: &str) -> Result<String, String> {
        check_name(name).map_err(|e| e.english())?;
        match &self.files {
            Files::Dir(d) => {
                let path = d.join(kind).join(format!("{name}.json"));
                read_bounded(&path).map_err(|e| format!("{}: {e}", path.display()))
            }
            Files::Memory(m) => m.get(&(kind.to_string(), name.to_string())).cloned().ok_or_else(|| format!("there is no `{name}`")),
        }
    }

    fn write(&mut self, kind: &str, name: &str, text: String) -> Result<(), String> {
        check_name(name).map_err(|e| e.english())?;
        match &mut self.files {
            Files::Dir(d) => {
                let folder = d.join(kind);
                std::fs::create_dir_all(&folder).map_err(|e| format!("{}: {e}", folder.display()))?;
                let path = folder.join(format!("{name}.json"));
                craft_atomic::atomic_write(&path, text.as_bytes()).map_err(|e| format!("{}: {e}", path.display()))
            }
            Files::Memory(m) => {
                m.insert((kind.to_string(), name.to_string()), text);
                Ok(())
            }
        }
    }

    fn remove(&mut self, kind: &str, name: &str) -> Result<(), String> {
        check_name(name).map_err(|e| e.english())?;
        match &mut self.files {
            Files::Dir(d) => {
                let path = d.join(kind).join(format!("{name}.json"));
                match std::fs::remove_file(&path) {
                    Ok(()) => Ok(()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    Err(e) => Err(format!("{}: {e}", path.display())),
                }
            }
            Files::Memory(m) => {
                m.remove(&(kind.to_string(), name.to_string()));
                Ok(())
            }
        }
    }

    /// Give the file `old` the name `new`, in one step: also on a disk that ignores case, where
    /// `Show` and `show` name one file.
    fn rename(&mut self, kind: &str, old: &str, new: &str) -> Result<(), String> {
        check_name(old).map_err(|e| e.english())?;
        check_name(new).map_err(|e| e.english())?;
        let taken = || format!("`{new}` already exists");
        match &mut self.files {
            Files::Dir(d) => {
                let (from, to) = (d.join(kind).join(format!("{old}.json")), d.join(kind).join(format!("{new}.json")));
                // A file named exactly `new` is another one (a disk that tells case apart can hold
                // both `Show` and `show`); it is never replaced.
                let target = std::ffi::OsString::from(format!("{new}.json"));
                let entries = std::fs::read_dir(d.join(kind)).map_err(|e| format!("{}: {e}", d.join(kind).display()))?;
                if new != old && entries.flatten().any(|e| e.file_name() == target) {
                    return Err(taken());
                }
                std::fs::rename(&from, &to).map_err(|e| format!("{}: {e}", from.display()))
            }
            Files::Memory(m) => {
                if new != old && m.contains_key(&(kind.to_string(), new.to_string())) {
                    return Err(taken());
                }
                let text = m.remove(&(kind.to_string(), old.to_string())).ok_or_else(|| format!("there is no `{old}`"))?;
                m.insert((kind.to_string(), new.to_string()), text);
                Ok(())
            }
        }
    }

    pub fn rename_collection(&mut self, old: &str, new: &str) -> Result<(), String> {
        self.rename(SCENES, old, new)
    }

    pub fn rename_profile(&mut self, old: &str, new: &str) -> Result<(), String> {
        self.rename(PROFILES, old, new)
    }

    pub fn collections(&self) -> Vec<String> {
        self.list(SCENES)
    }

    pub fn load_collection(&self, name: &str) -> Result<SceneCollection, String> {
        let text = self.read(SCENES, name)?;
        let mut c = read_collection(&text, "Scene").map_err(|e| format!("{name}: {e}"))?;
        c.name = name.to_string();
        Ok(c)
    }

    pub fn save_collection(&mut self, c: &SceneCollection) -> Result<(), String> {
        let text = write_collection(c).map_err(|e| e.to_string())?;
        self.write(SCENES, &c.name, text)
    }

    pub fn remove_collection(&mut self, name: &str) -> Result<(), String> {
        self.remove(SCENES, name)
    }

    pub fn profiles(&self) -> Vec<String> {
        self.list(PROFILES)
    }

    pub fn load_profile(&self, name: &str) -> Result<Profile, String> {
        let text = self.read(PROFILES, name)?;
        let mut p = read_profile(&text).map_err(|e| format!("{name}: {e}"))?;
        p.name = name.to_string();
        Ok(p)
    }

    pub fn save_profile(&mut self, p: &Profile) -> Result<(), String> {
        let text = write_profile(p).map_err(|e| e.to_string())?;
        self.write(PROFILES, &p.name, text)?;
        self.profile_saves = self.profile_saves.wrapping_add(1);
        Ok(())
    }

    /// How many profiles were saved since the store was opened.
    pub fn profile_saves(&self) -> u64 {
        self.profile_saves
    }

    pub fn remove_profile(&mut self, name: &str) -> Result<(), String> {
        self.remove(PROFILES, name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_must_be_file_names() {
        assert!(check_name("Gaming").is_ok());
        assert!(check_name("Late show 2").is_ok());
        assert!(check_name("Concert.v2").is_ok());
        for bad in ["", " lead", "trail ", "dot.", "a/b", "a:b", "con", "CON.txt", "nul.tar.gz", "Com1 .x", "x\u{7}"] {
            assert!(check_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn collections_and_profiles_round_trip_in_memory() {
        let mut s = Store::memory();
        let mut c = SceneCollection::new("Show", "Intro");
        c.scenes[0].name = "Intro".into();
        s.save_collection(&c).unwrap();
        assert_eq!(s.collections(), ["Show"]);
        assert_eq!(s.load_collection("Show").unwrap().scenes[0].name, "Intro");
        s.remove_collection("Show").unwrap();
        assert!(s.collections().is_empty());

        let p = Profile { name: "Stream".into(), ..Profile::default() };
        s.save_profile(&p).unwrap();
        assert_eq!(s.load_profile("Stream").unwrap(), p);
    }

    #[test]
    fn a_rename_never_replaces_another_file() {
        let mut s = Store::memory();
        s.save_collection(&SceneCollection::new("Show", "Intro")).unwrap();
        s.save_collection(&SceneCollection::new("show", "Other")).unwrap();
        assert!(s.rename_collection("Show", "show").is_err());
        assert_eq!(s.load_collection("show").unwrap().scenes[0].name, "Other");
        assert!(s.load_collection("Show").is_ok());
    }

    #[test]
    fn collections_round_trip_in_a_folder() {
        let dir = std::env::temp_dir().join(format!("studiocraft-store-{}", std::process::id()));
        let (mut s, problems) = Store::open(Location::Dir(dir.clone()));
        assert!(problems.is_empty(), "{problems:?}");
        let c = SceneCollection::new("B", "Scene");
        s.save_collection(&c).unwrap();
        s.save_collection(&SceneCollection::new("a", "Scene")).unwrap();
        assert_eq!(s.collections(), ["a", "B"]);
        assert!(dir.join("scenes").join("B.json").exists());
        s.update(|p| p.collection = "B".into());
        s.flush().unwrap();
        let (again, _) = Store::open(Location::Dir(dir.clone()));
        assert_eq!(again.prefs().collection, "B");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_oversized_saved_file_is_refused_before_it_is_read() {
        let dir = std::env::temp_dir().join(format!("studiocraft-store-oversized-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (s, _) = Store::open(Location::Dir(dir.clone()));
        let scenes = dir.join(SCENES);
        std::fs::create_dir_all(&scenes).unwrap();
        let file = std::fs::File::create(scenes.join("Big.json")).unwrap();
        file.set_len(u64::try_from(MAX_FILE_BYTES).unwrap() + 1).unwrap();
        drop(file);
        let e = s.load_collection("Big").unwrap_err();
        assert!(e.contains("can be at most 64 MB"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_system_title_bar_setting_is_kept_across_a_restart() {
        let dir = std::env::temp_dir().join(format!("studiocraft-store-title-bar-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (mut s, _) = Store::open(Location::Dir(dir.clone()));
        assert!(!s.prefs().system_title_bar, "off by default");
        for on in [true, false] {
            s.update(|p| p.system_title_bar = on);
            s.flush().unwrap();
            let (again, problems) = Store::open(Location::Dir(dir.clone()));
            assert!(problems.is_empty(), "{problems:?}");
            assert_eq!(again.prefs().system_title_bar, on);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_graphics_adapter_and_its_losses_are_kept_and_a_bad_value_reads_as_auto() {
        use studiocraft_render::device::AdapterKey;
        let dir = std::env::temp_dir().join(format!("studiocraft-store-gpu-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (mut s, _) = Store::open(Location::Dir(dir.clone()));
        assert_eq!(s.prefs().graphics_adapter, Preference::Auto, "automatic by default");
        let key = AdapterKey {
            name: "GPU".into(),
            backend: "vulkan".into(),
            vendor: 0x10de,
            device: 0x2684,
            pci_bus_id: "0000:01:00.0".into(),
            driver: "NVIDIA".into(),
        };
        for p in [Preference::Adapter(key), Preference::Software, Preference::LowPower] {
            s.update(|prefs| {
                prefs.graphics_adapter = p.clone();
                prefs.graphics_losses = vec![10, 20];
            });
            s.flush().unwrap();
            let (again, problems) = Store::open(Location::Dir(dir.clone()));
            assert!(problems.is_empty(), "{problems:?}");
            assert_eq!(again.prefs().graphics_adapter, p);
            assert_eq!(again.prefs().graphics_losses, [10, 20]);
        }
        let config = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.path()).find(|p| p.extension().is_some_and(|x| x == "toml")).unwrap();
        // a value of the wrong kind, and a table from a later build
        for adapter in ["graphics_adapter = \"quantum\"\n", "[graphics_adapter]\nkind = \"quantum\"\n"] {
            std::fs::write(&config, format!("language = \"de\"\ngraphics_losses = \"often\"\n\n{adapter}")).unwrap();
            let (bad, _) = Store::open(Location::Dir(dir.clone()));
            assert_eq!(bad.prefs().graphics_adapter, Preference::Auto, "{adapter}");
            assert!(bad.prefs().graphics_losses.is_empty(), "{adapter}");
            assert_eq!(bad.prefs().language, "de", "the other settings are kept: {adapter}");
        }
        // a long list reads as its last entries
        let long: Vec<String> = (1..=10_000).map(|t| t.to_string()).collect();
        std::fs::write(&config, format!("graphics_losses = [{}]\n", long.join(", "))).unwrap();
        let (many, _) = Store::open(Location::Dir(dir.clone()));
        assert_eq!(many.prefs().graphics_losses, [9_998, 9_999, 10_000]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dock_sizes_are_kept_across_a_restart() {
        let dir = std::env::temp_dir().join(format!("studiocraft-store-dock-sizes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (mut s, _) = Store::open(Location::Dir(dir.clone()));
        assert_eq!((s.prefs().docks.height, s.prefs().docks.scenes_width, s.prefs().docks.sources_width), (0.0, 0.0, 0.0));
        s.update(|p| {
            p.docks.height = 236.5;
            p.docks.scenes_width = 280.0;
            p.docks.sources_width = 412.0;
            p.docks.mixer = false;
        });
        s.flush().unwrap();
        let (mut again, problems) = Store::open(Location::Dir(dir.clone()));
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(again.prefs().docks, s.prefs().docks);
        // Clearing the sizes is kept too: the defaults come back after the next start.
        again.update(|p| p.docks = Docks { mixer: false, ..Docks::default() });
        again.flush().unwrap();
        let (third, _) = Store::open(Location::Dir(dir.clone()));
        assert_eq!(third.prefs().docks, Docks { mixer: false, ..Docks::default() });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_version_1_config_keeps_its_dock_settings_and_is_saved_as_the_current_version() {
        let dir = std::env::temp_dir().join(format!("studiocraft-store-prefs-v1-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let v1 = "schema_version = 1\n\n[docks]\nmixer = false\nheight = 240.0\nscenes_width = 250.0\nsources_width = 310.0\n";
        std::fs::write(dir.join("config.toml"), v1).unwrap();
        let (mut s, _) = Store::open(Location::Dir(dir.clone()));
        let d = s.prefs().docks;
        let want = Docks { mixer: false, height: 240.0, scenes_width: 250.0, sources_width: 310.0, ..Docks::default() };
        assert_eq!(d, want);
        assert_eq!((d.mixer_width, d.order()), (0.0, DOCK_ORDER), "the keys version 1 lacks read as their defaults");
        s.update(|p| p.docks.height = 250.0);
        s.flush().unwrap();
        let text = std::fs::read_to_string(dir.join("config.toml")).unwrap();
        assert!(text.contains("schema_version = 8"), "{text}");
        let (again, _) = Store::open(Location::Dir(dir.clone()));
        assert_eq!(again.prefs().docks, Docks { height: 250.0, ..want });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_version_2_config_keeps_its_dock_order_and_puts_every_dock_in_the_strip() {
        let dir = std::env::temp_dir().join(format!("studiocraft-store-prefs-v2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let v2 = "schema_version = 2\n\n[docks]\nmixer_width = 420.0\norder = [2, 0, 1]\n";
        std::fs::write(dir.join("config.toml"), v2).unwrap();
        let (mut s, _) = Store::open(Location::Dir(dir.clone()));
        let d = s.prefs().docks;
        assert_eq!(d, Docks { mixer_width: 420.0, order: [2, 0, 1, u8::MAX, u8::MAX], ..Docks::default() });
        assert_eq!(d.order(), [2, 0, 1, 3, 4]);
        assert_eq!((d.floating, d.window_pos(0)), ([false; DOCK_COUNT], None));
        s.update(|p| p.docks.floating[1] = true);
        s.flush().unwrap();
        let text = std::fs::read_to_string(dir.join("config.toml")).unwrap();
        assert!(text.contains("schema_version = 8"), "{text}");
        let (again, _) = Store::open(Location::Dir(dir.clone()));
        assert_eq!(again.prefs().docks.floating, [false, true, false, false, false]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_version_3_config_has_no_dock_layouts_and_saved_layouts_are_kept() {
        let dir = std::env::temp_dir().join(format!("studiocraft-store-prefs-v3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let v3 = "schema_version = 3\n\n[docks]\nmixer = false\nfloating = [false, true, false]\n";
        std::fs::write(dir.join("config.toml"), v3).unwrap();
        let (mut s, _) = Store::open(Location::Dir(dir.clone()));
        assert_eq!(s.prefs().dock_layouts, []);
        assert!(!s.prefs().docks.mixer);
        s.update(|p| {
            assert_eq!(p.save_dock_layout("Talk"), Ok(false));
            p.docks.mixer = true;
            assert_eq!(p.save_dock_layout("Game"), Ok(false));
        });
        s.flush().unwrap();
        let (again, _) = Store::open(Location::Dir(dir.clone()));
        let p = again.prefs();
        assert_eq!(p.dock_layout_names(), ["Talk", "Game"]);
        let talk = p.dock_layout("talk").map(|l| l.docks);
        assert_eq!(talk.map(|d| (d.mixer, d.floating)), Some((false, [false, true, false, false, false])));
        assert_eq!(p.dock_layout("Game").map(|l| l.docks.mixer), Some(true));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_version_4_config_keeps_its_docks_and_hides_application_audio() {
        let dir = std::env::temp_dir().join(format!("studiocraft-store-prefs-v4-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let v4 = "schema_version = 4\n\n[docks]\norder = [2, 0, 1]\nfloating = [false, false, true]\nwindow_pos = [[0.0, 0.0], [0.0, 0.0], [40.0, 60.0]]\n\
                  window_placed = [false, false, true]\nwindow_size = [[0.0, 0.0], [0.0, 0.0], [420.0, 300.0]]\n\n\
                  [[dock_layouts]]\nname = \"Talk\"\n\n[dock_layouts.docks]\nsources = false\nfloating = [true, false, false]\n";
        std::fs::write(dir.join("config.toml"), v4).unwrap();
        let (mut s, problems) = Store::open(Location::Dir(dir.clone()));
        assert!(problems.is_empty(), "{problems:?}");
        let d = s.prefs().docks;
        assert!(!d.app_audio, "Application Audio starts hidden");
        assert_eq!(d.order(), [2, 0, 1, 3, 4], "Application Audio goes after the saved order");
        assert_eq!(d.floating, [false, false, true, false, false]);
        assert_eq!((d.window_pos(2), d.window_size(2), d.window_pos(3)), (Some([40.0, 60.0]), Some([420.0, 300.0]), None));
        assert_eq!(d.shown_in(Region::Bottom, false), [0, 1]);
        assert_eq!(s.prefs().dock_layout("Talk").map(|l| l.docks.floating), Some([true, false, false, false, false]));
        s.update(|p| {
            p.docks.app_audio = true;
            p.docks.floating[3] = true;
        });
        s.flush().unwrap();
        let text = std::fs::read_to_string(dir.join("config.toml")).unwrap();
        assert!(text.contains("schema_version = 8"), "{text}");
        let (again, _) = Store::open(Location::Dir(dir.clone()));
        assert_eq!(again.prefs().docks.floating_shown(), [2, 3]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_version_5_config_keeps_its_shortcuts_and_saves_them_as_a_list() {
        let dir = std::env::temp_dir().join(format!("studiocraft-store-prefs-v5-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let v5 = "schema_version = 5\nlanguage = \"fr\"\n\n[hotkeys]\n\"record.toggle\" = \"Ctrl+R\"\n\"edit.undo\" = \"Ctrl+Z\"\n";
        std::fs::write(dir.join("config.toml"), v5).unwrap();
        let open = || Store::open(Location::Dir(dir.clone()));
        let texts = |s: &Store, key: &str| s.prefs().hotkeys.get(key).map(<[String]>::to_vec);
        let (mut s, problems) = open();
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(texts(&s, "record.toggle"), Some(vec!["Ctrl+R".to_string()]));
        assert_eq!(texts(&s, "edit.undo"), Some(vec!["Ctrl+Z".to_string()]));
        s.flush().unwrap();
        let text = std::fs::read_to_string(dir.join("config.toml")).unwrap();
        assert!(text.contains("schema_version = 8"), "{text}");
        assert!(!text.contains("[hotkeys]"), "the table is replaced: {text}");
        assert!(text.contains("command = \"record.toggle\""), "{text}");
        let (again, problems) = open();
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(again.prefs(), s.prefs());
        assert_eq!(again.prefs().language, "fr", "the other settings are kept");

        // Two instances open the version 5 file; one saves, then the other resets a shortcut.
        std::fs::write(dir.join("config.toml"), v5).unwrap();
        let (mut first, _) = open();
        let (mut second, _) = open();
        first.update(|p| p.language = "de".into());
        first.flush().unwrap();
        second.update(|p| {
            p.hotkeys.remove("record.toggle");
        });
        second.flush().unwrap();
        let (merged, problems) = open();
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(texts(&merged, "record.toggle"), None, "the reset shortcut does not come back");
        assert_eq!(texts(&merged, "edit.undo"), Some(vec!["Ctrl+Z".to_string()]));
        assert_eq!(merged.prefs().language, "de", "the first instance's change is kept");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_version_6_config_keeps_its_docks_and_layouts_under_the_canvas() {
        let dir = std::env::temp_dir().join(format!("studiocraft-store-prefs-v6-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let v6 = "schema_version = 6\n\n[docks]\norder = [2, 0, 1, 3]\nfloating = [false, true, false, false]\nheight = 230.0\n\n\
                  [[dock_layouts]]\nname = \"Live\"\n\n[dock_layouts.docks]\nmixer = false\n\n\
                  [[dock_layouts]]\nname = \"Talk\"\n\n[dock_layouts.docks]\norder = [1, 0, 2, 3]\n";
        std::fs::write(dir.join("config.toml"), v6).unwrap();
        let (mut s, problems) = Store::open(Location::Dir(dir.clone()));
        assert!(problems.is_empty(), "{problems:?}");
        let d = s.prefs().docks;
        assert_eq!((d.order(), d.floating[1], d.height), ([2, 0, 1, 3, 4], true, 230.0));
        assert_eq!((d.region, d.column_widths()), ([0; DOCK_COUNT], [0.0, 0.0]), "every dock under the canvas, no column widths");
        assert_eq!(d.shown_in(Region::Bottom, false), [2, 0]);
        assert_eq!(s.prefs().dock_layout_names(), ["Live (saved)", "Talk"]);
        let talk = s.prefs().dock_layout("Talk").map(|l| l.docks).unwrap_or_default();
        assert_eq!((talk.order(), talk.shown_in(Region::Bottom, false)), ([1, 0, 2, 3, 4], vec![1, 0, 2]), "a saved layout is Classic-shaped");
        s.update(|p| {
            p.docks.set_region(2, Region::Right);
            p.docks.right_width = 320.0;
        });
        s.flush().unwrap();
        let text = std::fs::read_to_string(dir.join("config.toml")).unwrap();
        assert!(text.contains("schema_version = 8") && text.contains("Live (saved)"), "{text}");
        let (again, problems) = Store::open(Location::Dir(dir.clone()));
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!((again.prefs().docks.region(2), again.prefs().docks.right_width), (Region::Right, 320.0));
        assert_eq!(again.prefs().dock_layout_names(), ["Live (saved)", "Talk"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_version_7_config_keeps_its_docks_and_reads_health_as_hidden() {
        let dir = std::env::temp_dir().join(format!("studiocraft-store-prefs-v7-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let v7 = "schema_version = 7\n\n[docks]\norder = [3, 2, 0, 1]\nfloating = [false, false, true, false]\n\
                  region = [1, 2, 0, 0]\nright_width = 310.0\nmixer_width = 420.0\n\n\
                  [[dock_layouts]]\nname = \"Talk\"\n\n[dock_layouts.docks]\nregion = [0, 0, 2, 0]\napp_audio = true\n";
        std::fs::write(dir.join("config.toml"), v7).unwrap();
        let (mut s, problems) = Store::open(Location::Dir(dir.clone()));
        assert!(problems.is_empty(), "{problems:?}");
        let d = s.prefs().docks;
        assert_eq!(d.order(), [3, 2, 0, 1, 4], "Health follows the saved order");
        assert_eq!((d.region(0), d.region(1), d.region(4)), (Region::Left, Region::Right, Region::Bottom));
        assert_eq!((d.floating[2], d.floating[4], d.right_width, d.mixer_width), (true, false, 310.0, 420.0));
        assert!(!d.health && !d.is_shown(4), "Health is hidden");
        assert_eq!((d.health_width, d.window_size(4), d.window_pos(4)), (0.0, None, None));
        let talk = s.prefs().dock_layout("Talk").map(|l| l.docks).unwrap_or_default();
        assert_eq!((talk.region(2), talk.region(4), talk.health, talk.app_audio), (Region::Right, Region::Bottom, false, true));
        s.update(|p| {
            p.docks.health = true;
            p.docks.set_region(4, Region::Right);
            p.docks.health_width = 330.0;
        });
        s.flush().unwrap();
        let text = std::fs::read_to_string(dir.join("config.toml")).unwrap();
        assert!(text.contains("schema_version = 8"), "{text}");
        let (again, problems) = Store::open(Location::Dir(dir.clone()));
        assert!(problems.is_empty(), "{problems:?}");
        let d = again.prefs().docks;
        assert_eq!((d.health, d.region(4), d.health_width, d.order()), (true, Region::Right, 330.0, [3, 2, 0, 1, 4]));
        assert_eq!(d.shown_in(Region::Right, false), [1, 4]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_reset_shortcut_stays_reset_after_a_save_and_a_load() {
        let dir = std::env::temp_dir().join(format!("studiocraft-store-hotkeys-reset-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let open = || Store::open(Location::Dir(dir.clone()));
        let (mut s, _) = open();
        s.update(|p| {
            p.hotkeys.insert("record.toggle", vec!["Ctrl+R".into()]);
            p.hotkeys.insert("edit.undo", vec!["Ctrl+Z".into()]);
        });
        s.flush().unwrap();
        // Another instance saves the file in between.
        let (mut other, _) = open();
        other.update(|p| p.language = "de".into());
        other.flush().unwrap();
        s.update(|p| {
            p.hotkeys.remove("record.toggle");
        });
        s.flush().unwrap();
        let (mut again, problems) = open();
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(again.prefs().hotkeys.get("record.toggle"), None, "the reset shortcut does not come back");
        assert_eq!(again.prefs().hotkeys.get("edit.undo").map(<[String]>::to_vec), Some(vec!["Ctrl+Z".to_string()]));
        assert_eq!(again.prefs().language, "de", "the other instance's change is kept");
        again.update(|p| p.hotkeys.clear());
        again.flush().unwrap();
        let (third, _) = open();
        assert!(third.prefs().hotkeys.is_empty(), "resetting every shortcut is kept too");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn another_instance_s_shortcut_changes_are_kept_when_this_one_resets_a_shortcut() {
        let dir = std::env::temp_dir().join(format!("studiocraft-store-hotkeys-merge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let open = || Store::open(Location::Dir(dir.clone()));
        let (mut s, _) = open();
        s.update(|p| {
            p.hotkeys.insert("record.toggle", vec!["Ctrl+R".into()]);
            // two shortcuts for one command
            p.hotkeys.insert("edit.undo", vec!["Ctrl+Z".into(), "Ctrl+Alt+Z".into()]);
        });
        s.flush().unwrap();
        let (mut other, _) = open();
        other.update(|p| {
            p.hotkeys.insert("edit.undo", vec!["Ctrl+Y".into()]);
            p.hotkeys.insert("app.settings", vec!["F5".into()]);
        });
        other.flush().unwrap();
        s.update(|p| {
            p.hotkeys.remove("record.toggle");
        });
        s.flush().unwrap();
        let (again, problems) = open();
        assert!(problems.is_empty(), "{problems:?}");
        let texts = |key: &str| again.prefs().hotkeys.get(key).map(<[String]>::to_vec);
        assert_eq!(texts("record.toggle"), None, "reset here");
        assert_eq!(texts("edit.undo"), Some(vec!["Ctrl+Y".to_string()]), "changed only by the other instance");
        assert_eq!(texts("app.settings"), Some(vec!["F5".to_string()]), "added by the other instance");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lists_kept_by_dock_index_read_short_lists_and_cut_long_ones() {
        let read = |text: &str| serde_json::from_str::<Docks>(text);
        assert_eq!(read(r#"{"floating": [true]}"#).ok().map(|d| d.floating), Some([true, false, false, false, false]));
        assert_eq!(read(r#"{"floating": [true, true, true, true, true, true]}"#).ok().map(|d| d.floating), Some([true; DOCK_COUNT]));
        assert_eq!(read(r#"{"window_size": [[1.0, 2.0]]}"#).ok().map(|d| d.window_size), Some([[1.0, 2.0], [0.0; 2], [0.0; 2], [0.0; 2], [0.0; 2]]));
        assert_eq!(read(r#"{"floating": []}"#).ok().map(|d| d.floating), Some([false; DOCK_COUNT]));
        assert!(read(r#"{"floating": ["yes"]}"#).is_err(), "a value of the wrong type is still an error");
        assert!(read(r#"{"floating": true}"#).is_err(), "so is a value that is not a list");
    }

    #[test]
    fn dock_layouts_are_replaced_by_name_removed_and_limited() {
        let mut p = Prefs::default();
        assert_eq!(p.save_dock_layout("One"), Ok(false));
        p.docks.sources = false;
        assert_eq!(p.save_dock_layout("ONE"), Ok(true), "the same name in other letters replaces it");
        assert_eq!(p.dock_layout_names(), ["ONE"]);
        assert_eq!(p.dock_layout("one").map(|l| l.docks.sources), Some(false));
        assert!(p.save_dock_layout("").is_err());
        assert!(p.save_dock_layout(" spaced").is_err());
        for i in 1..MAX_DOCK_LAYOUTS {
            assert_eq!(p.save_dock_layout(&format!("L{i}")), Ok(false));
        }
        assert_eq!(p.dock_layouts.len(), MAX_DOCK_LAYOUTS);
        assert!(p.save_dock_layout("One more").is_err(), "the list is full");
        assert_eq!(p.save_dock_layout("L5"), Ok(true), "a full list still takes a replacement");
        assert_eq!(p.remove_dock_layout("one"), Ok(()));
        assert!(p.remove_dock_layout("one").is_err(), "already removed");
        assert_eq!(p.dock_layouts.len(), MAX_DOCK_LAYOUTS - 1);

        // A file with more layouts than the limit keeps the first ones.
        let many: Vec<serde_json::Value> = (0..100).map(|i| serde_json::json!({"name": format!("N{i}"), "docks": {}})).collect();
        let read: Prefs = serde_json::from_value(serde_json::json!({ "dock_layouts": many })).unwrap();
        assert_eq!(read.dock_layouts.len(), MAX_DOCK_LAYOUTS);
        assert_eq!(read.dock_layouts[0].name, "N0");
    }

    #[test]
    fn a_window_place_is_used_only_when_it_is_set_and_finite() {
        let mut d = Docks::default();
        assert_eq!(d.window_pos(2), None, "not set");
        d.window_pos[2] = [40.0, 60.0];
        assert_eq!(d.window_pos(2), None, "a place that was never saved");
        assert!(d.place_window(2, [0.0, 0.0], [300.0, 200.0]));
        assert_eq!(d.window_pos(2), Some([0.0, 0.0]), "the top-left corner is a place like any other");
        assert!(!d.place_window(2, [0.0, 0.0], [300.0, 200.0]), "nothing changed");
        assert!(d.place_window(2, [40.0, 60.0], [300.0, 200.0]));
        assert_eq!(d.window_pos(2), Some([40.0, 60.0]));
        d.window_pos[2] = [f32::NAN, 60.0];
        assert_eq!(d.window_pos(2), None);
        assert_eq!(d.window_pos(9), None, "an unknown dock");
        assert!(!d.place_window(9, [1.0, 1.0], [300.0, 200.0]));
        d.floating = [true, false, true, false, false];
        d.mixer = false;
        assert_eq!((d.shown_in(Region::Bottom, false), d.floating_shown()), (vec![1], vec![0]), "a hidden dock is in neither list");
    }

    #[test]
    fn a_damaged_dock_size_reads_as_the_default() {
        let dir = std::env::temp_dir().join(format!("studiocraft-store-dock-damaged-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (mut s, _) = Store::open(Location::Dir(dir.clone()));
        s.update(|p| {
            p.docks.height = f32::NAN;
            p.docks.scenes_width = -40.0;
            p.docks.sources_width = f32::INFINITY;
        });
        s.flush().unwrap();
        let (again, _) = Store::open(Location::Dir(dir.clone()));
        let d = again.prefs().docks;
        assert_eq!(crate::strip::height(800.0, 744.0, d.height), 200.0);
        assert_eq!(crate::strip::widths(1280.0, &[0, 1, 2], d.widths()), [300.0, 340.0, 638.0]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_saved_dock_order_is_kept_and_a_damaged_one_is_repaired() {
        let with = |order: [u8; DOCK_COUNT]| Docks { order, ..Docks::default() };
        assert_eq!(with([2, 0, 1, 3, 4]).order(), [2, 0, 1, 3, 4]);
        assert_eq!(with([2, 2, 2, 2, 2]).order(), [2, 0, 1, 3, 4], "a dock named twice keeps its first place");
        assert_eq!(with([9, 1, 200, 3, 4]).order(), [1, 3, 4, 0, 2], "unknown docks are left out");
        assert_eq!(with([u8::MAX; DOCK_COUNT]).order(), DOCK_ORDER);
        let read = |text: &str| serde_json::from_str::<Docks>(text).map(|d| d.order());
        assert_eq!(read(r#"{"order": [1, 2, 0]}"#).ok(), Some([1, 2, 0, 3, 4]), "a list saved before Application Audio");
        assert_eq!(read(r#"{"order": [3, 1, 2, 0]}"#).ok(), Some([3, 1, 2, 0, 4]), "a list saved before Health");
        assert_eq!(read(r#"{"order": [4, 3, 1, 2, 0]}"#).ok(), Some([4, 3, 1, 2, 0]));
        assert_eq!(read(r#"{"order": [1]}"#).ok(), Some([1, 0, 2, 3, 4]), "a short list is filled in");
        assert_eq!(read(r#"{"order": [1, 0, 2, 2, 1, 4]}"#).ok(), Some([1, 0, 2, 3, 4]), "a long list is cut");
        assert_eq!(read(r#"{"order": [-1, 300, 2]}"#).ok(), Some([2, 0, 1, 3, 4]));
        assert_eq!(read(r#"{"order": "left"}"#).ok(), Some(DOCK_ORDER), "not a list");
        assert_eq!(read(r#"{"order": [1.5, "x", 0]}"#).ok(), Some(DOCK_ORDER), "a list of other things");
        assert_eq!(read(r#"{"order": [[2], {"a": 1}, 2, 0]}"#).ok(), Some([2, 0, 1, 3, 4]), "lists and tables in the list are skipped");
        let long = format!(r#"{{"order": [2, 1{}]}}"#, ", 0".repeat(100_000));
        assert_eq!(read(&long).ok(), Some([2, 1, 0, 3, 4]), "only the first five entries are read");
        assert_eq!(read("{}").ok(), Some(DOCK_ORDER));
    }

    #[test]
    fn shift_passes_over_hidden_docks_and_stops_at_the_ends() {
        let mut d = Docks::default();
        assert!(d.shift(2, true, false));
        assert_eq!(d.order(), [0, 2, 1, 3, 4]);
        assert!(!d.shift(0, true, false), "the first dock cannot move left");
        assert_eq!(d.order(), [0, 2, 1, 3, 4]);
        d.mixer = false;
        assert!(d.shift(0, false, false), "Scenes passes the hidden mixer and lands right of Sources");
        assert_eq!(d.order(), [2, 1, 0, 3, 4]);
        assert_eq!(d.shown_in(Region::Bottom, false), [1, 0]);
        assert!(!d.shift(0, false, false), "nothing shown on its right: Application Audio and Health are hidden");
        assert!(!d.shift(1, true, false), "only the hidden mixer is on its left");
        assert_eq!(d.order(), [2, 1, 0, 3, 4]);
        d.app_audio = true;
        assert!(d.shift(3, true, false), "Application Audio moves left once it is shown");
        assert_eq!(d.order(), [2, 1, 3, 0, 4]);
        d.health = true;
        assert!(d.shift(4, true, false), "and so does Health");
        assert_eq!(d.order(), [2, 1, 3, 4, 0]);
    }

    #[test]
    fn each_dock_sits_in_its_region_and_moves_among_the_docks_there() {
        let mut d = Docks::default();
        assert_eq!(Region::ALL.map(|r| d.shown_in(r, false)), [vec![0, 1, 2], vec![], vec![]], "every dock starts under the canvas");
        assert!(d.set_region(0, Region::Left));
        assert!(!d.set_region(0, Region::Left), "already there");
        assert!(d.set_region(2, Region::Left));
        assert!(d.set_region(1, Region::Right));
        assert!(!d.set_region(9, Region::Left), "an unknown dock");
        assert_eq!(Region::ALL.map(|r| d.shown_in(r, false)), [vec![], vec![0, 2], vec![1]]);
        assert_eq!(d.shown_in(Region::Bottom, true), [0, 1, 2], "collapsed columns are drawn in the strip, in order");
        assert!(d.shown_in(Region::Left, true).is_empty());
        // A move passes over the docks of other regions.
        assert!(d.shift(2, true, false), "the mixer moves above Scenes in the left column");
        assert_eq!(d.order(), [2, 0, 1, 3, 4]);
        assert!(!d.shift(1, true, false), "Sources is alone in the right column");
        assert!(d.shift(1, true, true), "while collapsed, Sources moves past Scenes in the strip");
        assert_eq!(d.order(), [2, 1, 0, 3, 4]);
        // A floating or hidden dock is in no region's list but keeps its region.
        d.floating[2] = true;
        d.scenes = false;
        assert_eq!(d.shown_in(Region::Left, false), Vec::<u8>::new());
        assert_eq!((d.region(2), d.region(0)), (Region::Left, Region::Left));
        // An unknown saved code reads as under the canvas, and setting it to that is no change.
        d.region[3] = 7;
        assert_eq!(d.region(3), Region::Bottom);
        assert!(!d.set_region(3, Region::Bottom));
        assert_eq!(d.region[3], 0);
        let read = |text: &str| serde_json::from_str::<Docks>(text).map(|d| d.region);
        assert_eq!(read(r#"{"region": [1]}"#).ok(), Some([1, 0, 0, 0, 0]), "a short list");
        assert_eq!(read("{}").ok(), Some([0; DOCK_COUNT]));
        assert!(read(r#"{"region": ["left"]}"#).is_err(), "a value of the wrong type is an error, as in the other lists");
    }

    #[test]
    fn the_built_in_layouts_are_read_only_and_classic_is_the_reset_strip() {
        let mut p = Prefs::default();
        assert_eq!(BUILT_IN_LAYOUTS, ["Set up", "Live", "Review", "Classic"]);
        assert_eq!(p.dock_layout("classic").map(|l| (l.name, l.docks)), Some(("Classic".to_string(), Docks::default())));
        let set_up = p.dock_layout("SET UP").map(|l| l.docks).unwrap_or_default();
        assert_eq!((set_up.shown_in(Region::Left, false), set_up.shown_in(Region::Bottom, false)), (vec![0, 1], vec![2]));
        assert_eq!((set_up.left_width, set_up.shown_in(Region::Right, false)), (256.0, vec![]));
        let live = p.dock_layout("Live").map(|l| l.docks).unwrap_or_default();
        assert_eq!((live.shown_in(Region::Left, false), live.shown_in(Region::Bottom, false)), (vec![1, 2], vec![0]));
        assert_eq!((live.shown_in(Region::Right, false), live.left_width, live.right_width), (vec![4], 268.0, 292.0), "Health in the right column");
        let review = p.dock_layout("review").map(|l| l.docks).unwrap_or_default();
        assert_eq!((review.shown_in(Region::Left, false), review.shown_in(Region::Bottom, false)), (vec![0], vec![2, 3]));
        assert!(!review.health && !Docks::default().health, "only Live shows Health");
        for name in BUILT_IN_LAYOUTS {
            let l = built_in_layout(name).map(|l| l.docks).unwrap_or_default();
            assert!(l.transitions && l.controls && l.floating == [false; DOCK_COUNT], "{name}");
            assert!(p.save_dock_layout(name).is_err(), "{name} can't be saved over");
            assert!(p.save_dock_layout(&name.to_uppercase()).is_err(), "nor in other letters");
            assert!(p.remove_dock_layout(name).is_err(), "{name} can't be removed");
        }
        assert!(p.dock_layouts.is_empty());
        assert_eq!(p.dock_layout("Setup"), None);
        assert!(p.remove_dock_layout("Talk").is_err(), "no such layout");
    }

    #[test]
    fn a_saved_layout_named_like_a_built_in_one_is_kept_under_another_name() {
        let layouts = serde_json::json!([
            {"name": "Live", "docks": {"mixer": false}},
            {"name": "live (saved)", "docks": {}},
            {"name": "Talk", "docks": {"order": [2, 0, 1]}},
            {"name": "CLASSIC", "docks": {"sources": false}},
        ]);
        let read: Prefs = serde_json::from_value(serde_json::json!({ "dock_layouts": layouts })).unwrap();
        assert_eq!(read.dock_layout_names(), ["Live (saved 2)", "live (saved)", "Talk", "CLASSIC (saved)"]);
        assert_eq!(read.dock_layout("live (saved 2)").map(|l| l.docks.mixer), Some(false), "the layout keeps its docks");
        assert_eq!(read.dock_layout("Live").map(|l| l.docks), built_in_layout("Live").map(|l| l.docks), "the name is the built-in one's");
        assert_eq!(read.dock_layout("Talk").map(|l| (l.docks.order(), l.docks.region)), Some(([2, 0, 1, 3, 4], [0; DOCK_COUNT])));
    }
}
