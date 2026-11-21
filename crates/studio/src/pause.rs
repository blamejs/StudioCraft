//! The pause screen: what the program shows while it is paused.
//!
//! While paused, the program, and with it every recording, stream and replay buffer, shows the
//! pause screen instead of the program scene: a background color, then one picture or one video
//! that loops, then the countdown text. Each layer runs as a source thread of its own, outside the
//! scene collection, so nothing about a pause is saved or undone. Nothing runs while the program
//! is not paused.
//!
//! The countdown runs on the master clock, which is monotonic, so a change of the system time does
//! not move it. [`PauseState`] holds the countdown and takes the time as an argument, so it is
//! tested without waiting.

use std::path::Path;
use std::sync::Arc;

use serde_json::{Value, json};
use studiocraft_engine::host::SourceHandle;
use studiocraft_media::clock::MasterClock;
use studiocraft_model::{PauseAtZero, PauseSettings, Settings, SourceId, Transform, Vec2};
use studiocraft_plugin::Registry;
use studiocraft_render::{ItemDesc, SceneDesc};

/// The ids the pause screen's layer threads start under. The compositor draws the layers under
/// ids no source of the collection holds ([`free_ids`]).
const BACKGROUND: SourceId = SourceId(u64::MAX);
const MEDIA: SourceId = SourceId(u64::MAX - 1);
const TEXT: SourceId = SourceId(u64::MAX - 2);

/// The three largest ids for which `taken` is false, for the background, the media and the text.
/// A collection holds far fewer sources than there are ids, so the search ends.
pub(crate) fn free_ids(taken: impl Fn(SourceId) -> bool) -> [SourceId; 3] {
    let mut free = (0..=u64::MAX).rev().map(SourceId).filter(|id| !taken(*id));
    let mut next = || free.next().unwrap_or(SourceId(0));
    [next(), next(), next()]
}

/// Media file types that hold sound only, which the pause screen does not take.
const SOUND_ONLY: [&str; 6] = ["mp3", "wav", "flac", "ogg", "m4a", "aac"];

/// The notice for a picture or video the pause screen can't show.
pub(crate) const MEDIA_NOTICE: &str = "The pause screen's media could not be shown: {error}";

/// The notice for a countdown text the pause screen can't show.
pub(crate) const TEXT_NOTICE: &str = "The pause screen's countdown could not be shown: {error}";

/// The margin around the countdown text, as a share of each side of the canvas.
const MARGIN: f32 = 0.05;

/// `ms` as the countdown shows it: `m:ss`, or `h:mm:ss` from one hour, with the seconds rounded
/// up, so the text reads `0:01` until the last second has passed.
pub fn countdown_text(ms: u64) -> String {
    clock_text(ms.div_ceil(1000))
}

/// `ms` as the time paused shows it, with the seconds rounded down.
pub fn paused_text(ms: u64) -> String {
    clock_text(ms / 1000)
}

fn clock_text(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

/// The file extensions a source plug-in `kind` opens, lowercase.
fn extensions_of(registry: &Registry, kind: &str) -> Vec<String> {
    let Ok(plugin) = registry.source(kind) else { return Vec::new() };
    plugin
        .schema()
        .fields
        .into_iter()
        .filter_map(|f| match f.kind {
            studiocraft_model::FieldKind::Path { extensions, .. } => Some(extensions),
            _ => None,
        })
        .flatten()
        .map(|e| e.to_ascii_lowercase())
        .collect()
}

/// The picture and video file extensions the pause screen shows, for a file dialog's filter.
pub fn media_extensions(registry: &Registry) -> Vec<String> {
    let mut all = extensions_of(registry, "image");
    all.extend(extensions_of(registry, "media").into_iter().filter(|e| !SOUND_ONLY.contains(&e.as_str())));
    all.sort();
    all.dedup();
    all
}

/// The plug-in that shows `path`: `image` for a picture, `media` for a video. An error for a file
/// type neither takes, or one that holds sound only.
pub(crate) fn media_kind(registry: &Registry, path: &str) -> Result<&'static str, String> {
    let ext = Path::new(path).extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).unwrap_or_default();
    if !ext.is_empty() && extensions_of(registry, "image").contains(&ext) {
        Ok("image")
    } else if !ext.is_empty() && !SOUND_ONLY.contains(&ext.as_str()) && extensions_of(registry, "media").contains(&ext) {
        Ok("media")
    } else {
        Err(format!("`{path}` is not a picture or video file the pause screen can show"))
    }
}

/// What reaching zero asks of the studio.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Zero {
    /// The text at zero replaces the countdown.
    Keep,
    /// Resume the program.
    Resume,
}

/// A running pause: its settings and countdown. Times are milliseconds on the master clock.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PauseState {
    pub since_ms: u64,
    /// The profile's settings with this pause's own values.
    pub settings: PauseSettings,
    /// When the countdown reaches zero; `None` without a countdown.
    pub deadline_ms: Option<u64>,
    /// The countdown's length as last set, in seconds.
    pub total_s: u32,
    /// Whether the countdown has reached zero.
    pub reached: bool,
}

impl PauseState {
    pub fn new(now_ms: u64, settings: PauseSettings) -> PauseState {
        let mut s = PauseState { since_ms: now_ms, deadline_ms: None, total_s: 0, reached: false, settings };
        s.set_countdown(now_ms, s.settings.countdown_s);
        s
    }

    /// A countdown of `secs` from `now_ms`, or none for 0.
    pub fn set_countdown(&mut self, now_ms: u64, secs: u32) {
        let secs = secs.min(*PauseSettings::COUNTDOWN_S.end());
        self.settings.countdown_s = secs;
        self.total_s = secs;
        self.reached = false;
        self.deadline_ms = (secs > 0).then(|| now_ms.saturating_add(u64::from(secs) * 1000));
    }

    /// Add `secs` to the time left; a countdown at zero, or none, starts again from `secs`. The
    /// time left is at most 12 hours.
    pub fn extend(&mut self, now_ms: u64, secs: u32) {
        let max_ms = u64::from(*PauseSettings::COUNTDOWN_S.end()) * 1000;
        let left = self.remaining_ms(now_ms).unwrap_or(0);
        let total = left.saturating_add(u64::from(secs) * 1000).min(max_ms);
        self.deadline_ms = Some(now_ms.saturating_add(total));
        self.total_s = u32::try_from(total.div_ceil(1000)).unwrap_or(u32::MAX);
        self.settings.countdown_s = self.total_s;
        self.reached = false;
    }

    /// The time left, or `None` without a countdown.
    pub fn remaining_ms(&self, now_ms: u64) -> Option<u64> {
        self.deadline_ms.map(|d| d.saturating_sub(now_ms))
    }

    /// The countdown text at `now_ms`, or `None` without a countdown.
    pub fn text(&self, now_ms: u64) -> Option<String> {
        let left = self.remaining_ms(now_ms)?;
        if self.reached || left == 0 {
            return Some(self.settings.zero_text.clone());
        }
        Some(self.settings.text.replacen(PauseSettings::TIME, &countdown_text(left), 1))
    }

    /// What to do at `now_ms`: [`Zero`] once, when the countdown first reaches zero.
    pub fn step(&mut self, now_ms: u64) -> Option<Zero> {
        if self.reached || self.remaining_ms(now_ms) != Some(0) {
            return None;
        }
        self.reached = true;
        Some(match self.settings.at_zero {
            PauseAtZero::Keep => Zero::Keep,
            PauseAtZero::Resume => Zero::Resume,
        })
    }

    /// The state `pause.status` reports, with the errors of the picture or video and of the
    /// countdown text.
    pub fn json(&self, now_ms: u64, media_error: Option<&str>, text_error: Option<&str>) -> Value {
        let countdown = self.deadline_ms.map(|_| {
            json!({
                "total_s": self.total_s,
                "remaining_ms": self.remaining_ms(now_ms).unwrap_or(0),
                "at_zero": self.settings.at_zero.id(),
                "reached": self.reached,
            })
        });
        json!({
            "paused": true,
            "since_ms": now_ms.saturating_sub(self.since_ms),
            "countdown": countdown,
            "media": self.settings.media,
            "media_error": media_error,
            "text_error": text_error,
        })
    }
}

/// The source threads that draw the pause screen.
pub(crate) struct Layers {
    background: SourceHandle,
    media: Option<SourceHandle>,
    text: Option<SourceHandle>,
    /// The settings each layer was last sent.
    background_settings: Settings,
    media_settings: Option<(&'static str, Settings)>,
    text_settings: Settings,
    /// The last media error the user was told about.
    told: Option<String>,
    /// Why the picture or video could not start.
    media_failed: Option<String>,
    /// The last countdown text error the user was told about.
    told_text: Option<String>,
    /// Why the countdown text's thread could not start.
    text_failed: Option<String>,
    /// The settings the screen is drawn with: how the media is fitted and where the text goes.
    pub look: PauseSettings,
}

impl Layers {
    /// Start the layers for `s` on a `canvas`, with the countdown text `text`. A picture or video
    /// that could not start is left out, with the reason in the returned list; a countdown text
    /// that could not start is reported by [`Layers::untold_errors`]. Without the background
    /// there are no layers.
    pub fn start(
        registry: &Registry,
        clock: MasterClock,
        s: &PauseSettings,
        canvas: (u32, u32),
        text: Option<&str>,
    ) -> (Option<Layers>, Vec<String>) {
        let mut problems = Vec::new();
        let spawn = |id: SourceId, kind: &str, settings: Settings, problems: &mut Vec<String>| {
            let started = registry
                .source(kind)
                .map_err(|e| e.to_string())
                .and_then(|p| SourceHandle::spawn(id, p, settings, clock).map_err(|e| format!("no thread for the pause screen: {e}")));
            started.map_err(|e| problems.push(e)).ok()
        };
        let background_settings = background_settings(s, canvas);
        let Some(background) = spawn(BACKGROUND, "color", background_settings.clone(), &mut problems) else { return (None, problems) };
        let mut layers = Layers {
            background,
            media: None,
            text: None,
            background_settings,
            media_settings: None,
            text_settings: Settings::new(),
            told: None,
            media_failed: None,
            told_text: None,
            text_failed: None,
            look: s.clone(),
        };
        layers.set_media(registry, clock, &s.media, &mut problems);
        layers.set_text(registry, clock, s, canvas, text);
        (Some(layers), problems)
    }

    /// Show `path` (none when empty), starting its thread again only when it changed.
    pub fn set_media(&mut self, registry: &Registry, clock: MasterClock, path: &str, problems: &mut Vec<String>) {
        let (wanted, mut failed) = if path.is_empty() {
            (None, None)
        } else {
            match media_kind(registry, path) {
                Ok(kind) => (Some((kind, media_settings(kind, path))), None),
                Err(e) => (None, Some(e)),
            }
        };
        if failed.is_none() && self.media_failed.is_none() && wanted == self.media_settings {
            return;
        }
        self.media = wanted.as_ref().and_then(|(kind, settings)| {
            registry
                .source(kind)
                .map_err(|e| e.to_string())
                .and_then(|p| SourceHandle::spawn(MEDIA, p, settings.clone(), clock).map_err(|e| format!("no thread for the pause screen: {e}")))
                .map_err(|e| failed = Some(e))
                .ok()
        });
        self.media_settings = wanted.filter(|_| self.media.is_some());
        // A failure to start is told through `problems` now, and reported by `media_error`.
        self.told.clone_from(&failed);
        self.media_failed.clone_from(&failed);
        problems.extend(failed);
    }

    /// Show `text` as the countdown, or nothing. Only a changed text reaches the text's thread. A
    /// text thread that could not start is tried again with the next changed text, and its
    /// reason is the text's error until then.
    pub fn set_text(&mut self, registry: &Registry, clock: MasterClock, s: &PauseSettings, canvas: (u32, u32), text: Option<&str>) {
        let Some(t) = text else {
            self.text = None;
            self.text_failed = None;
            self.text_settings = Settings::new();
            return;
        };
        let settings = text_settings(s, canvas, t);
        if settings == self.text_settings {
            return;
        }
        match &self.text {
            Some(h) => h.update(settings.clone()),
            None => {
                let started = registry
                    .source("text")
                    .map_err(|e| e.to_string())
                    .and_then(|p| SourceHandle::spawn(TEXT, p, settings.clone(), clock).map_err(|e| format!("no thread for the pause screen: {e}")));
                (self.text, self.text_failed) = match started {
                    Ok(h) => (Some(h), None),
                    Err(e) => (None, Some(e)),
                };
            }
        }
        self.text_settings = settings;
    }

    /// Size the background for a new `canvas`.
    pub fn set_canvas(&mut self, s: &PauseSettings, canvas: (u32, u32)) {
        let settings = background_settings(s, canvas);
        if settings != self.background_settings {
            self.background.update(settings.clone());
            self.background_settings = settings;
        }
    }

    /// The media's error, if it is not showing.
    pub fn media_error(&self) -> Option<String> {
        self.media.as_ref().and_then(SourceHandle::error).or_else(|| self.media_failed.clone())
    }

    /// The countdown text's error, if it is not showing.
    pub fn text_error(&self) -> Option<String> {
        self.text.as_ref().and_then(SourceHandle::error).or_else(|| self.text_failed.clone())
    }

    /// The errors of the media and of the countdown text the user has not been told about yet,
    /// each with the notice that tells it.
    pub fn untold_errors(&mut self) -> Vec<(&'static str, String)> {
        let (media, text) = (self.media_error(), self.text_error());
        let media = untold(&mut self.told, media).map(|e| (MEDIA_NOTICE, e));
        let text = untold(&mut self.told_text, text).map(|e| (TEXT_NOTICE, e));
        media.into_iter().chain(text).collect()
    }

    /// The sizes of the media and the text, once they show something.
    pub fn sizes(&self) -> (Option<(u32, u32)>, Option<(u32, u32)>) {
        (self.media.as_ref().and_then(|m| m.io.video.size()), self.text.as_ref().and_then(|t| t.io.video.size()))
    }

    /// The pause screen as the compositor draws it on a `canvas`, bottom to top. The media and the
    /// text are placed once they show something. The layers are drawn under ids for which `taken`
    /// is false, so they never share an id with a source of the collection.
    pub fn scene(&self, canvas: (u32, u32), taken: impl Fn(SourceId) -> bool) -> SceneDesc {
        let [background, media, text] = free_ids(taken);
        let s = &self.look;
        let item = |source: SourceId, h: &SourceHandle, transform: Transform| ItemDesc {
            source,
            video: Arc::clone(&h.io.video),
            transform,
            effects: Vec::new(),
        };
        let mut items = vec![item(background, &self.background, Transform::default())];
        let (media_size, text_size) = self.sizes();
        if let (Some(m), Some(size)) = (&self.media, media_size) {
            items.push(item(media, m, s.fit.place(Transform::default(), size, canvas)));
        }
        if let (Some(t), Some(size)) = (&self.text, text_size) {
            items.push(item(text, t, Transform { position: text_position(s, canvas, size), ..Transform::default() }));
        }
        SceneDesc { items }
    }
}

/// `now`, when it is an error other than the one in `told`, which it replaces. The same error
/// repeated by each retry is told once; an error after the media showed again is told again.
fn untold(told: &mut Option<String>, now: Option<String>) -> Option<String> {
    if now.is_none() {
        *told = None;
        return None;
    }
    if *told == now {
        return None;
    }
    told.clone_from(&now);
    now
}

/// Where the countdown text of `size` goes on a `canvas`: at its anchor inside a margin of 5% of
/// each side.
pub(crate) fn text_position(s: &PauseSettings, canvas: (u32, u32), size: (u32, u32)) -> Vec2 {
    let (cw, ch) = (canvas.0 as f32, canvas.1 as f32);
    let (mx, my) = (cw * MARGIN, ch * MARGIN);
    let (fx, fy) = s.anchor.fractions();
    let room = |side: f32, margin: f32, own: f32| (side - 2.0 * margin - own).max(0.0);
    Vec2::new((mx + fx * room(cw, mx, size.0 as f32)).round(), (my + fy * room(ch, my, size.1 as f32)).round())
}

/// The text's size in pixels: its share of the canvas height, from 6 to 1,024 pixels.
pub(crate) fn text_px(s: &PauseSettings, canvas: (u32, u32)) -> u32 {
    (u64::from(canvas.1) * u64::from(s.size_pct) / 100).clamp(6, 1024) as u32
}

fn background_settings(s: &PauseSettings, canvas: (u32, u32)) -> Settings {
    Settings::new().with("color", s.background.clone()).with("width", canvas.0.max(1)).with("height", canvas.1.max(1))
}

fn media_settings(kind: &str, path: &str) -> Settings {
    let s = Settings::new().with("file", path);
    if kind == "media" { s.with("loop", true) } else { s }
}

fn text_settings(s: &PauseSettings, canvas: (u32, u32), text: &str) -> Settings {
    Settings::new()
        .with("text", text)
        .with("font", s.font.clone())
        .with("size", text_px(s, canvas))
        .with("color", s.color.clone())
        .with("align", "center")
        .with("outline", s.outline)
        .with("outline_color", s.outline_color.clone())
}

#[cfg(test)]
mod tests {
    use std::sync::RwLock;

    use studiocraft_model::{FitMode, PauseAnchor};

    use super::*;
    use crate::{CaptionBoard, builtin_registry};

    fn settings(countdown_s: u32, at_zero: PauseAtZero) -> PauseSettings {
        PauseSettings { countdown_s, at_zero, ..PauseSettings::default() }
    }

    #[test]
    fn the_countdown_rounds_up_and_shows_hours_from_one_hour() {
        assert_eq!(countdown_text(0), "0:00");
        assert_eq!(countdown_text(1), "0:01", "the last second shows until it has passed");
        assert_eq!(countdown_text(1000), "0:01");
        assert_eq!(countdown_text(1001), "0:02");
        assert_eq!(countdown_text(247_000), "4:07");
        assert_eq!(countdown_text(3_599_000), "59:59");
        assert_eq!(countdown_text(3_599_001), "1:00:00");
        assert_eq!(countdown_text(3_727_000), "1:02:07");
        assert_eq!(countdown_text(43_200_000), "12:00:00");
        assert_eq!(countdown_text(u64::MAX), countdown_text(u64::MAX), "no overflow");
        assert_eq!(paused_text(1999), "0:01", "the time paused rounds down");
    }

    #[test]
    fn the_countdown_counts_down_and_reaches_zero_once() {
        let mut p = PauseState::new(10_000, settings(3, PauseAtZero::Keep));
        assert_eq!(p.text(10_000).as_deref(), Some("Back in 0:03"));
        assert_eq!(p.text(11_500).as_deref(), Some("Back in 0:02"));
        assert_eq!(p.text(12_999).as_deref(), Some("Back in 0:01"));
        assert_eq!(p.step(12_999), None);
        assert_eq!(p.step(13_000), Some(Zero::Keep));
        assert_eq!(p.step(14_000), None, "zero is reached once");
        assert_eq!(p.text(13_000).as_deref(), Some("Back in a moment"));
        assert!(p.reached);
        let mut r = PauseState::new(0, settings(1, PauseAtZero::Resume));
        assert_eq!(r.step(5_000), Some(Zero::Resume), "a late tick still reaches zero");
        assert_eq!(r.step(6_000), None);
    }

    #[test]
    fn no_countdown_shows_no_text_and_never_reaches_zero() {
        let mut p = PauseState::new(0, settings(0, PauseAtZero::Resume));
        assert_eq!((p.text(0), p.deadline_ms, p.step(u64::MAX)), (None, None, None));
        assert_eq!(p.json(5_000, None, None)["countdown"], Value::Null);
    }

    #[test]
    fn adding_time_extends_the_countdown_up_to_twelve_hours() {
        let mut p = PauseState::new(0, settings(60, PauseAtZero::Keep));
        p.extend(30_000, 60);
        assert_eq!(p.remaining_ms(30_000), Some(90_000));
        // At zero, the added time counts from now and the zero step can run again.
        let mut z = PauseState::new(0, settings(1, PauseAtZero::Keep));
        assert_eq!(z.step(2_000), Some(Zero::Keep));
        z.extend(5_000, 60);
        assert_eq!((z.remaining_ms(5_000), z.reached), (Some(60_000), false));
        assert_eq!(z.step(65_000), Some(Zero::Keep));
        // Without a countdown, adding time starts one.
        let mut n = PauseState::new(0, settings(0, PauseAtZero::Keep));
        n.extend(1_000, 120);
        assert_eq!((n.remaining_ms(1_000), n.total_s), (Some(120_000), 120));
        // The time left never passes 12 hours.
        let mut big = PauseState::new(0, settings(43_200, PauseAtZero::Keep));
        big.extend(0, 43_200);
        assert_eq!((big.remaining_ms(0), big.total_s), (Some(43_200_000), 43_200));
    }

    #[test]
    fn a_new_countdown_counts_from_now() {
        let mut p = PauseState::new(0, settings(300, PauseAtZero::Keep));
        assert_eq!(p.step(300_000), Some(Zero::Keep));
        p.set_countdown(400_000, 10);
        assert_eq!((p.remaining_ms(400_000), p.reached, p.total_s), (Some(10_000), false, 10));
        p.set_countdown(400_000, 0);
        assert_eq!(p.text(400_000), None);
    }

    #[test]
    fn the_status_reports_the_countdown() {
        let p = PauseState::new(1_000, PauseSettings { media: "a.png".into(), ..settings(300, PauseAtZero::Resume) });
        let v = p.json(61_000, Some("missing"), Some("no font"));
        assert_eq!(v["paused"], json!(true));
        assert_eq!(v["since_ms"], json!(60_000));
        assert_eq!(v["countdown"], json!({"total_s": 300, "remaining_ms": 240_000, "at_zero": "resume", "reached": false}));
        assert_eq!((v["media"].as_str(), v["media_error"].as_str()), (Some("a.png"), Some("missing")));
        assert_eq!(v["text_error"].as_str(), Some("no font"));
    }

    #[test]
    fn media_files_are_pictures_or_videos() {
        let reg = builtin_registry(&Arc::new(RwLock::new(String::new())), &Arc::new(CaptionBoard::new())).unwrap();
        assert_eq!(media_kind(&reg, "C:/a/break.PNG"), Ok("image"));
        assert_eq!(media_kind(&reg, "break.jpeg"), Ok("image"));
        assert_eq!(media_kind(&reg, "/v/loop.mp4"), Ok("media"));
        assert_eq!(media_kind(&reg, "loop.webm"), Ok("media"));
        for bad in ["song.mp3", "voice.wav", "notes.txt", "noextension", ".png", ""] {
            assert!(media_kind(&reg, bad).is_err(), "{bad}");
        }
        let offered = media_extensions(&reg);
        assert!(["png", "jpg", "gif", "mp4", "webm"].iter().all(|e| offered.contains(&(*e).to_string())), "{offered:?}");
        assert!(SOUND_ONLY.iter().all(|e| !offered.contains(&(*e).to_string())), "{offered:?}");
        assert!(offered.windows(2).all(|w| w[0] < w[1]), "sorted once each: {offered:?}");
    }

    #[test]
    fn the_text_sits_at_its_anchor_inside_the_margin() {
        let s = |anchor| PauseSettings { anchor, ..PauseSettings::default() };
        let canvas = (1920, 1080);
        assert_eq!(text_position(&s(PauseAnchor::TopLeft), canvas, (200, 50)), Vec2::new(96.0, 54.0));
        assert_eq!(text_position(&s(PauseAnchor::BottomCenter), canvas, (200, 50)), Vec2::new(860.0, 976.0));
        assert_eq!(text_position(&s(PauseAnchor::BottomRight), canvas, (200, 50)), Vec2::new(1624.0, 976.0));
        // Text wider than the room inside the margins starts at the margin.
        assert_eq!(text_position(&s(PauseAnchor::Center), canvas, (4000, 50)).x, 96.0);
        assert_eq!(text_px(&PauseSettings::default(), canvas), 64, "6% of 1080");
        assert_eq!(text_px(&PauseSettings { size_pct: 2, ..PauseSettings::default() }, (16, 16)), 6, "at least 6 pixels");
        assert_eq!(text_px(&PauseSettings { size_pct: 15, ..PauseSettings::default() }, (8192, 8192)), 1024, "at most 1024 pixels");
    }

    #[test]
    fn a_media_error_is_told_once_until_the_media_shows_again() {
        let mut told = None;
        assert_eq!(untold(&mut told, Some("gone".into())).as_deref(), Some("gone"));
        assert_eq!(untold(&mut told, Some("gone".into())), None, "each retry repeats it");
        assert_eq!(untold(&mut told, Some("bad".into())).as_deref(), Some("bad"), "a new error is told");
        assert_eq!(untold(&mut told, None), None);
        assert_eq!(untold(&mut told, Some("bad".into())).as_deref(), Some("bad"), "after the media showed again");
    }

    #[test]
    fn the_screen_draws_the_background_first_and_places_media_and_text_once_shown() {
        let reg = builtin_registry(&Arc::new(RwLock::new(String::new())), &Arc::new(CaptionBoard::new())).unwrap();
        let s = PauseSettings { media: String::new(), fit: FitMode::Fill, ..PauseSettings::default() };
        let (layers, problems) = Layers::start(&reg, MasterClock::new(), &s, (1280, 720), None);
        assert!(problems.is_empty(), "{problems:?}");
        let Some(mut layers) = layers else { panic!("no layers") };
        let scene = layers.scene((1280, 720), |_| false);
        assert_eq!(scene.items.len(), 1, "only the background without media or text");
        assert_eq!(scene.items[0].source, BACKGROUND);
        assert_eq!(scene.items[0].transform, Transform::default());
        // An unknown file type is refused without stopping the screen.
        let mut problems = Vec::new();
        layers.set_media(&reg, MasterClock::new(), "notes.txt", &mut problems);
        assert_eq!(problems.len(), 1);
        assert!(layers.media.is_none());
        assert_eq!(layers.media_error(), problems.first().cloned(), "the status reports why");
        assert!(layers.untold_errors().is_empty(), "the problem was told when it was returned");
        // A picture that does not exist is shown as an error, and the screen keeps its background.
        layers.set_media(&reg, MasterClock::new(), "Z:/missing/break.png", &mut problems);
        assert!(layers.media.is_some());
        assert_eq!(layers.scene((1280, 720), |_| false).items.len(), 1, "media that shows nothing is not placed");
        // A source of the collection under the background's usual id moves the background.
        let taken = |id: SourceId| id == BACKGROUND;
        assert_eq!(layers.scene((1280, 720), taken).items[0].source, MEDIA);
    }

    #[test]
    fn the_layers_take_the_largest_ids_no_source_holds() {
        let top = |n: u64| SourceId(u64::MAX - n);
        assert_eq!(free_ids(|_| false), [BACKGROUND, MEDIA, TEXT]);
        assert_eq!([BACKGROUND, MEDIA, TEXT], [top(0), top(1), top(2)]);
        let taken = [top(0), top(2), top(3)];
        assert_eq!(free_ids(|id| taken.contains(&id)), [top(1), top(4), top(5)]);
    }

    #[test]
    fn a_countdown_text_that_cannot_show_is_told_once() {
        let all = builtin_registry(&Arc::new(RwLock::new(String::new())), &Arc::new(CaptionBoard::new())).unwrap();
        let mut reg = Registry::new();
        let Some(color) = all.sources().iter().find(|p| p.id() == "color") else { panic!("no color source") };
        reg.register_source(color.clone()).unwrap();
        let s = PauseSettings::default();
        let (layers, problems) = Layers::start(&reg, MasterClock::new(), &s, (1280, 720), Some("Back in 5:00"));
        assert!(problems.is_empty(), "{problems:?}");
        let Some(mut layers) = layers else { panic!("no layers") };
        assert!(layers.text_error().is_some());
        let told = layers.untold_errors();
        assert_eq!(told.len(), 1, "{told:?}");
        assert_eq!(told[0].0, TEXT_NOTICE);
        assert!(layers.untold_errors().is_empty(), "the same error is told once");
        // The same text does not try again; a new text does, and fails the same way.
        layers.set_text(&reg, MasterClock::new(), &s, (1280, 720), Some("Back in 4:59"));
        assert!(layers.untold_errors().is_empty());
        // Without a countdown there is no text and no error.
        layers.set_text(&reg, MasterClock::new(), &s, (1280, 720), None);
        assert_eq!(layers.text_error(), None);
    }
}
