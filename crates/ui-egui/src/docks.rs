//! The docks under the preview: Scenes, Sources, Audio Mixer and Application Audio. The transition
//! and output controls are in the output bar ([`crate::output_bar`]). Each dock has a header with
//! its title and its actions ([`crate::kit::panel_header`]) and no footer.

use std::collections::HashMap;

use egui::{Color32, Rect, RichText, Sense, TextStyle, Ui, pos2, vec2};
use serde_json::json;
use studiocraft_engine::lanes::{AppKey, Lane, LaneKind, Lanes};
use studiocraft_media::mix::{BlockLevels, METER_FLOOR_DB, MeterBallistics, from_db, to_db};
use studiocraft_model::{ItemId, SceneId, SourceId};
use studiocraft_studio::Studio;

use crate::icons::{self, Icon};
use crate::kit::panel_header::{PanelHeaderStyle, panel_header};
use crate::theme;
use crate::{Act, Rename};

/// The interface state the docks read and change.
pub struct DockState<'a> {
    pub selected: Option<ItemId>,
    pub renaming: &'a mut Option<(Rename, String)>,
    pub meters: &'a mut HashMap<SourceId, MeterBallistics>,
    /// The Application Audio dock's meters.
    pub lanes: &'a mut LaneCache,
    /// Seconds since the meters were last advanced.
    pub dt: f32,
    /// The place of the dock being drawn among the docks in the strip, and how many are in the
    /// strip; `None` for a dock in a window of its own.
    pub place: Option<(usize, usize)>,
}

/// What a dock's header drew, for `ui.inspect`: its rectangle, the room its title had, the name
/// and rectangle of each action from right to left, and whether the title was shortened to fit.
#[derive(Clone, Debug)]
pub struct Drawn {
    pub header: Rect,
    pub title: Rect,
    pub actions: Vec<(&'static str, Rect)>,
    pub truncated: bool,
    /// How far the title of a dock in a window of its own was dragged this frame; zero in the
    /// strip.
    pub grab: egui::Vec2,
}

/// A dock's header; `title` is English and shown translated. `add` adds the actions from right to
/// left and records each one.
fn header(ui: &mut Ui, title: &str, add: impl FnOnce(&mut Ui, &mut Vec<(&'static str, Rect)>)) -> Drawn {
    let ctx = ui.ctx().clone();
    // The Title style is installed with the theme; before that the button text style stands in.
    let font = ui.style().text_styles.get(&TextStyle::Name(theme::TITLE.into())).cloned().unwrap_or_else(|| TextStyle::Button.resolve(ui.style()));
    let style = PanelHeaderStyle::new(theme::palette(&ctx), &theme::sizes(&ctx), font);
    let r = panel_header(ui, tl!(title), &style, |ui| {
        let mut actions = Vec::new();
        add(ui, &mut actions);
        actions
    });
    Drawn { header: r.rect, title: r.title, actions: r.inner, truncated: r.truncated, grab: egui::Vec2::ZERO }
}

/// The menu a right-click on a dock's title opens, for `dock` (its name in
/// `control::DOCK_NAMES`). A dock in the strip offers Move Left and Move Right (`dock.move`; the
/// first dock cannot move left and the last cannot move right) and Undock; a dock in a window of
/// its own offers Dock in the Strip (both `dock.float`). The title of a dock in a window of its
/// own is also the window's drag handle, and `drawn.grab` gets how far it was dragged.
fn dock_menu(ui: &mut Ui, drawn: &mut Drawn, dock: &'static str, place: Option<(usize, usize)>, acts: &mut Vec<Act>) {
    // In the strip the title takes clicks only, so a drag there still reaches the strip's top edge.
    let sense = if place.is_some() { Sense::click() } else { Sense::click_and_drag() };
    let r = ui.interact(drawn.title, ui.scope_id().with(("dock-title", dock)), sense);
    drawn.grab = r.drag_delta();
    r.context_menu(|ui| {
        let Some((k, n)) = place else {
            if ui.button(tl!("Dock in the Strip")).clicked() {
                acts.push(Act::run("dock.float", json!({"dock": dock, "floating": false})));
                ui.close();
            }
            return;
        };
        if ui.add_enabled(k > 0, egui::Button::new(tl!("Move Left"))).clicked() {
            acts.push(Act::run("dock.move", json!({"dock": dock, "to": "left"})));
            ui.close();
        }
        if ui.add_enabled(k + 1 < n, egui::Button::new(tl!("Move Right"))).clicked() {
            acts.push(Act::run("dock.move", json!({"dock": dock, "to": "right"})));
            ui.close();
        }
        ui.separator();
        if ui.button(tl!("Undock")).clicked() {
            acts.push(Act::run("dock.float", json!({"dock": dock, "floating": true})));
            ui.close();
        }
    });
}

/// A header action named `name` for `ui.inspect`; `tip` is English and shown translated.
fn action(ui: &mut Ui, actions: &mut Vec<(&'static str, Rect)>, name: &'static str, icon: Icon, tip: &str, enabled: bool) -> bool {
    let size = theme::sizes(ui.ctx()).control_sm;
    let r = icons::response(ui, icon, tl!(tip), enabled, size, Color32::TRANSPARENT);
    actions.push((name, r.rect));
    r.clicked()
}

/// The area for the dock's content under its header, inside the dock padding.
fn body(ui: &mut Ui) -> Ui {
    let pad = theme::sizes(ui.ctx()).pad_panel;
    let r = ui.available_rect_before_wrap();
    let rect = Rect::from_min_max(r.min + pad, (r.max - pad).max(r.min + pad));
    ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(craft_ui::direction::column(ui)))
}

/// A text field for renaming; the new name once Enter is pressed or the field loses focus, and
/// `true` in the second place when the edit ended either way.
fn rename_field(ui: &mut Ui, text: &mut String) -> (Option<String>, bool) {
    let r = ui.add(egui::TextEdit::singleline(text).desired_width(f32::INFINITY));
    if !r.has_focus() && !r.lost_focus() {
        r.request_focus();
    }
    if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        return (None, true);
    }
    if r.lost_focus() {
        return (Some(text.trim().to_string()), true);
    }
    (None, false)
}

/// The labels of the four move commands, by their `to` parameter.
pub(crate) const MOVES: [(&str, &str); 4] = [("top", "Move to Top"), ("up", "Move Up"), ("down", "Move Down"), ("bottom", "Move to Bottom")];

pub fn scenes(ui: &mut Ui, studio: &Studio, st: &mut DockState, acts: &mut Vec<Act>) -> Drawn {
    let c = studio.collection();
    let edit = studio.engine().edit_scene();
    let program = c.program_scene;
    let studio_mode = studio.engine().studio_mode();
    let mut drawn = header(ui, "Scenes", |ui, a| {
        let index = c.scene_index(edit).unwrap_or(0);
        if action(ui, a, "down", Icon::Down, "Move the scene down", index + 1 < c.scenes.len()) {
            acts.push(Act::run("scene.move", json!({"scene": edit, "to": "down"})));
        }
        if action(ui, a, "up", Icon::Up, "Move the scene up", index > 0) {
            acts.push(Act::run("scene.move", json!({"scene": edit, "to": "up"})));
        }
        if action(ui, a, "remove", Icon::Minus, "Remove the selected scene", c.scenes.len() > 1) {
            acts.push(Act::run("scene.remove", json!({"scene": edit})));
        }
        if action(ui, a, "add", Icon::Plus, "Add a scene", true) {
            acts.push(Act::run("scene.add", json!({})));
        }
    });
    dock_menu(ui, &mut drawn, "scenes", st.place, acts);
    let area = egui::ScrollArea::vertical().id_salt("scenes").auto_shrink([false, false]);
    area.show(&mut body(ui), |ui| {
        for (i, scene) in c.scenes.iter().enumerate() {
            if let Some((Rename::Scene(id), text)) = st.renaming.as_mut()
                && *id == scene.id
            {
                let (name, done) = rename_field(ui, text);
                if let Some(name) = name.filter(|n| !n.is_empty() && *n != scene.name) {
                    acts.push(Act::run("scene.rename", json!({"scene": scene.id, "name": name})));
                }
                if done {
                    *st.renaming = None;
                }
                continue;
            }
            let mut label = RichText::new(&scene.name);
            if studio_mode && scene.id == program {
                label = label.color(Color32::from_rgb(220, 80, 80));
            }
            let r = ui.add_sized([ui.available_width(), 20.0], egui::Button::selectable(scene.id == edit, label));
            let r = if i < crate::commands::SCENE_HOTKEYS {
                r.on_hover_text(craft_i18n::fmt(tl!("Scene {n}"), &[("n", &(i + 1).to_string())]))
            } else {
                r
            };
            if r.clicked() {
                acts.push(Act::run("scene.select", json!({"scene": scene.id})));
            }
            if r.double_clicked() {
                if studio_mode {
                    acts.push(Act::run("studio.transition", json!({})));
                } else {
                    *st.renaming = Some((Rename::Scene(scene.id), scene.name.clone()));
                }
            }
            r.context_menu(|ui| {
                if ui.button(tl!("Rename")).clicked() {
                    *st.renaming = Some((Rename::Scene(scene.id), scene.name.clone()));
                    ui.close();
                }
                if ui.button(tl!("Duplicate")).clicked() {
                    acts.push(Act::run("scene.duplicate", json!({"scene": scene.id})));
                    ui.close();
                }
                if ui.add_enabled(c.scenes.len() > 1, egui::Button::new(tl!("Remove"))).clicked() {
                    acts.push(Act::run("scene.remove", json!({"scene": scene.id})));
                    ui.close();
                }
                ui.separator();
                for (to, label) in MOVES {
                    if ui.button(tl!(label)).clicked() {
                        acts.push(Act::run("scene.move", json!({"scene": scene.id, "to": to})));
                        ui.close();
                    }
                }
            });
        }
    });
    drawn
}

/// The `+` menu: every source type, and the sources already made.
fn add_source_menu(ui: &mut Ui, studio: &Studio, scene: SceneId, acts: &mut Vec<Act>) {
    for p in studio.registry().sources() {
        if ui.button(tl!(p.name())).clicked() {
            acts.push(Act::AddSource { kind: p.id().to_string() });
            ui.close();
        }
    }
    let c = studio.collection();
    let in_scene: Vec<SourceId> = c.scene(scene).map(|s| s.items.iter().map(|i| i.source).collect()).unwrap_or_default();
    let existing: Vec<_> = c.sources.iter().filter(|s| !in_scene.contains(&s.id) && !c.global_audio.contains(&s.id)).collect();
    if !existing.is_empty() {
        ui.separator();
        ui.menu_button(tl!("Existing Source"), |ui| {
            for s in existing {
                if ui.button(&s.name).clicked() {
                    acts.push(Act::run("source.add_existing", json!({"source": s.id})));
                    ui.close();
                }
            }
        });
    }
}

pub fn sources(ui: &mut Ui, studio: &Studio, st: &mut DockState, acts: &mut Vec<Act>) -> Drawn {
    let c = studio.collection();
    let edit = studio.engine().edit_scene();
    let scene = c.scene(edit);
    let sel = st.selected.and_then(|id| scene.and_then(|s| s.item(id)));
    let mut drawn = header(ui, "Sources", |ui, a| {
        if action(ui, a, "properties", Icon::Gear, "Properties", sel.is_some())
            && let Some(it) = sel
        {
            acts.push(Act::Properties(it.source));
        }
        let index = sel.and_then(|it| scene.and_then(|s| s.item_index(it.id)));
        let top = scene.map_or(0, |s| s.items.len().saturating_sub(1));
        if action(ui, a, "down", Icon::Down, "Move the source down", index.is_some_and(|i| i > 0))
            && let Some(it) = sel
        {
            acts.push(Act::run("item.move", json!({"item": it.id, "to": "down"})));
        }
        if action(ui, a, "up", Icon::Up, "Move the source up", index.is_some_and(|i| i < top))
            && let Some(it) = sel
        {
            acts.push(Act::run("item.move", json!({"item": it.id, "to": "up"})));
        }
        if action(ui, a, "remove", Icon::Minus, "Remove the selected source from the scene", sel.is_some()) {
            acts.push(Act::run("selection.remove", json!({})));
        }
        let size = theme::sizes(ui.ctx()).control_sm;
        let r = icons::menu_sized(ui, Icon::Plus, tl!("Add a source"), size, scene.is_some(), |ui| add_source_menu(ui, studio, edit, acts));
        a.push(("add", r.rect));
    });
    dock_menu(ui, &mut drawn, "sources", st.place, acts);
    let Some(scene) = scene else { return drawn };
    let area = egui::ScrollArea::vertical().id_salt("sources").auto_shrink([false, false]);
    area.show(&mut body(ui), |ui| {
        if scene.items.is_empty() {
            ui.weak(tl!("No sources in this scene. Add one with +."));
        }
        for it in scene.items.iter().rev() {
            let Some(src) = c.source(it.source) else { continue };
            craft_ui::direction::horizontal(ui, |ui| {
                let size = vec2(20.0, 20.0);
                let (eye, tip) = if it.visible { (Icon::Eye, "Hide") } else { (Icon::EyeOff, "Show") };
                if icons::sized(ui, eye, tl!(tip), true, size, Color32::TRANSPARENT) {
                    acts.push(Act::run("item.visible", json!({"item": it.id, "visible": !it.visible})));
                }
                let (lock, tip) = if it.locked { (Icon::Lock, "Unlock") } else { (Icon::Unlock, "Lock (it can't be moved in the preview)") };
                if icons::sized(ui, lock, tl!(tip), true, size, Color32::TRANSPARENT) {
                    acts.push(Act::run("item.lock", json!({"item": it.id, "locked": !it.locked})));
                }
                if let Some((Rename::Source(id), text)) = st.renaming.as_mut()
                    && *id == src.id
                {
                    let (name, done) = rename_field(ui, text);
                    if let Some(name) = name.filter(|n| !n.is_empty() && *n != src.name) {
                        acts.push(Act::run("source.rename", json!({"source": src.id, "name": name})));
                    }
                    if done {
                        *st.renaming = None;
                    }
                    return;
                }
                let mut label = RichText::new(&src.name);
                if studio.source_error(src.id).is_some() {
                    label = label.color(ui.visuals().warn_fg_color);
                }
                let r = ui.add_sized([ui.available_width(), 20.0], egui::Button::selectable(st.selected == Some(it.id), label));
                let r = match studio.source_error(src.id) {
                    Some(e) => r.on_hover_text(crate::i18n::Msg::plain(e).shown()),
                    None => r,
                };
                if r.clicked() {
                    acts.push(Act::Select(Some(it.id)));
                }
                if r.double_clicked() {
                    acts.push(Act::Properties(src.id));
                }
                r.context_menu(|ui| {
                    acts.push(Act::Select(Some(it.id)));
                    item_menu(ui, it.id, src.id, &src.name, st.renaming, acts);
                });
            });
        }
    });
    drawn
}

/// The context menu of a scene item (Sources dock and preview).
pub fn item_menu(ui: &mut Ui, item: ItemId, source: SourceId, name: &str, renaming: &mut Option<(Rename, String)>, acts: &mut Vec<Act>) {
    if ui.button(tl!("Rename")).clicked() {
        *renaming = Some((Rename::Source(source), name.to_string()));
        ui.close();
    }
    if ui.button(tid!("selection.properties", "Properties…")).clicked() {
        acts.push(Act::Properties(source));
        ui.close();
    }
    if ui.button(tid!("selection.filters", "Filters…")).clicked() {
        acts.push(Act::Filters(source));
        ui.close();
    }
    ui.menu_button(tlc!("menu", "Transform"), |ui| {
        for (id, label) in [
            ("selection.reset", "Reset Transform"),
            ("selection.rotate_cw", "Rotate 90° Clockwise"),
            ("selection.rotate_ccw", "Rotate 90° Counterclockwise"),
            ("selection.fit", "Fit to Screen"),
            ("selection.stretch", "Stretch to Screen"),
            ("selection.center", "Center to Screen"),
        ] {
            if ui.button(tid!(id, label)).clicked() {
                acts.push(Act::run(id, json!({})));
                ui.close();
            }
        }
    });
    ui.menu_button(tl!("Order"), |ui| {
        for (to, label) in MOVES {
            if ui.button(tl!(label)).clicked() {
                acts.push(Act::run("item.move", json!({"item": item, "to": to})));
                ui.close();
            }
        }
    });
    ui.separator();
    if ui.button(tl!("Duplicate")).clicked() {
        acts.push(Act::run("item.duplicate", json!({"item": item})));
        ui.close();
    }
    if ui.button(tl!("Remove")).clicked() {
        acts.push(Act::run("item.remove", json!({"item": item})));
        ui.close();
    }
}

/// Meter colors by level: green, then yellow from -20 dB, then red from -9 dB.
pub const METER_YELLOW_DB: f32 = -20.0;
pub const METER_RED_DB: f32 = -9.0;

fn meter_x(db: f32, r: Rect) -> f32 {
    let t = ((db - METER_FLOOR_DB) / -METER_FLOOR_DB).clamp(0.0, 1.0);
    r.left() + t * r.width()
}

/// Two channel bars with their peak marks.
fn meter(ui: &mut Ui, m: &MeterBallistics, muted: bool) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 14.0), Sense::hover());
    let painter = ui.painter_at(rect);
    let zones = [
        (METER_FLOOR_DB, METER_YELLOW_DB, Color32::from_rgb(60, 200, 80)),
        (METER_YELLOW_DB, METER_RED_DB, Color32::from_rgb(230, 200, 40)),
        (METER_RED_DB, 0.0, Color32::from_rgb(230, 60, 50)),
    ];
    for c in 0..2 {
        let h = (rect.height() - 2.0) / 2.0;
        let y = rect.top() + c as f32 * (h + 2.0);
        let bar = Rect::from_min_max(pos2(rect.left(), y), pos2(rect.right(), y + h));
        for (lo, hi, color) in zones {
            let color = if muted { Color32::from_gray(110) } else { color };
            let zone = Rect::from_min_max(pos2(meter_x(lo, bar), bar.top()), pos2(meter_x(hi, bar), bar.bottom()));
            painter.rect_filled(zone, 0.0, color.gamma_multiply(0.22));
            let lit = Rect::from_min_max(zone.min, pos2(meter_x(m.level_db[c], bar).min(zone.right()), zone.bottom()));
            if lit.width() > 0.0 {
                painter.rect_filled(lit, 0.0, color);
            }
        }
        let rms = meter_x(m.rms_db[c], bar);
        painter.line_segment([pos2(rms, bar.top()), pos2(rms, bar.bottom())], (1.0, Color32::from_black_alpha(140)));
        if m.hold_db[c] > METER_FLOOR_DB {
            let x = meter_x(m.hold_db[c], bar);
            painter.line_segment([pos2(x, bar.top()), pos2(x, bar.bottom())], (2.0, Color32::WHITE));
        }
    }
}

fn gain_text(gain: f32) -> String {
    let db = to_db(gain);
    let value = if db <= METER_FLOOR_DB { "-inf".to_string() } else { format!("{db:.1}") };
    craft_i18n::fmt(tl!("{db} dB"), &[("db", &value)])
}

pub fn mixer(ui: &mut Ui, studio: &Studio, st: &mut DockState, acts: &mut Vec<Act>) -> Drawn {
    let mut drawn = header(ui, "Audio Mixer", |_, _| ());
    dock_menu(ui, &mut drawn, "mixer", st.place, acts);
    let c = studio.collection();
    let program: Vec<SourceId> = c.scene(c.program_scene).map(|s| s.items.iter().map(|i| i.source).collect()).unwrap_or_default();
    let audio: Vec<_> = c
        .sources
        .iter()
        .filter(|s| studio.registry().source(&s.kind).is_ok_and(|p| p.caps().audio))
        .filter(|s| c.global_audio.contains(&s.id) || program.contains(&s.id))
        .collect();
    st.meters.retain(|id, _| audio.iter().any(|s| s.id == *id));
    let area = egui::ScrollArea::vertical().id_salt("mixer").auto_shrink([false, false]);
    area.show(&mut body(ui), |ui| {
        if audio.is_empty() {
            ui.weak(tl!("No audio sources are heard in this scene."));
        }
        for s in audio {
            let m = st.meters.entry(s.id).or_default();
            if let Some(l) = studio.levels(s.id) {
                m.update(l, st.dt);
            }
            craft_ui::direction::horizontal(ui, |ui| {
                ui.label(RichText::new(&s.name).strong());
                ui.with_layout(craft_ui::direction::trailing(ui), |ui| {
                    icons::menu(ui, Icon::Gear, tl!("Audio source actions"), |ui| {
                        if ui.button(tid!("selection.properties", "Properties…")).clicked() {
                            acts.push(Act::Properties(s.id));
                            ui.close();
                        }
                        if ui.button(tid!("selection.filters", "Filters…")).clicked() {
                            acts.push(Act::Filters(s.id));
                            ui.close();
                        }
                        if ui.button(tl!("Rename")).clicked() {
                            *st.renaming = Some((Rename::Source(s.id), s.name.clone()));
                            ui.close();
                        }
                    });
                    let (icon, tip, tint) = if s.audio.muted {
                        (Icon::Muted, "Unmute", Color32::from_rgb(230, 60, 50))
                    } else {
                        (Icon::Speaker, "Mute", Color32::TRANSPARENT)
                    };
                    if icons::sized(ui, icon, tl!(tip), true, vec2(26.0, 22.0), tint) {
                        acts.push(Act::run("source.mute", json!({"source": s.id, "muted": !s.audio.muted})));
                    }
                    ui.monospace(gain_text(s.audio.gain));
                });
            });
            if let Some(e) = studio.source_error(s.id) {
                ui.colored_label(ui.visuals().warn_fg_color, crate::i18n::Msg::plain(e).shown());
            }
            meter(ui, m, s.audio.muted);
            let mut db = to_db(s.audio.gain).max(METER_FLOOR_DB);
            let r = ui.add(egui::Slider::new(&mut db, METER_FLOOR_DB..=0.0).show_value(false).trailing_fill(true));
            if r.changed() {
                let gain = if db <= METER_FLOOR_DB { 0.0 } else { from_db(db) };
                acts.push(Act::run("source.volume", json!({"source": s.id, "gain": gain})));
            }
            if r.drag_stopped() {
                acts.push(Act::Seal);
            }
            ui.add_space(6.0);
        }
    });
    drawn
}

/// An Application Audio lane, for its meter: the source that carries it, or before it has one,
/// the application it names.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum LaneId {
    Source(SourceId),
    App(String),
}

/// The meter key of `lane`.
pub fn lane_id(lane: &Lane) -> LaneId {
    match (lane.source, &lane.app) {
        (Some(s), _) => LaneId::Source(s),
        (None, Some(app)) => LaneId::App(app.clone()),
        (None, None) => LaneId::App(lane.name.clone()),
    }
}

/// How often the Application Audio dock reads its lanes again while the collection does not
/// change, in seconds.
pub const LANE_REFRESH: f32 = 0.1;

/// The Application Audio dock's lanes and a meter for each. The dock reads the lanes again when
/// the collection changes and otherwise at most every [`LANE_REFRESH`] seconds, so a frame in
/// between does not rebuild them.
#[derive(Default)]
pub struct LaneCache {
    lanes: Option<Lanes>,
    ids: Vec<LaneId>,
    /// The application of each lane without a source, whose meter reads the system's level.
    keys: Vec<Option<AppKey>>,
    meters: Vec<MeterBallistics>,
    revision: u64,
    read_at: Option<std::time::Instant>,
}

impl LaneCache {
    /// Reads the lanes again when they are due, and otherwise reads each application's newest
    /// level for a lane without a source. A lane keeps its meter across reads.
    pub fn refresh(&mut self, studio: &Studio, now: std::time::Instant) {
        let revision = studio.engine().revision();
        let fresh = self.read_at.is_some_and(|at| now.saturating_duration_since(at).as_secs_f32() < LANE_REFRESH);
        if let Some(lanes) = &mut self.lanes
            && revision == self.revision
            && fresh
        {
            for (lane, key) in lanes.lanes.iter_mut().zip(&self.keys) {
                if let Some(key) = key {
                    lane.peak = studio.engine().app_audio_peak(key);
                }
            }
            return;
        }
        let lanes = studio.engine().app_audio_lanes();
        self.keys = lanes.lanes.iter().map(|l| l.app.as_deref().filter(|_| l.source.is_none()).and_then(AppKey::decode)).collect();
        let ids: Vec<LaneId> = lanes.lanes.iter().map(lane_id).collect();
        let mut old: Vec<(LaneId, MeterBallistics)> = self.ids.drain(..).zip(self.meters.drain(..)).collect();
        self.meters = ids.iter().map(|id| old.iter().position(|(o, _)| o == id).map(|i| old.swap_remove(i).1).unwrap_or_default()).collect();
        self.ids = ids;
        self.lanes = Some(lanes);
        self.revision = revision;
        self.read_at = Some(now);
    }

    /// The lanes last read, if any.
    #[cfg(test)]
    pub fn lanes(&self) -> Option<&Lanes> {
        self.lanes.as_ref()
    }
}

/// What a lane's meter shows: the source's level in the mix for a lane with a source, and the
/// application's level as the system reports it for a lane without one. A lane without a reading
/// shows silence, so its meter falls.
pub fn lane_levels(lane: &Lane, source: Option<BlockLevels>) -> BlockLevels {
    if lane.source.is_some() {
        return source.unwrap_or_default();
    }
    lane.peak.filter(|p| p.is_finite()).map(|p| BlockLevels { peak: [p.clamp(0.0, 1.0); 2], rms: [0.0; 2] }).unwrap_or_default()
}

/// Whether an application lane's check box is checked: a routed source of the lane reaches the
/// program. A lane whose routed source is set to monitor only is unchecked, so checking it routes
/// the application again.
pub fn lane_checked(lane: &Lane) -> bool {
    lane.routed_heard
}

/// The settings-menu item that routes an application lane, for a lane not routed and a routed one.
pub(crate) const ROUTE_ITEMS: [&str; 2] = ["Hear in Every Scene", "Stop Hearing in Every Scene"];
/// The tip of an application lane's check box, for a lane not routed and a routed one.
pub(crate) const ROUTE_TIPS: [&str; 2] =
    ["Hear this application in every scene, with the microphone", "Stop hearing this application in every scene"];

/// The lane's settings menu: routing for an application, and the properties and filters of the
/// lane's source.
fn lane_menu(ui: &mut Ui, lane: &Lane, acts: &mut Vec<Act>) {
    icons::menu(ui, Icon::Gear, tl!("Lane settings"), |ui| {
        if lane.kind == LaneKind::Application
            && let Some(app) = &lane.app
        {
            let label = ROUTE_ITEMS[usize::from(lane_checked(lane))];
            if ui.button(tl!(label)).clicked() {
                acts.push(Act::run("app_audio.route", json!({"app": app, "routed": !lane_checked(lane)})));
                ui.close();
            }
        }
        if let Some(s) = lane.source {
            if lane.kind == LaneKind::Application {
                ui.separator();
            }
            if ui.button(tid!("selection.properties", "Properties…")).clicked() {
                acts.push(Act::Properties(s));
                ui.close();
            }
            if ui.button(tid!("selection.filters", "Filters…")).clicked() {
                acts.push(Act::Filters(s));
                ui.close();
            }
        }
    });
}

/// The Application Audio dock: a lane for each microphone source and each running application
/// with audio, each with a live meter, drawn gray while the program does not hear it. The check
/// box of an application lane routes it into the program in every scene, together with the
/// microphone (`app_audio.route`). A lane with a source has its volume and mute, and its settings
/// menu opens the source's properties and filters.
pub fn app_audio(ui: &mut Ui, studio: &Studio, st: &mut DockState, acts: &mut Vec<Act>) -> Drawn {
    let mut drawn = header(ui, "Application Audio", |_, _| ());
    dock_menu(ui, &mut drawn, "app_audio", st.place, acts);
    st.lanes.refresh(studio, std::time::Instant::now());
    let LaneCache { lanes: Some(lanes), meters, .. } = &mut *st.lanes else {
        return drawn;
    };
    let dt = st.dt;
    let area = egui::ScrollArea::vertical().id_salt("app_audio").auto_shrink([false, false]);
    area.show(&mut body(ui), |ui| {
        let warn = ui.visuals().warn_fg_color;
        match (&lanes.reason, &lanes.error) {
            (Some(reason), _) if !lanes.supported => {
                ui.weak(crate::i18n::Msg::plain(reason.clone()).shown());
            }
            (_, Some(e)) => {
                ui.colored_label(warn, crate::i18n::Msg::plain(e.clone()).shown());
            }
            _ if lanes.listing => {
                ui.weak(tl!("Looking for applications with audio…"));
            }
            _ => {}
        }
        if lanes.lanes.is_empty() {
            ui.weak(tl!("No microphones or applications with audio."));
        }
        for (lane, m) in lanes.lanes.iter().zip(meters.iter_mut()) {
            m.update(lane_levels(lane, lane.source.and_then(|s| studio.levels(s))), dt);
            let gray = !lane.heard;
            craft_ui::direction::horizontal(ui, |ui| {
                if lane.kind == LaneKind::Application
                    && let Some(app) = &lane.app
                {
                    let mut on = lane_checked(lane);
                    let tip = ROUTE_TIPS[usize::from(on)];
                    if ui.checkbox(&mut on, "").on_hover_text(tl!(tip)).changed() {
                        acts.push(Act::run("app_audio.route", json!({"app": app, "routed": on})));
                    }
                }
                let mut name = RichText::new(&lane.name).strong();
                if gray {
                    name = name.color(ui.visuals().weak_text_color());
                }
                let r = ui.label(name);
                if lane.kind == LaneKind::Application && !lane.running {
                    let _ = r.on_hover_text(tl!("This application is not running."));
                }
                ui.with_layout(craft_ui::direction::trailing(ui), |ui| {
                    lane_menu(ui, lane, acts);
                    if let Some(s) = lane.source {
                        let (icon, tip, tint) = if lane.muted {
                            (Icon::Muted, "Unmute", Color32::from_rgb(230, 60, 50))
                        } else {
                            (Icon::Speaker, "Mute", Color32::TRANSPARENT)
                        };
                        if icons::sized(ui, icon, tl!(tip), true, vec2(26.0, 22.0), tint) {
                            acts.push(Act::run("source.mute", json!({"source": s, "muted": !lane.muted})));
                        }
                        ui.monospace(gain_text(lane.gain));
                    }
                });
            });
            if let Some(e) = lane.source.and_then(|s| studio.source_error(s)) {
                ui.colored_label(warn, crate::i18n::Msg::plain(e).shown());
            }
            meter(ui, m, gray || lane.muted);
            let mut db = to_db(lane.gain).max(METER_FLOOR_DB);
            let slider = egui::Slider::new(&mut db, METER_FLOOR_DB..=0.0).show_value(false).trailing_fill(true);
            let r = ui.add_enabled(lane.source.is_some(), slider);
            if let Some(s) = lane.source {
                if r.changed() {
                    let gain = if db <= METER_FLOOR_DB { 0.0 } else { from_db(db) };
                    acts.push(Act::run("source.volume", json!({"source": s, "gain": gain})));
                }
                if r.drag_stopped() {
                    acts.push(Act::Seal);
                }
            }
            ui.add_space(6.0);
        }
    });
    drawn
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meter_positions_cover_the_bar() {
        let r = Rect::from_min_size(pos2(0.0, 0.0), vec2(120.0, 10.0));
        assert_eq!(meter_x(METER_FLOOR_DB, r), 0.0);
        assert_eq!(meter_x(0.0, r), 120.0);
        assert_eq!(meter_x(-30.0, r), 60.0);
        assert_eq!(meter_x(f32::NEG_INFINITY, r), 0.0);
    }

    #[test]
    fn gains_read_in_decibels() {
        assert_eq!(gain_text(1.0), "0.0 dB");
        assert_eq!(gain_text(0.0), "-inf dB");
        assert_eq!(gain_text(0.5), "-6.0 dB");
    }

    fn app_lane(source: Option<SourceId>, peak: Option<f32>) -> Lane {
        Lane {
            kind: LaneKind::Application,
            name: "player.exe".into(),
            app: Some("player.exe|c:\\apps\\player.exe".into()),
            source,
            routed: source.is_some(),
            routed_heard: source.is_some(),
            heard: source.is_some(),
            gain: 1.0,
            muted: false,
            peak,
            running: true,
            playing: peak.is_some(),
            within: None,
        }
    }

    #[test]
    fn a_lane_meters_its_source_and_else_the_applications_level() {
        let mixed = BlockLevels { peak: [0.25, 0.5], rms: [0.1, 0.2] };
        assert_eq!(lane_levels(&app_lane(Some(SourceId(3)), Some(0.9)), Some(mixed)), mixed, "the source's level in the mix");
        assert_eq!(lane_levels(&app_lane(None, Some(0.4)), None).peak, [0.4, 0.4]);
        assert_eq!(lane_levels(&app_lane(None, Some(7.0)), None).peak, [1.0, 1.0], "a reading over full scale is held at it");
        assert_eq!(lane_levels(&app_lane(None, Some(f32::NAN)), None), BlockLevels::default());
        assert_eq!(lane_levels(&app_lane(None, None), None), BlockLevels::default(), "no reading falls to silence");
        assert_eq!(lane_levels(&app_lane(Some(SourceId(3)), Some(0.9)), None), BlockLevels::default(), "a source not in the mix is silent");
    }

    #[test]
    fn a_routed_lane_the_program_does_not_hear_is_unchecked() {
        assert!(lane_checked(&app_lane(Some(SourceId(3)), None)));
        // Routed but set to monitor only, while another source of it is shown in the scene.
        let monitor_only = Lane { routed_heard: false, ..app_lane(Some(SourceId(3)), None) };
        assert!(!lane_checked(&monitor_only), "checking it routes the application again");
        assert!(!lane_checked(&app_lane(None, None)));
    }

    #[test]
    fn a_lane_keeps_its_meter_by_source_once_it_has_one() {
        assert_eq!(lane_id(&app_lane(None, None)), LaneId::App("player.exe|c:\\apps\\player.exe".into()));
        assert_eq!(lane_id(&app_lane(Some(SourceId(3)), None)), LaneId::Source(SourceId(3)));
    }
}
