//! The Health dock: one column with the state of the stream's destinations, the encoder, the
//! audio tracks, the replay buffer, live captions and the program's loudness, and the pause
//! screen while the program is paused. The Live layout shows it in the right column.
//!
//! Every value is read each frame from counters and state the studio already keeps, so drawing
//! the dock does not wait on an output, a file or the network. [`sections`] builds what the dock
//! shows from those values, and [`dock`] draws it.

use craft_behavior::ColumnWidth;
use craft_theme::variant::Tone;
use craft_ui::buttons::Button;
use craft_ui::contract::{Props as _, Show as _, ShowContainer as _};
use craft_ui::display::{Divider, Elide, Shade, Text};
use craft_ui::layout_components::{Grid, Row};
use craft_ui::style::TypeRole;
use egui::{Rect, Sense, Ui, pos2, vec2};
use serde_json::{Value, json};
use studiocraft_engine::mixer::LoudnessReading;
use studiocraft_media::stats::Snapshot;
use studiocraft_model::Destination;
use studiocraft_model::profile::LoudnessTarget;
use studiocraft_plugin::{DestinationState, DestinationStatus};
use studiocraft_studio::{AudioTracksState, CaptionState, OutputKind, OutputState, Studio};

use crate::docks::{self, DockState, Drawn};
use crate::kit::tokens::{MarkColors, MeterColors, paint};
use crate::{Act, theme};

/// The quietest loudness the loudness bar shows, in LUFS; its loud end is 0 LUFS.
pub(crate) const BAR_FLOOR_LUFS: f64 = -40.0;

/// A button of the dock: its name in `ui.inspect`, its label in the interface language, the
/// command it runs with its parameters, and whether it can be pressed.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct HealthButton {
    pub name: &'static str,
    pub label: String,
    pub command: &'static str,
    pub params: Value,
    pub enabled: bool,
}

impl HealthButton {
    fn new(name: &'static str, label: &str, command: &'static str, params: Value) -> HealthButton {
        HealthButton { name, label: label.to_string(), command, params, enabled: true }
    }

    fn enabled(mut self, on: bool) -> HealthButton {
        self.enabled = on;
        self
    }

    fn json(&self) -> Value {
        json!({"name": self.name, "label": self.label, "command": self.command, "params": self.params, "enabled": self.enabled})
    }
}

/// A line of a section: a label, its value in `tone`'s color, and a button at its end.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Line {
    pub label: String,
    pub value: String,
    pub tone: Tone,
    pub button: Option<HealthButton>,
}

impl Line {
    fn new(label: &str, value: String) -> Line {
        Line { label: label.to_string(), value, tone: Tone::Neutral, button: None }
    }

    fn tone(mut self, tone: Tone) -> Line {
        self.tone = tone;
        self
    }
}

/// The loudness bar: the short-term loudness when there is one, and the target band, in LUFS.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Band {
    pub value: Option<f64>,
    pub low: f64,
    pub high: f64,
}

/// A section of the dock: its name in `ui.inspect`, its title, its state in `tone`'s color, its
/// lines, its buttons under them, a button beside its title, and the loudness bar.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Section {
    pub key: &'static str,
    pub title: String,
    pub status: String,
    pub tone: Tone,
    pub lines: Vec<Line>,
    pub buttons: Vec<HealthButton>,
    pub header: Option<HealthButton>,
    pub band: Option<Band>,
}

impl Section {
    fn new(key: &'static str, title: &str, status: String, tone: Tone) -> Section {
        Section { key, title: title.to_string(), status, tone, lines: Vec::new(), buttons: Vec::new(), header: None, band: None }
    }

    fn json(&self) -> Value {
        let lines: Vec<Value> =
            self.lines.iter().map(|l| json!({"label": l.label, "value": l.value, "button": l.button.as_ref().map(HealthButton::json)})).collect();
        json!({
            "section": self.key,
            "title": self.title,
            "status": self.status,
            "lines": lines,
            "buttons": self.buttons.iter().chain(&self.header).map(HealthButton::json).collect::<Vec<_>>(),
        })
    }
}

/// The audio tracks' values the dock shows, read from the take's status.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Tracks {
    /// The status bar's text for the take and whether it is a warning, while it shows one.
    pub item: Option<(String, bool)>,
    pub state: Option<AudioTracksState>,
    pub tracks: u64,
    pub bytes: u64,
    pub free_bytes: u64,
    pub dropped_seconds: f64,
}

/// What the dock is built from.
pub(crate) struct Facts<'a> {
    /// The pause's countdown time left (`None` without a countdown) while the program is paused,
    /// and how long it has been paused.
    pub paused: Option<Option<u64>>,
    pub paused_for_ms: Option<u64>,
    /// The profile's destinations and the running stream's own, with the stream's state.
    pub destinations: &'a [Destination],
    pub live: &'a [DestinationState],
    pub stream: Option<&'a OutputState>,
    pub recording: Option<&'a OutputState>,
    pub replay: Option<&'a OutputState>,
    pub stats: Snapshot,
    /// The newest take's values, when a take ran this session, and whether a finished take can
    /// be opened in the editor.
    pub tracks: Option<Tracks>,
    pub last_session: bool,
    pub captions: &'a CaptionState,
    pub loudness: LoudnessReading,
    pub target: LoudnessTarget,
}

/// The dock's sections, in the order they are drawn: the pause screen while the program is
/// paused, then destinations, encoder, audio tracks, replay buffer, live captions and loudness.
pub(crate) fn sections(f: &Facts) -> Vec<Section> {
    let mut out: Vec<Section> = pause(f).into_iter().collect();
    out.extend([destinations(f), encoder(f), tracks(f), replay(f), captions(f), loudness(f)]);
    out
}

fn pause(f: &Facts) -> Option<Section> {
    let (text, _) = crate::pause_banner::text(f.paused, f.paused_for_ms)?;
    let mut s = Section::new("pause", tl!("Pause screen"), text, Tone::Warn);
    s.buttons = vec![
        HealthButton::new("extend", tl!("+1 min"), "pause.extend", json!({"seconds": 60})),
        HealthButton::new("resume", tl!("Resume"), "pause.stop", json!({})),
    ];
    Some(s)
}

/// The destinations section: how many of the running stream's destinations are live, and a line
/// for each of the profile's destinations with its state. While the stream runs, a destination
/// it sends to has Stop while it sends or tries to and Start otherwise.
fn destinations(f: &Facts) -> Section {
    let running = f.stream.is_some_and(|o| o.active);
    let live_now = f.live.iter().filter(|s| matches!(s.status, DestinationStatus::Live)).count();
    let (status, tone) = match (running, f.live.len()) {
        (true, 0) => (f.stream.map(crate::output_status).unwrap_or_default(), Tone::Neutral),
        (true, n) => {
            let text = craft_i18n::fmt(tl!("{live} of {count} live"), &[("live", &live_now.to_string()), ("count", &n.to_string())]);
            (text, if live_now == n { Tone::Good } else { Tone::Warn })
        }
        (false, _) if f.destinations.is_empty() => (tl!("No destinations yet. Add one to stream to it.").to_string(), Tone::Neutral),
        (false, _) => (tl!("Not streaming").to_string(), Tone::Neutral),
    };
    let mut s = Section::new("destinations", tl!("Destinations"), status, tone);
    s.header = Some(HealthButton::new("streaming", tl!("Streaming…"), "view.streaming", json!({})));
    let paused = f.paused.is_some();
    for d in f.destinations {
        let live = f.live.iter().find(|s| s.id == d.id);
        let (mut value, tone) = crate::streaming::status_text(d, running, live);
        if running && paused && live.is_some_and(|s| matches!(s.status, DestinationStatus::Live)) {
            value = tl!("Live · sending the pause screen").to_string();
        }
        let button = live.filter(|_| running).map(|l| {
            let params = json!({"id": d.id});
            if crate::streaming::sending(l) {
                HealthButton::new("stop", tl!("Stop"), "stream.destination_stop", params)
            } else {
                HealthButton::new("start", tl!("Start"), "stream.destination_start", params)
            }
        });
        s.lines.push(Line { button, ..Line::new(&crate::streaming::destination_title(d), value).tone(tone) });
    }
    s
}

/// The encoder section: the Stats window's frame times and counts, and the size of the running
/// recording and stream.
fn encoder(f: &Facts) -> Section {
    let st = &f.stats;
    let late = st.lagged > 0 || st.encoder_dropped > 0;
    let (status, tone) = if late { (tl!("Frames are late or skipped"), Tone::Warn) } else { (tl!("No frames late or skipped"), Tone::Good) };
    let mut s = Section::new("encoder", tl!("Encoder"), status.to_string(), tone);
    let ms = |v: f64| craft_i18n::fmt(tl!("{ms} ms"), &[("ms", &format!("{v:.2}"))]);
    let warn = |n: u64| if n > 0 { Tone::Warn } else { Tone::Neutral };
    s.lines = vec![
        Line::new(tl!("Frame time (last)"), ms(st.render_ms)),
        Line::new(tl!("Frame time (average)"), ms(st.render_avg_ms)),
        Line::new(tl!("Frames drawn late"), st.lagged.to_string()).tone(warn(st.lagged)),
        Line::new(tl!("Frames skipped by encoders"), st.encoder_dropped.to_string()).tone(warn(st.encoder_dropped)),
        Line::new(tl!("Encoder queue"), st.encoder_queued.to_string()),
    ];
    for (o, label) in [(f.recording, tl!("Recording")), (f.stream, tl!("Stream"))] {
        if let Some(o) = o {
            s.lines.push(Line::new(label, sized(o)));
        }
    }
    s
}

/// An output's state and the megabytes it has written.
fn sized(o: &OutputState) -> String {
    let mb = format!("{:.1}", o.bytes as f64 / 1e6);
    craft_i18n::fmt(tl!("{status} · {mb} MB"), &[("status", &crate::output_status(o)), ("mb", &mb)])
}

/// The audio tracks section: the status bar's item for the take while it shows one, and the
/// take's track count, file size, free disk space and audio lost, with Open Last in Editor.
fn tracks(f: &Facts) -> Section {
    let (status, tone) = match f.tracks.as_ref().and_then(|t| t.item.clone()) {
        Some((text, true)) => (text, Tone::Warn),
        Some((text, false)) => (text, Tone::Good),
        None => (tl!("Off").to_string(), Tone::Neutral),
    };
    let mut s = Section::new("tracks", tl!("Audio tracks"), status, tone);
    if let Some(t) = f.tracks.as_ref().filter(|t| t.state.is_some()) {
        let mb = format!("{:.1}", t.bytes as f64 / 1e6);
        let gb = format!("{:.1}", t.free_bytes as f64 / 1e9);
        let lost = format!("{:.1}", t.dropped_seconds);
        s.lines = vec![
            Line::new(tl!("Tracks"), t.tracks.to_string()),
            Line::new(tl!("Files"), craft_i18n::fmt(tl!("{mb} MB"), &[("mb", &mb)])),
            Line::new(tl!("Disk free"), craft_i18n::fmt(tl!("{gb} GB"), &[("gb", &gb)])),
            Line::new(tl!("Audio lost"), craft_i18n::fmt(tl!("{lost} s"), &[("lost", &lost)])).tone(if t.dropped_seconds > 0.0 {
                Tone::Warn
            } else {
                Tone::Neutral
            }),
        ];
    }
    s.buttons =
        vec![HealthButton::new("open_last", tl!("Open Last Audio Tracks in Editor"), "audio_tracks.open_last", json!({})).enabled(f.last_session)];
    s
}

/// The replay buffer section: its state and size, Save Replay while it runs, and Start or Stop.
fn replay(f: &Facts) -> Section {
    let active = f.replay.is_some_and(|o| o.active);
    let stopping = f.replay.is_some_and(|o| o.stopping);
    let (status, tone) = match f.replay {
        Some(o) if o.active || o.stopping => (sized(o), Tone::Good),
        Some(o) => (crate::output_status(o), Tone::Neutral),
        None => (tl!("Off").to_string(), Tone::Neutral),
    };
    let mut s = Section::new("replay", tl!("Replay buffer"), status, tone);
    let toggle = if active { ("stop", tl!("Stop")) } else { ("start", tl!("Start")) };
    s.buttons = vec![
        HealthButton::new("save", tl!("Save Replay"), "replay.save", json!({})).enabled(active && !stopping),
        HealthButton::new(toggle.0, toggle.1, "replay.toggle", json!({})).enabled(!stopping),
    ];
    s
}

/// The live captions section: their state, and Start or Stop. There is no button while they
/// finish after a stop.
fn captions(f: &Facts) -> Section {
    let (status, tone, button) = match f.captions {
        CaptionState::Loading => (tl!("Loading…").to_string(), Tone::Neutral, Some(("stop", tl!("Stop")))),
        CaptionState::Running => (tl!("Running").to_string(), Tone::Good, Some(("stop", tl!("Stop")))),
        CaptionState::Stopping => (tl!("Stopping…").to_string(), Tone::Neutral, None),
        CaptionState::Stopped => (tl!("Off").to_string(), Tone::Neutral, Some(("start", tl!("Start")))),
        CaptionState::Failed(e) => (craft_i18n::fmt(tl!("Failed: {error}"), &[("error", e)]), Tone::Warn, Some(("start", tl!("Start")))),
    };
    let mut s = Section::new("captions", tl!("Live captions"), status, tone);
    s.buttons = button.map(|(name, label)| HealthButton::new(name, label, "captions.toggle", json!({}))).into_iter().collect();
    s
}

/// A loudness value with one decimal and its unit, or `-inf` for silence.
fn level(v: f64, unit: &str) -> String {
    let value = if v.is_finite() { format!("{v:.1}") } else { "-inf".to_string() };
    craft_i18n::fmt(unit, &[("value", &value)])
}

/// The program loudness section: the short-term loudness against the profile's target band, the
/// integrated loudness and the highest true peak, and the bar.
fn loudness(f: &Facts) -> Section {
    let (low, high) = (f.target.low_lufs, f.target.high_lufs);
    let short = f.loudness.short_term_lufs;
    let (status, tone) = if !short.is_finite() {
        (tl!("No program audio yet"), Tone::Neutral)
    } else if short < low {
        (tl!("Below the target band"), Tone::Warn)
    } else if short > high {
        (tl!("Above the target band"), Tone::Warn)
    } else {
        (tl!("In the target band"), Tone::Good)
    };
    let mut s = Section::new("loudness", tl!("Program loudness"), status.to_string(), tone);
    let lufs = tl!("{value} LUFS");
    s.lines = vec![
        Line::new(tl!("Short-term"), level(short, lufs)).tone(tone),
        Line::new(tl!("Integrated"), level(f.loudness.integrated_lufs, lufs)),
        Line::new(tl!("True peak"), level(f.loudness.true_peak_dbtp, tl!("{value} dBTP"))),
        Line::new(
            tl!("Target band"),
            craft_i18n::fmt(tl!("{low} to {high} LUFS"), &[("low", &format!("{low:.0}")), ("high", &format!("{high:.0}"))]),
        ),
    ];
    s.band = Some(Band { value: short.is_finite().then_some(short), low, high });
    s
}

/// Where `lufs` sits across `width` points of the loudness bar, from [`BAR_FLOOR_LUFS`] at 0 to
/// 0 LUFS at `width`.
pub(crate) fn bar_x(lufs: f64, width: f32) -> f32 {
    let v = if lufs.is_nan() { BAR_FLOOR_LUFS } else { lufs.clamp(BAR_FLOOR_LUFS, 0.0) };
    ((v - BAR_FLOOR_LUFS) / -BAR_FLOOR_LUFS) as f32 * width.max(0.0)
}

/// What the dock drew last, for `ui.inspect`: its sections, or nothing when it was not drawn.
#[derive(Clone, Debug, Default)]
pub struct Seen {
    sections: Option<Vec<Section>>,
}

impl Seen {
    pub(crate) fn json(&self) -> Value {
        self.sections.as_ref().map_or(Value::Null, |s| json!({"sections": s.iter().map(Section::json).collect::<Vec<_>>()}))
    }
}

/// The facts of `studio` for the dock.
fn tracks_of(studio: &Studio, outputs_run: bool) -> Option<Tracks> {
    let s = studio.audio_tracks_status()?;
    Some(Tracks {
        item: crate::tracks_item(s, outputs_run, studio.audio_tracks_source_limit()).map(|(text, warn, _)| (text, warn)),
        state: s.state(),
        tracks: s.tracks(),
        bytes: s.bytes(),
        free_bytes: s.free_bytes(),
        dropped_seconds: s.dropped_seconds(),
    })
}

/// The Health dock: its header, then each section with a line between two.
pub fn dock(ui: &mut Ui, studio: &Studio, st: &mut DockState, acts: &mut Vec<Act>) -> Drawn {
    let (drawn, _) = docks::title_header(ui, "Health", &[]);
    docks::dock_menu(ui, &drawn, "health", st.place, acts);
    let profile = studio.profile();
    let stream = studio.output(OutputKind::Streaming);
    let recording = studio.output(OutputKind::Recording);
    let replay = studio.output(OutputKind::Replay);
    // The running stream's destinations count only when it started with the profile in use.
    let live = if studio.stream_profile() == Some(profile.name.as_str()) { studio.stream_destinations() } else { Vec::new() };
    let outputs_run = [&recording, &stream].into_iter().flatten().any(|o| o.active);
    let captions = studio.captions_state();
    let facts = Facts {
        paused: studio.paused(),
        paused_for_ms: studio.paused_for_ms(),
        destinations: &profile.stream.destinations,
        live: &live,
        stream: stream.as_ref(),
        recording: recording.as_ref(),
        replay: replay.as_ref(),
        stats: studio.stats(),
        tracks: tracks_of(studio, outputs_run),
        last_session: studio.audio_tracks_last_session().is_some(),
        captions: &captions,
        loudness: studio.master_loudness(),
        target: profile.audio.loudness_target,
    };
    let shown = sections(&facts);
    let area = egui::ScrollArea::vertical().id_salt("health").auto_shrink([false, false]);
    area.show(&mut docks::body(ui), |ui| {
        for (k, s) in shown.iter().enumerate() {
            if k > 0 {
                Divider::horizontal().show(ui);
            }
            section(ui, s, acts);
        }
    });
    st.health.sections = Some(shown);
    // The values change while nothing else does, so the dock draws again within a quarter second.
    ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
    drawn
}

fn push(b: &HealthButton, acts: &mut Vec<Act>) {
    acts.push(Act::run(b.command, b.params.clone()));
}

/// A button of the dock; `salt` is its section, its line (`usize::MAX` for none) and its name.
fn button(ui: &mut Ui, b: &HealthButton, salt: (&'static str, usize, &'static str), acts: &mut Vec<Act>) {
    if Button::new(b.label.clone()).enabled(b.enabled).id_salt(salt).show(ui).clicked() {
        push(b, acts);
    }
}

fn section(ui: &mut Ui, s: &Section, acts: &mut Vec<Act>) {
    const COLUMNS: [ColumnWidth; 2] = [ColumnWidth::Content; 2];
    let _ = Row::new().fill_width().show(ui, |ui, _| {
        let _ = Text::new(s.title.clone()).role(TypeRole::BodyStrong).elide(Elide::End).show(ui);
        if let Some(b) = &s.header {
            button(ui, b, (s.key, usize::MAX, b.name), acts);
        }
    });
    let status = Text::new(s.status.clone());
    let _ = if s.tone == Tone::Neutral { status.shade(Shade::Muted).show(ui) } else { status.tone(s.tone).show(ui) };
    if s.lines.iter().any(|l| l.button.is_some()) {
        // A line with a button is a row of its own: the label, then the value under it.
        for (k, l) in s.lines.iter().enumerate() {
            let _ = Row::new().fill_width().show(ui, |ui, _| {
                let _ = Text::new(l.label.clone()).elide(Elide::End).show(ui);
                if let Some(b) = &l.button {
                    button(ui, b, (s.key, k, b.name), acts);
                }
            });
            let _ = Text::new(l.value.clone()).role(TypeRole::Caption).tone(l.tone).show(ui);
        }
    } else if !s.lines.is_empty() {
        let _ = Grid::new(&COLUMNS).label(s.title.clone()).show(ui, |ui, _| {
            for l in &s.lines {
                let _ = Text::new(l.label.clone()).shade(Shade::Muted).show(ui);
                let _ = Text::new(l.value.clone()).role(TypeRole::Mono).tone(l.tone).show(ui);
            }
        });
    }
    if let Some(band) = s.band {
        bar(ui, band);
    }
    if !s.buttons.is_empty() {
        let _ = Row::new().show(ui, |ui, _| {
            for b in &s.buttons {
                button(ui, b, (s.key, usize::MAX, b.name), acts);
            }
        });
    }
}

/// The loudness bar from [`BAR_FLOOR_LUFS`] to 0 LUFS: the meter track, the target band in the
/// band color, and the short-term loudness as a mark, green inside the band and yellow outside.
fn bar(ui: &mut Ui, band: Band) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width().max(0.0), 10.0), Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }
    let mode = craft_ui::runtime::env::Env::of(ui).view().mode();
    let meters = MeterColors::of(mode);
    let accent = theme::with_root(ui.ctx(), |cx| theme::color(cx, cx.tokens.color.accent.text));
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, paint(meters.track));
    let x = |v: f64| rect.left() + bar_x(v, rect.width());
    let zone = Rect::from_min_max(pos2(x(band.low), rect.top()), pos2(x(band.high), rect.bottom()));
    painter.rect_filled(zone, 0.0, paint(MarkColors::of(mode).meter_target(accent)));
    if let Some(v) = band.value {
        let color = if (band.low..=band.high).contains(&v) { paint(meters.green) } else { paint(meters.yellow) };
        let at = x(v);
        painter.line_segment([pos2(at, rect.top()), pos2(at, rect.bottom())], (2.0, color));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use studiocraft_plugin::{DestinationCounts, OutputStatus};

    fn reading(short: f64) -> LoudnessReading {
        LoudnessReading { momentary_lufs: short, short_term_lufs: short, integrated_lufs: short, range_lu: 0.0, true_peak_dbtp: -1.4, generation: 1 }
    }

    fn output(active: bool, bytes: u64) -> OutputState {
        OutputState {
            active,
            stopping: false,
            status: if active { OutputStatus::Running } else { OutputStatus::Stopped },
            target: String::new(),
            elapsed: Duration::from_secs(30),
            bytes,
            destinations: DestinationCounts::default(),
        }
    }

    fn destination(id: &str, name: &str) -> Destination {
        Destination { id: id.into(), name: name.into(), enabled: true, ..Destination::default() }
    }

    fn state(id: &str, status: DestinationStatus) -> DestinationState {
        DestinationState { id: id.into(), target: String::new(), status, bytes: 0, dropped: 0, reconnects: 0 }
    }

    fn facts<'a>(destinations: &'a [Destination], live: &'a [DestinationState], captions: &'a CaptionState) -> Facts<'a> {
        Facts {
            paused: None,
            paused_for_ms: None,
            destinations,
            live,
            stream: None,
            recording: None,
            replay: None,
            stats: Snapshot::default(),
            tracks: None,
            last_session: false,
            captions,
            loudness: reading(f64::NEG_INFINITY),
            target: LoudnessTarget::default(),
        }
    }

    fn of<'a>(all: &'a [Section], key: &str) -> &'a Section {
        all.iter().find(|s| s.key == key).unwrap_or_else(|| panic!("no {key} section"))
    }

    #[test]
    fn the_sections_follow_one_another_and_the_pause_screen_shows_only_while_paused() {
        let stopped = CaptionState::Stopped;
        let mut f = facts(&[], &[], &stopped);
        let keys = |f: &Facts| sections(f).iter().map(|s| s.key).collect::<Vec<_>>();
        assert_eq!(keys(&f), ["destinations", "encoder", "tracks", "replay", "captions", "loudness"]);
        f.paused = Some(None);
        f.paused_for_ms = Some(5_000);
        let all = sections(&f);
        assert_eq!(all.iter().map(|s| s.key).collect::<Vec<_>>()[..2], ["pause", "destinations"]);
        let pause = of(&all, "pause");
        assert_eq!(pause.status, "Paused for 0:05. Viewers see the pause screen.");
        let commands: Vec<_> = pause.buttons.iter().map(|b| (b.command, b.params.clone())).collect();
        assert_eq!(commands, [("pause.extend", json!({"seconds": 60})), ("pause.stop", json!({}))]);
    }

    #[test]
    fn destinations_count_the_live_ones_and_each_has_stop_or_start_while_the_stream_runs() {
        let stopped = CaptionState::Stopped;
        let ds = [destination("main", "Main"), destination("backup", "Backup"), destination("spare", "")];
        let live = [state("main", DestinationStatus::Live), state("backup", DestinationStatus::Stopped)];
        let mut f = facts(&ds, &live, &stopped);
        let s = destinations(&f);
        assert_eq!((s.status.as_str(), s.tone), ("Not streaming", Tone::Neutral), "the stream is not running");
        assert!(s.lines.iter().all(|l| l.button.is_none()), "no Stop or Start while it is not running");
        let stream = output(true, 0);
        f.stream = Some(&stream);
        let s = destinations(&f);
        assert_eq!((s.status.as_str(), s.tone), ("1 of 2 live", Tone::Warn));
        let rows: Vec<_> =
            s.lines.iter().map(|l| (l.label.as_str(), l.value.as_str(), l.button.as_ref().map(|b| (b.command, b.params.clone())))).collect();
        assert_eq!(
            rows,
            [
                ("Main", "Live", Some(("stream.destination_stop", json!({"id": "main"})))),
                ("Backup", "Stopped", Some(("stream.destination_start", json!({"id": "backup"})))),
                ("spare", "Not in this stream; it is sent to from the next start", None),
            ]
        );
        assert_eq!(s.header.as_ref().map(|b| b.command), Some("view.streaming"));
        // While paused, a live destination sends the pause screen.
        f.paused = Some(None);
        assert_eq!(destinations(&f).lines[0].value, "Live · sending the pause screen");
        let all_live = [state("main", DestinationStatus::Live)];
        f.live = &all_live;
        assert_eq!((destinations(&f).status.as_str(), destinations(&f).tone), ("1 of 1 live", Tone::Good));
        // A stream started with another profile shows its own state.
        f.live = &[];
        assert_eq!(destinations(&f).status, "Running");
        let none = facts(&[], &[], &stopped);
        assert_eq!(destinations(&none).status, "No destinations yet. Add one to stream to it.");
    }

    #[test]
    fn the_encoder_warns_of_late_and_skipped_frames_and_lists_the_running_outputs() {
        let stopped = CaptionState::Stopped;
        let mut f = facts(&[], &[], &stopped);
        let s = encoder(&f);
        assert_eq!((s.status.as_str(), s.tone, s.lines.len()), ("No frames late or skipped", Tone::Good, 5));
        f.stats = Snapshot { render_ms: 4.256, encoder_dropped: 3, encoder_queued: 2, ..Snapshot::default() };
        let rec = output(true, 12_300_000);
        f.recording = Some(&rec);
        let s = encoder(&f);
        assert_eq!((s.status.as_str(), s.tone), ("Frames are late or skipped", Tone::Warn));
        let lines: Vec<_> = s.lines.iter().map(|l| (l.label.as_str(), l.value.as_str(), l.tone)).collect();
        assert_eq!(lines[0], ("Frame time (last)", "4.26 ms", Tone::Neutral));
        assert_eq!(lines[3], ("Frames skipped by encoders", "3", Tone::Warn));
        assert_eq!(lines[5], ("Recording", "Running · 12.3 MB", Tone::Neutral));
    }

    #[test]
    fn audio_tracks_show_the_take_and_open_last_is_enabled_only_with_a_finished_take() {
        let stopped = CaptionState::Stopped;
        let mut f = facts(&[], &[], &stopped);
        let s = tracks(&f);
        assert_eq!((s.status.as_str(), s.lines.len(), s.buttons[0].enabled), ("Off", 0, false));
        f.tracks = Some(Tracks {
            item: Some(("TRACKS 00:01:05".into(), false)),
            state: Some(AudioTracksState::Recording),
            tracks: 5,
            bytes: 41_600_000,
            free_bytes: 212_000_000_000,
            dropped_seconds: 0.0,
        });
        f.last_session = true;
        let s = tracks(&f);
        assert_eq!((s.status.as_str(), s.tone), ("TRACKS 00:01:05", Tone::Good));
        let lines: Vec<_> = s.lines.iter().map(|l| (l.label.as_str(), l.value.as_str())).collect();
        assert_eq!(lines, [("Tracks", "5"), ("Files", "41.6 MB"), ("Disk free", "212.0 GB"), ("Audio lost", "0.0 s")]);
        assert_eq!((s.buttons[0].command, s.buttons[0].enabled), ("audio_tracks.open_last", true));
    }

    #[test]
    fn replay_and_captions_offer_the_buttons_their_state_allows() {
        let stopped = CaptionState::Stopped;
        let mut f = facts(&[], &[], &stopped);
        let names = |s: &Section| s.buttons.iter().map(|b| (b.name, b.command, b.enabled)).collect::<Vec<_>>();
        assert_eq!(names(&replay(&f)), [("save", "replay.save", false), ("start", "replay.toggle", true)]);
        let on = output(true, 2_000_000);
        f.replay = Some(&on);
        let s = replay(&f);
        assert_eq!(s.status, "Running · 2.0 MB");
        assert_eq!(names(&s), [("save", "replay.save", true), ("stop", "replay.toggle", true)]);
        let finishing = OutputState { stopping: true, ..output(true, 0) };
        f.replay = Some(&finishing);
        assert_eq!(names(&replay(&f)), [("save", "replay.save", false), ("stop", "replay.toggle", false)], "nothing while it stops");

        let cases = [
            (CaptionState::Loading, "Loading…", Some("stop")),
            (CaptionState::Running, "Running", Some("stop")),
            (CaptionState::Stopping, "Stopping…", None),
            (CaptionState::Stopped, "Off", Some("start")),
            (CaptionState::Failed("no model".into()), "Failed: no model", Some("start")),
        ];
        for (state, status, button) in cases {
            let s = captions(&facts(&[], &[], &state));
            assert_eq!((s.status.as_str(), s.buttons.first().map(|b| b.name)), (status, button), "{state:?}");
            assert!(s.buttons.iter().all(|b| b.command == "captions.toggle"));
        }
    }

    #[test]
    fn loudness_is_judged_against_the_target_band() {
        let stopped = CaptionState::Stopped;
        let mut f = facts(&[], &[], &stopped);
        let judged = |f: &Facts| {
            let s = loudness(f);
            (s.status, s.tone, s.band.and_then(|b| b.value))
        };
        assert_eq!(judged(&f), ("No program audio yet".to_string(), Tone::Neutral, None), "silence");
        for (v, status, tone) in [
            (-16.1, "In the target band", Tone::Good),
            (-18.0, "In the target band", Tone::Good),
            (-14.0, "In the target band", Tone::Good),
            (-22.4, "Below the target band", Tone::Warn),
            (-9.0, "Above the target band", Tone::Warn),
        ] {
            f.loudness = reading(v);
            assert_eq!(judged(&f), (status.to_string(), tone, Some(v)), "{v}");
        }
        f.loudness = reading(-16.1);
        let lines: Vec<_> = loudness(&f).lines.iter().map(|l| (l.label.clone(), l.value.clone())).collect();
        assert_eq!(lines[0], ("Short-term".to_string(), "-16.1 LUFS".to_string()));
        assert_eq!(lines[2], ("True peak".to_string(), "-1.4 dBTP".to_string()));
        assert_eq!(lines[3], ("Target band".to_string(), "-18 to -14 LUFS".to_string()));
        f.target = LoudnessTarget { low_lufs: -24.0, high_lufs: -22.0 };
        assert_eq!(judged(&f).0, "Above the target band", "the profile's own band");
    }

    #[test]
    fn the_bar_runs_from_minus_40_to_0_lufs_and_holds_values_outside_it() {
        assert_eq!([bar_x(-40.0, 200.0), bar_x(-20.0, 200.0), bar_x(0.0, 200.0)], [0.0, 100.0, 200.0]);
        assert_eq!([bar_x(-90.0, 200.0), bar_x(6.0, 200.0), bar_x(f64::NEG_INFINITY, 200.0)], [0.0, 200.0, 0.0]);
        assert_eq!([bar_x(f64::NAN, 200.0), bar_x(-20.0, -5.0)], [0.0, 0.0], "a damaged value or width");
    }
}
