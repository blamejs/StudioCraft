//! The top row of the window: the menu titles on the left and the output bar on the right, with
//! the Edit and Studio switch, the transition picker and its duration, the Record and Go Live
//! buttons with their timers, and the Pause button with its menu arrow.
//!
//! When the row is too narrow, the controls give up room in this order: the transition picker
//! and its duration move behind one icon button that opens them in a panel, then Record, Go Live
//! and Pause leave their labels out and keep the glyph and, while running, the timer, and last the
//! menu titles move into the menu bar's overflow menu.

use craft_behavior::fit::{FitItem, FitKeep, fit_items};
use craft_keymap::CommandRef;
use craft_menu::{MenuBar, MenuBarUi};
use craft_ui::contract::Tracked;
use egui::{Rect, TextStyle, Ui, pos2, vec2};
use serde_json::{Value, json};
use studiocraft_model::SettingsSchema;
use studiocraft_studio::{OutputKind, OutputState, Studio};

use crate::icons::{self, Icon};
use crate::kit::onair::{Glyph, MenuArrow, OnAirButton, OnAirState, OnAirStyle};
use crate::kit::segmented::{Segmented, SegmentedStyle};
use crate::{Act, StudioApp, forms, i18n, theme};

/// The space between groups of the bar.
pub const GROUP_GAP: f32 = 8.0;
/// The space before Record.
pub const OUTPUT_GAP: f32 = 12.0;
/// The space after the last control.
pub const MARGIN: f32 = 8.0;

/// Where the transition picker and its duration are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    /// In the bar.
    Inline,
    /// Behind the icon button.
    Overflow,
    /// Docks › Transition Controls is off.
    Hidden,
}

impl Place {
    pub fn name(self) -> &'static str {
        match self {
            Place::Inline => "inline",
            Place::Overflow => "overflow",
            Place::Hidden => "hidden",
        }
    }
}

/// The widths of the bar's groups in points. A group that Docks hides has width 0.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Widths {
    /// The Edit and Studio switch.
    pub mode: f32,
    /// The transition picker, its duration and its settings button.
    pub transitions: f32,
    /// The icon button that opens the transition panel.
    pub overflow: f32,
    pub record: f32,
    pub live: f32,
    /// The Pause button with its arrow.
    pub pause: f32,
    /// Record, Go Live and Pause without their labels.
    pub record_compact: f32,
    pub live_compact: f32,
    pub pause_compact: f32,
}

impl Widths {
    /// The width of everything right of the menu titles, with the transitions `at` that place and
    /// Record, Go Live and Pause without labels when `compact`.
    pub fn outputs(&self, at: Place, compact: bool) -> f32 {
        let (record, live, pause) =
            if compact { (self.record_compact, self.live_compact, self.pause_compact) } else { (self.record, self.live, self.pause) };
        let transitions = match at {
            Place::Inline => self.transitions,
            Place::Overflow => self.overflow,
            Place::Hidden => 0.0,
        };
        let mut total = 0.0;
        let mut any = false;
        for (w, gap) in [(self.mode, GROUP_GAP), (transitions, GROUP_GAP), (record, OUTPUT_GAP), (live, GROUP_GAP), (pause, GROUP_GAP)] {
            if w > 0.0 {
                total += if any { gap + w } else { w };
                any = true;
            }
        }
        if any { total + MARGIN } else { 0.0 }
    }
}

/// How the row is shared: where the transitions go, whether Record and Go Live are compact, and
/// the widths of the menu titles and of the output bar.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fit {
    pub transitions: Place,
    pub compact: bool,
    pub menus: f32,
    pub outputs: f32,
}

/// Share a row `room` points wide between menu titles that need `menus` points and the output bar.
pub fn fit(room: f32, menus: f32, w: &Widths) -> Fit {
    let room = if room.is_finite() { room.max(0.0) } else { 0.0 };
    let menus = if menus.is_finite() { menus.max(0.0) } else { 0.0 };
    let done = |transitions, compact| {
        let outputs = w.outputs(transitions, compact);
        Fit { transitions, compact, menus: menus.min((room - outputs).max(0.0)), outputs }
    };
    let mut at = if w.transitions > 0.0 { Place::Inline } else { Place::Hidden };
    if at == Place::Inline {
        // The transitions are the one group that can move into an overflow button; the rest of
        // the bar is one group that stays.
        let fixed = w.outputs(Place::Hidden, false).max(1.0);
        let overflow = w.overflow.max(1.0);
        let available = (room - menus).max(0.0);
        let items = [FitItem::new(fixed, 0, FitKeep::Pinned), FitItem::new(w.transitions, 0, FitKeep::Free)];
        let toolbar = fit_items(&items, available, GROUP_GAP, GROUP_GAP, overflow);
        if toolbar.overflow.is_empty() {
            return done(Place::Inline, false);
        }
        // The rest of the bar stays even when it does not fit beside the overflow button; then
        // Record and Go Live also leave their labels out.
        if f64::from(fixed) + f64::from(GROUP_GAP) <= f64::from(available) - f64::from(overflow) {
            return done(Place::Overflow, false);
        }
        at = Place::Overflow;
    } else if menus + w.outputs(at, false) <= room {
        return done(at, false);
    }
    done(at, true)
}

// port: P9 (output bar) measures the menu titles with the framework's menu bar instead.

/// The width the menu titles of `bar` take as the bar shows them, each with the button padding on
/// both sides.
pub fn menu_width(ui: &Ui, bar: &MenuBar) -> f32 {
    let font = TextStyle::Button.resolve(ui.style());
    let padding = ui.spacing().button_padding.x;
    let color = ui.visuals().text_color();
    // A title with no letter a keyboard types shows its English mnemonic after it: `ファイル(F)`.
    // craft-menu writes that suffix into the title text on every platform, macOS included; macOS
    // only leaves out the underline. `M` stands in for the letter, which is not known here.
    let shown = |t: &str| {
        if t.chars().any(|c| c.is_ascii_alphanumeric()) || crate::commands::english_label(craft_menu::Labeled::Title(t)).is_none() {
            t.to_string()
        } else {
            format!("{t}(M)")
        }
    };
    let titles: f32 = bar.menus.iter().map(|m| ui.painter().layout_no_wrap(shown(&m.title), font.clone(), color).size().x + 2.0 * padding).sum();
    titles + 0.5
}

/// What an output button shows.
#[derive(Clone, Debug, PartialEq)]
pub struct Face {
    pub state: OnAirState,
    pub label: String,
    pub timer: Option<String>,
    /// The tooltip: what a click does.
    pub tip: String,
    /// What a screen reader announces.
    pub name: String,
}

pub fn clock_text(d: std::time::Duration) -> String {
    let s = d.as_secs();
    format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

/// What the button of the `kind` output shows in `state`.
pub fn face(kind: OutputKind, state: Option<&OutputState>) -> Face {
    let (idle, start, stop, stopping, running, named) = match kind {
        OutputKind::Recording => (
            tl!("Record"),
            tid!("record.start", "Start Recording"),
            tid!("record.stop", "Stop Recording"),
            tl!("Finishing Recording…"),
            tl!("REC"),
            tl!("Recording, {time}"),
        ),
        OutputKind::Streaming => (
            tl!("Go Live"),
            tid!("stream.start", "Start Streaming"),
            tid!("stream.stop", "Stop Streaming"),
            tl!("Ending Stream…"),
            tl!("LIVE"),
            tl!("Live, {time}"),
        ),
        OutputKind::Replay => (
            tl!("Replay"),
            tid!("replay.start", "Start Replay Buffer"),
            tid!("replay.stop", "Stop Replay Buffer"),
            tl!("Stopping Replay Buffer…"),
            tl!("REPLAY"),
            tl!("Replay buffer, {time}"),
        ),
    };
    let make = |state, label: &str, timer: Option<String>, tip: &str, name: String| Face { state, label: label.into(), timer, tip: tip.into(), name };
    match state {
        Some(s) if s.stopping => make(OnAirState::Stopping, tl!("Stopping…"), None, stopping, stopping.into()),
        Some(s) if s.status == studiocraft_plugin::OutputStatus::Starting => {
            make(OnAirState::Starting, tl!("Starting…"), None, stop, tl!("Starting…").into())
        }
        Some(s) if s.active => {
            let time = clock_text(s.elapsed);
            let name = craft_i18n::fmt(named, &[("time", &time)]);
            make(OnAirState::Running, running, Some(time), stop, name)
        }
        _ => make(OnAirState::Idle, idle, None, start, idle.into()),
    }
}

/// What the Pause button shows. `paused` holds the countdown's time left in milliseconds while
/// paused (`None` without a countdown), and `for_ms` how long the program has been paused.
pub fn pause_face(paused: Option<Option<u64>>, for_ms: Option<u64>) -> Face {
    let Some(left) = paused else {
        return Face {
            state: OnAirState::Idle,
            label: tl!("Pause").into(),
            timer: None,
            tip: tid!("pause.start", "Pause Program").into(),
            name: tl!("Pause").into(),
        };
    };
    let (time, named) = match left {
        Some(ms) => (studiocraft_studio::countdown_text(ms), tl!("Resume, {time} left")),
        None => (studiocraft_studio::paused_text(for_ms.unwrap_or(0)), tl!("Resume, paused for {time}")),
    };
    let name = craft_i18n::fmt(named, &[("time", &time)]);
    Face { state: OnAirState::Running, label: tl!("Resume").into(), timer: Some(time), tip: tid!("pause.stop", "Resume Program").into(), name }
}

/// The glyph of the Pause button's face: Pause, or Resume while paused.
pub fn pause_glyph(f: &Face) -> Glyph {
    if f.state == OnAirState::Running { Glyph::Resume } else { Glyph::Pause }
}

/// The countdowns the Pause button's menu offers, in minutes.
pub const PAUSE_MINUTES: [u32; 5] = [1, 2, 5, 10, 15];

/// The entries of the Pause button's menu, in English, with the command each runs: a pause for each
/// of [`PAUSE_MINUTES`], a pause without a countdown, and Pause Screen Settings…. While paused, the
/// first six start the countdown again from now.
pub fn pause_menu() -> Vec<(&'static str, CommandRef)> {
    let labels = ["Pause for 1 Minute", "Pause for 2 Minutes", "Pause for 5 Minutes", "Pause for 10 Minutes", "Pause for 15 Minutes"];
    let mut menu: Vec<(&'static str, CommandRef)> =
        labels.into_iter().zip(PAUSE_MINUTES).map(|(l, m)| (l, CommandRef::with_params("pause.start", json!({"countdown_s": m * 60})))).collect();
    menu.push(("Pause Without a Countdown", CommandRef::with_params("pause.start", json!({"countdown_s": 0}))));
    menu.push(("Pause Screen Settings…", CommandRef::new("pause.settings")));
    menu
}

/// The button for `f`.
fn button<'a>(f: &'a Face, glyph: Glyph, style: &'a OnAirStyle) -> OnAirButton<'a> {
    OnAirButton::new(glyph, f.state, &f.label, &f.name, style).timer(f.timer.as_deref())
}

/// Where the parts of the bar were drawn last, for `ui.inspect`.
#[derive(Clone, Debug, Default)]
pub struct Drawn {
    pub rect: Option<Rect>,
    pub fit: Option<Fit>,
    pub parts: Vec<(&'static str, Rect)>,
    /// The estimated and the real width of the transition group when it was last drawn in the bar.
    /// The picker can be wider than its minimum width, so while the estimate stays the same, the
    /// next frame fits the bar with the real width.
    pub transitions_drawn: Option<(f32, f32)>,
}

impl Drawn {
    pub fn json(&self) -> Value {
        let rect = |r: Rect| json!({"x": r.min.x, "y": r.min.y, "width": r.width(), "height": r.height()});
        json!({
            "rect": self.rect.map(rect),
            "transitions": self.fit.map(|f| f.transitions.name()),
            "compact": self.fit.map(|f| f.compact),
            "parts": self.parts.iter().map(|(name, r)| json!({"name": name, "rect": rect(*r)})).collect::<Vec<_>>(),
        })
    }
}

/// The transition picker's width: its longest name, the padding and the arrow.
fn combo_width(ui: &Ui, studio: &Studio) -> f32 {
    let font = TextStyle::Button.resolve(ui.style());
    let longest = studio
        .registry()
        .transitions()
        .iter()
        .map(|p| ui.painter().layout_no_wrap(tl!(p.name()).to_owned(), font.clone(), egui::Color32::WHITE).size().x)
        .fold(0.0, f32::max);
    let sp = ui.spacing();
    (longest + 2.0 * sp.button_padding.x + sp.icon_width + sp.icon_spacing).ceil().clamp(96.0, 200.0)
}

/// The settings form of the current transition, kept between frames: the schema, rebuilt when
/// the transition changes.
fn transition_schema<'s>(studio: &Studio, cached: &'s mut Option<SettingsSchema>) -> &'s SettingsSchema {
    let current = studio.registry().transition(&studio.collection().transition.kind).ok();
    let schema = current.as_ref().map(|p| p.schema()).unwrap_or_default();
    if cached.as_ref() != Some(&schema) {
        *cached = Some(schema);
    }
    cached.get_or_insert_with(SettingsSchema::default)
}

/// The duration field, `width` wide; disabled for a transition that switches at once.
fn duration_field(ui: &mut Ui, studio: &Studio, width: f32, height: f32, acts: &mut Vec<Act>) -> egui::Response {
    let t = &studio.collection().transition;
    let instant = studio.registry().transition(&t.kind).ok().is_some_and(|p| p.instant());
    let mut ms = t.duration_ms;
    let field =
        egui::DragValue::new(&mut ms).range(0..=studiocraft_model::collection::MAX_TRANSITION_MS).speed(10.0).suffix(format!(" {}", tl!("ms")));
    let r = ui.add_enabled_ui(!instant, |ui| ui.add_sized(vec2(width, height), field)).inner.on_hover_text(tl!("Duration"));
    if r.changed() {
        acts.push(Act::run("transition.duration", json!({"ms": ms})));
    }
    if r.drag_stopped() || r.lost_focus() {
        acts.push(Act::Seal);
    }
    r
}

/// The current transition's settings, when it has any.
fn settings_form(ui: &mut Ui, studio: &Studio, schema: &SettingsSchema, acts: &mut Vec<Act>) {
    let mut settings = studio.collection().transition.settings.clone();
    let ev = forms::form(ui, egui::Id::unique("transition-form"), schema, &mut settings);
    if ev.changed {
        acts.push(Act::run("transition.settings", json!({"settings": settings})));
    }
}

/// The transition panel that the overflow button opens: the transitions as a list, the duration
/// and the current transition's settings.
fn transition_panel(ui: &mut Ui, studio: &Studio, schema: &SettingsSchema, field: f32, height: f32, acts: &mut Vec<Act>) {
    ui.set_min_width(200.0);
    ui.strong(tl!("Transition"));
    let kind = &studio.collection().transition.kind;
    for p in studio.registry().transitions() {
        if ui.selectable_label(p.id() == *kind, tl!(p.name())).clicked() && p.id() != *kind {
            acts.push(Act::run("transition.set", json!({"kind": p.id()})));
        }
    }
    ui.separator();
    ui.horizontal(|ui| {
        ui.label(tl!("Duration"));
        duration_field(ui, studio, field, height, acts);
    });
    if !schema.fields.is_empty() {
        ui.separator();
        settings_form(ui, studio, schema, acts);
    }
}

impl StudioApp {
    /// The top row, `room` points wide: the menu titles and the output bar. The menu commands
    /// chosen reach the app through the frame output. `keyboard` false keeps F10 and Alt from the
    /// menu bar.
    pub(crate) fn top_row(&mut self, ui: &mut Ui, room: f32, laid: &Tracked<MenuBar>, keyboard: bool, acts: &mut Vec<Act>) {
        let docks = self.store.prefs().docks;
        let studio = &self.studio;
        let schema = transition_schema(studio, &mut self.transition_schema).clone();

        // Every control on the row is the standard control height.
        let (control, number, seg_style, air_style) = theme::with_root(ui.ctx(), |cx| {
            let t = &cx.tokens;
            (t.size.control.md, t.size.length.number.md, SegmentedStyle::new(cx), OnAirStyle::new(cx, theme::timer_font(cx)))
        });
        let square = vec2(control, control);
        let modes = [tl!("Edit"), tl!("Studio")];
        let studio_mode = usize::from(studio.engine().studio_mode());
        let segmented = Segmented::new(egui::Id::unique("output-bar-mode"), &modes, studio_mode, &seg_style);
        let record = face(OutputKind::Recording, studio.output(OutputKind::Recording).as_ref());
        let live = face(OutputKind::Streaming, studio.output(OutputKind::Streaming).as_ref());
        let pause = pause_face(studio.paused(), studio.paused_for_ms());
        let pause_glyph = pause_glyph(&pause);
        let arrow_name = tl!("Pause options");
        let arrow = MenuArrow::new(pause_glyph, pause.state, arrow_name, &air_style);
        let spacing = ui.spacing().item_spacing.x;
        let combo = combo_width(ui, studio);
        let mut widths = Widths { overflow: square.x, ..Widths::default() };
        if docks.controls {
            widths.mode = segmented.width(ui);
            widths.record = button(&record, Glyph::Record, &air_style).width(ui);
            widths.live = button(&live, Glyph::Live, &air_style).width(ui);
            widths.pause = button(&pause, pause_glyph, &air_style).width(ui) + arrow.width();
            widths.record_compact = button(&record, Glyph::Record, &air_style).compact(true).width(ui);
            widths.live_compact = button(&live, Glyph::Live, &air_style).compact(true).width(ui);
            widths.pause_compact = button(&pause, pause_glyph, &air_style).compact(true).width(ui) + arrow.width();
        }
        let gear = if schema.fields.is_empty() { 0.0 } else { spacing + square.x };
        let estimate = combo + spacing + number + gear;
        if docks.transitions {
            widths.transitions = match self.output_bar.transitions_drawn {
                Some((e, drawn)) if e == estimate => drawn,
                _ => estimate,
            };
        }
        let fit = fit(room, menu_width(ui, laid), &widths);

        let mut parts = Vec::new();
        let mut transitions_drawn = None;
        let height = ui.available_height();
        ui.allocate_ui_with_layout(vec2(room, height), i18n::row(ui), |ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            let top = ui.max_rect().top();
            let start = i18n::start_x(ui, ui.cursor());
            // port: P9 (output bar) lets the framework's menu bar share the row; this child
            // keeps the titles to the width the fit gives them, past which they overflow.
            ui.allocate_ui_with_layout(vec2(fit.menus, height), i18n::row(ui), |ui| {
                // The window has no native menu bar on any platform, so the bar takes F10 and Alt
                // unless a modal has them. Alt with a letter opens a menu or runs a row; a label
                // with no letter a keyboard types takes its English label's letter.
                MenuBarUi::new(laid).keyboard(keyboard).mnemonics(crate::commands::MNEMONICS).english(crate::commands::english_label).show(ui)
            });
            let end = i18n::start_x(ui, ui.cursor());
            parts.push(("menus", Rect::from_min_max(pos2(start.min(end), top), pos2(start.max(end), top + height))));
            ui.add_space((room - (end - start).abs() - fit.outputs).max(0.0));
            let mut first = true;
            let mut gap = |ui: &mut Ui, w: f32| {
                if !first {
                    ui.add_space(w);
                }
                first = false;
            };
            if docks.controls {
                gap(ui, GROUP_GAP);
                let r = segmented.show(ui);
                parts.push(("mode", r.rect));
                if r.changed.is_some() {
                    acts.push(Act::run("studio.toggle", json!({})));
                }
            }
            match fit.transitions {
                Place::Inline => {
                    gap(ui, GROUP_GAP);
                    // A scope keeps the spacing inside the group and none after it.
                    let group = ui.scope(|ui| {
                        ui.spacing_mut().item_spacing.x = spacing;
                        let t = &studio.collection().transition;
                        let current = studio.registry().transition(&t.kind).ok();
                        let name = current.as_ref().map_or(t.kind.as_str(), |p| tl!(p.name()));
                        // port: P9 (output bar) replaces this egui combo box with `Select`. `Select`
                        // sizes itself to its widest entry, while the fit gives this box the width
                        // `combo`, and it shows its placeholder for a transition kind the registry
                        // does not know, which this box shows by its id.
                        #[expect(clippy::disallowed_types, reason = "craft-ui no longer has its combo box; the output bar moves to `Select`")]
                        let r = egui::ComboBox::from_id_salt("transition-kind").selected_text(name).width(combo).show_ui(ui, |ui| {
                            for p in studio.registry().transitions() {
                                if ui.selectable_label(p.id() == t.kind, tl!(p.name())).clicked() && p.id() != t.kind {
                                    acts.push(Act::run("transition.set", json!({"kind": p.id()})));
                                }
                            }
                        });
                        parts.push(("transition", r.response.on_hover_text(tl!("Transition")).rect));
                        parts.push(("duration", duration_field(ui, studio, number, control, acts).rect));
                        if !schema.fields.is_empty() {
                            let r = icons::panel(ui, Icon::Gear, tl!("Transition settings"), square, |ui| {
                                ui.set_min_width(200.0);
                                settings_form(ui, studio, &schema, acts);
                            });
                            parts.push(("transition_settings", r.rect));
                        }
                    });
                    transitions_drawn = Some(group.response.rect.width());
                }
                Place::Overflow => {
                    gap(ui, GROUP_GAP);
                    let r = icons::panel(ui, Icon::Transition, tl!("Transition"), square, |ui| {
                        transition_panel(ui, studio, &schema, number, control, acts);
                    });
                    parts.push(("transition_overflow", r.rect));
                }
                Place::Hidden => {}
            }
            if docks.controls {
                for (f, glyph, id, part, gap_before) in
                    [(&record, Glyph::Record, "record.toggle", "record", OUTPUT_GAP), (&live, Glyph::Live, "stream.toggle", "stream", GROUP_GAP)]
                {
                    gap(ui, gap_before);
                    let r = button(f, glyph, &air_style).compact(fit.compact).show(ui).on_hover_text(&f.tip);
                    parts.push((part, r.rect));
                    if r.clicked() {
                        acts.push(Act::run(id, json!({})));
                    }
                }
                gap(ui, GROUP_GAP);
                let r = button(&pause, pause_glyph, &air_style).joined(true).compact(fit.compact).show(ui).on_hover_text(&pause.tip);
                parts.push(("pause", r.rect));
                if r.clicked() {
                    acts.push(Act::run("pause.toggle", json!({})));
                }
                let r = arrow.show(ui).on_hover_text(arrow_name);
                parts.push(("pause_menu", r.rect));
                // The menu opens under the arrow's reading-start edge.
                let below = if i18n::is_rtl(ui) { egui::RectAlign::BOTTOM_START.flipped_x() } else { egui::RectAlign::BOTTOM_START };
                egui::Popup::menu(&r).align(below).show(|ui| {
                    for (label, cmd) in pause_menu() {
                        if ui.button(i18n::command_label(&cmd.id, label)).clicked() {
                            acts.push(Act::Run(cmd));
                        }
                    }
                });
            }
        });
        self.output_bar.fit = Some(fit);
        self.output_bar.parts = parts;
        if let Some(w) = transitions_drawn {
            self.output_bar.transitions_drawn = Some((estimate, w));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use studiocraft_plugin::OutputStatus;

    use super::*;

    fn widths() -> Widths {
        Widths {
            mode: 100.0,
            transitions: 200.0,
            overflow: 24.0,
            record: 90.0,
            live: 100.0,
            pause: 110.0,
            record_compact: 36.0,
            live_compact: 36.0,
            pause_compact: 60.0,
        }
    }

    #[test]
    fn the_output_bar_adds_its_gaps_and_margin() {
        let w = widths();
        assert_eq!(w.outputs(Place::Inline, false), 100.0 + 8.0 + 200.0 + 12.0 + 90.0 + 8.0 + 100.0 + 8.0 + 110.0 + 8.0);
        assert_eq!(w.outputs(Place::Overflow, false), 100.0 + 8.0 + 24.0 + 12.0 + 90.0 + 8.0 + 100.0 + 8.0 + 110.0 + 8.0);
        assert_eq!(w.outputs(Place::Overflow, true), 100.0 + 8.0 + 24.0 + 12.0 + 36.0 + 8.0 + 36.0 + 8.0 + 60.0 + 8.0);
        assert_eq!(w.outputs(Place::Hidden, false), 100.0 + 12.0 + 90.0 + 8.0 + 100.0 + 8.0 + 110.0 + 8.0);
        let only_transitions = Widths { transitions: 200.0, overflow: 24.0, ..Widths::default() };
        assert_eq!(only_transitions.outputs(Place::Inline, false), 208.0);
        assert_eq!(Widths::default().outputs(Place::Hidden, true), 0.0, "nothing shown takes no room");
    }

    #[test]
    fn the_controls_give_up_room_in_order() {
        let w = widths();
        let menus = 450.0;
        let full = w.outputs(Place::Inline, false);
        let wide = fit(menus + full, menus, &w);
        assert_eq!((wide.transitions, wide.compact, wide.menus), (Place::Inline, false, menus));
        // One point short: the transitions move behind their button first.
        let step1 = fit(menus + full - 1.0, menus, &w);
        assert_eq!((step1.transitions, step1.compact, step1.menus), (Place::Overflow, false, menus));
        // Then Record and Go Live drop their labels.
        let over = w.outputs(Place::Overflow, false);
        let step2 = fit(menus + over - 1.0, menus, &w);
        assert_eq!((step2.transitions, step2.compact, step2.menus), (Place::Overflow, true, menus));
        // Then the menu titles shrink, and the output bar keeps its width.
        let compact = w.outputs(Place::Overflow, true);
        let step3 = fit(menus + compact - 50.0, menus, &w);
        assert_eq!((step3.transitions, step3.compact), (Place::Overflow, true));
        assert_eq!(step3.menus, menus - 50.0);
        assert_eq!(step3.menus + step3.outputs, menus + compact - 50.0, "the row is filled and not exceeded");
    }

    #[test]
    fn hidden_transitions_skip_the_overflow_step() {
        let w = Widths { transitions: 0.0, ..widths() };
        let menus = 450.0;
        let full = w.outputs(Place::Hidden, false);
        assert_eq!(fit(menus + full, menus, &w), Fit { transitions: Place::Hidden, compact: false, menus, outputs: full });
        let short = fit(menus + full - 1.0, menus, &w);
        assert_eq!((short.transitions, short.compact), (Place::Hidden, true));
    }

    #[test]
    fn a_row_with_no_room_or_bad_sizes_gives_the_menus_nothing() {
        let w = widths();
        for room in [0.0, -5.0, f32::NAN] {
            let f = fit(room, 450.0, &w);
            assert_eq!((f.transitions, f.compact, f.menus), (Place::Overflow, true, 0.0), "{room}");
        }
        let f = fit(f32::INFINITY, 450.0, &w);
        assert_eq!(f.menus, 0.0, "an unbounded row is treated as no room");
        assert_eq!(fit(2000.0, f32::NAN, &w).menus, 0.0);
    }

    fn state(active: bool, stopping: bool, status: OutputStatus, secs: u64) -> OutputState {
        OutputState {
            active,
            stopping,
            status,
            target: "x.mkv".into(),
            elapsed: Duration::from_secs(secs),
            bytes: 0,
            destinations: Default::default(),
        }
    }

    #[test]
    fn output_buttons_say_what_they_do() {
        let idle = face(OutputKind::Recording, None);
        assert_eq!((idle.state, idle.label.as_str(), idle.tip.as_str(), idle.timer), (OnAirState::Idle, "Record", "Start Recording", None));
        let live = face(OutputKind::Streaming, Some(&state(true, false, OutputStatus::Running, 3723)));
        assert_eq!(live.state, OnAirState::Running);
        assert_eq!((live.label.as_str(), live.timer.as_deref(), live.tip.as_str()), ("LIVE", Some("01:02:03"), "Stop Streaming"));
        assert_eq!(live.name, "Live, 01:02:03");
        let rec = face(OutputKind::Recording, Some(&state(true, false, OutputStatus::Running, 5)));
        assert_eq!(rec.name, "Recording, 00:00:05");
        let starting = face(OutputKind::Streaming, Some(&state(true, false, OutputStatus::Starting, 0)));
        assert_eq!((starting.state, starting.label.as_str(), starting.tip.as_str()), (OnAirState::Starting, "Starting…", "Stop Streaming"));
        let stopping = face(OutputKind::Recording, Some(&state(true, true, OutputStatus::Running, 9)));
        assert_eq!((stopping.state, stopping.tip.as_str()), (OnAirState::Stopping, "Finishing Recording…"));
        let failed = face(OutputKind::Streaming, Some(&state(false, false, OutputStatus::Failed("refused".into()), 2)));
        assert_eq!((failed.state, failed.label.as_str()), (OnAirState::Idle, "Go Live"));
        let replay = face(OutputKind::Replay, Some(&state(true, false, OutputStatus::Running, 61)));
        assert_eq!((replay.label.as_str(), replay.tip.as_str(), replay.name.as_str()), ("REPLAY", "Stop Replay Buffer", "Replay buffer, 00:01:01"));
        assert_eq!(face(OutputKind::Replay, None).tip, "Start Replay Buffer");
    }

    #[test]
    fn the_pause_button_reads_resume_with_the_time_left_while_paused() {
        let idle = pause_face(None, None);
        assert_eq!((idle.state, idle.label.as_str(), idle.tip.as_str(), idle.timer.as_deref()), (OnAirState::Idle, "Pause", "Pause Program", None));
        assert_eq!(pause_glyph(&idle), Glyph::Pause);
        let counting = pause_face(Some(Some(272_000)), Some(28_000));
        assert_eq!(counting.state, OnAirState::Running, "filled with the warning amber");
        assert_eq!((counting.label.as_str(), counting.timer.as_deref(), counting.tip.as_str()), ("Resume", Some("4:32"), "Resume Program"));
        assert_eq!(counting.name, "Resume, 4:32 left");
        assert_eq!(pause_glyph(&counting), Glyph::Resume);
        let open = pause_face(Some(None), Some(130_400));
        assert_eq!((open.label.as_str(), open.timer.as_deref()), ("Resume", Some("2:10")), "the time paused without a countdown");
        assert_eq!(open.name, "Resume, paused for 2:10");
    }

    #[test]
    fn the_pause_menu_offers_five_countdowns_none_and_the_settings() {
        let menu = pause_menu();
        let seconds: Vec<Option<u64>> = menu.iter().filter(|(_, c)| c.id == "pause.start").map(|(_, c)| c.params["countdown_s"].as_u64()).collect();
        assert_eq!(seconds, [Some(60), Some(120), Some(300), Some(600), Some(900), Some(0)]);
        let labels: Vec<&str> = menu.iter().map(|(l, _)| *l).collect();
        assert_eq!(
            labels,
            [
                "Pause for 1 Minute",
                "Pause for 2 Minutes",
                "Pause for 5 Minutes",
                "Pause for 10 Minutes",
                "Pause for 15 Minutes",
                "Pause Without a Countdown",
                "Pause Screen Settings…"
            ]
        );
        assert_eq!(menu.last().map(|(_, c)| c.clone()), Some(CommandRef::new("pause.settings")));
    }

    #[test]
    fn clocks_count_hours_past_a_day() {
        assert_eq!(clock_text(Duration::from_secs(0)), "00:00:00");
        assert_eq!(clock_text(Duration::from_secs(59 * 60 + 59)), "00:59:59");
        assert_eq!(clock_text(Duration::from_secs(100 * 3600 + 1)), "100:00:01");
    }
}
