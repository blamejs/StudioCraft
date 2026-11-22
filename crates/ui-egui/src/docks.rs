//! The docks under the preview: Scenes, Sources, Audio Mixer and Application Audio. The transition
//! and output controls are in the output bar ([`crate::output_bar`]). Each dock has a header with
//! its title and its actions and no footer. The Scenes and Sources docks show a framework title
//! row and list; the Audio Mixer and Application Audio docks use [`crate::kit::panel_header`].

use std::borrow::Cow;
use std::collections::HashMap;

use craft_behavior::{ColumnKey, ColumnWidth, EditEvent, IconId, Selection, SelectionChange, keyed};
use craft_keymap::CommandRef;
use craft_theme::variant::Tone;
use craft_ui::contract::overlay::Anchor;
use craft_ui::contract::{
    Bind, CollectionEvent, ColumnSpec, Props as _, SelectMode, Show as _, ShowCollection as _, ShowContainer as _, ToggleColumn, Trigger,
    request_rename,
};
use craft_ui::display::Divider;
use craft_ui::list::{List, ListRow, ToggleCell};
use craft_ui::menu::{Menu, MenuItem};
use craft_ui::primitives::IconSpec;
use craft_ui::runtime::env::Label;
use craft_ui::sections::{HeaderCommand, HeaderStrength, TitleRow};
use egui::{Color32, Rect, RichText, Sense, Ui, pos2, vec2};
use serde_json::json;
use studiocraft_engine::lanes::{AppKey, Lane, LaneKind, Lanes};
use studiocraft_media::mix::{BlockLevels, METER_FLOOR_DB, MeterBallistics, from_db, to_db};
use studiocraft_model::{ItemId, SceneId, SceneItem, SourceConfig, SourceId};
use studiocraft_studio::Studio;

use crate::commands::{SCENE_HEADER, SCENE_HOTKEYS, SOURCE_HEADER};
use crate::icons::{self, Icon};
use crate::kit::panel_header::{PanelHeaderStyle, panel_header};
use crate::kit::tokens::{MeterColors, paint};
use crate::theme;
use crate::{Act, Rename};

/// The menus of the Scenes and Sources docks, kept from frame to frame.
#[derive(Debug, Default)]
pub struct DockLists {
    /// Whether the Sources dock's Add menu is open (`sources.add`).
    pub add_open: bool,
    /// The scene whose row a right-click asked a menu for.
    scene_menu: Option<SceneId>,
    /// The scene item whose row a right-click asked a menu for.
    item_menu: Option<ItemId>,
}

/// The interface state the docks read and change.
pub struct DockState<'a> {
    pub selected: Option<ItemId>,
    pub renaming: &'a mut Option<(Rename, String)>,
    pub lists: &'a mut DockLists,
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
}

/// A dock's header; `title` is English and shown translated. `add` adds the actions from right to
/// left and records each one.
fn header(ui: &mut Ui, title: &str, add: impl FnOnce(&mut Ui, &mut Vec<(&'static str, Rect)>)) -> Drawn {
    let style = theme::with_root(ui.ctx(), |cx| PanelHeaderStyle::new(cx, theme::title_font(cx)));
    let r = panel_header(ui, tl!(title), &style, |ui| {
        let mut actions = Vec::new();
        add(ui, &mut actions);
        actions
    });
    Drawn { header: r.rect, title: r.title, actions: r.inner, truncated: r.truncated }
}

/// The menu a right-click on a dock's title opens, for `dock` (its name in
/// `control::DOCK_NAMES`). A dock in the strip offers Move Left and Move Right (`dock.move`; the
/// first dock cannot move left and the last cannot move right) and Undock; a dock in a window of
/// its own offers Dock in the Strip (both `dock.float`).
fn dock_menu(ui: &mut Ui, drawn: &Drawn, dock: &'static str, place: Option<(usize, usize)>, acts: &mut Vec<Act>) {
    // The title takes clicks only, so a drag there still reaches the strip's top edge.
    let r = ui.interact(drawn.title, ui.scope_id().with(("dock-title", dock)), Sense::click());
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

/// A dock's header as a framework title row: `title` (English, shown translated) and, at the end
/// edge, one button per entry of `buttons` (name in `ui.inspect`, icon, command, tooltip), in
/// reading order. A button runs its command, and the window's command states give its tooltip and
/// whether it is enabled. Also returns the title row's response.
fn title_header(ui: &mut Ui, title: &str, buttons: &[(&'static str, &'static str, &str, &str)]) -> (Drawn, egui::Response) {
    let (height, pad, side, fill, font) = theme::with_root(ui.ctx(), |cx| {
        let t = &cx.tokens;
        (t.size.panel_header, t.space.pad.regular, t.size.control.md, theme::color(cx, t.color.surface.raised), theme::title_font(cx))
    });
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width().max(0.0), height), Sense::hover());
    if ui.is_rect_visible(rect) {
        ui.painter().rect_filled(rect, 0.0, fill);
    }
    let left = (rect.left() + pad).min(rect.right());
    let room = Rect::from_min_max(pos2(left, rect.top()), pos2((rect.right() - pad).max(left), rect.bottom()));
    let mut row_ui = ui.new_child(egui::UiBuilder::new().id_salt(("dock-header", title)).max_rect(room));
    let commands = buttons.iter().map(|(_, icon, id, _)| HeaderCommand::new(IconId::new(icon), CommandRef::new(*id)));
    let out = TitleRow::new(tl!(title).to_string()).strength(HeaderStrength::Strong).commands(commands).show(&mut row_ui);
    let row = out.edit.response.rect;
    // The title row lays its buttons out from the end edge, each as wide as a standard control
    // and as tall as the row, with no gap between them.
    let side = side.min(row.height());
    let rtl = crate::i18n::is_rtl(ui);
    let actions: Vec<(&'static str, Rect)> = buttons
        .iter()
        .rev()
        .enumerate()
        .map(|(k, (name, ..))| {
            let off = side * k as f32;
            let x = if rtl { row.left() + off } else { row.right() - off - side };
            (*name, Rect::from_min_size(pos2(x, row.top()), vec2(side, row.height())))
        })
        .collect();
    let band = side * buttons.len() as f32;
    let title_rect = if rtl {
        Rect::from_min_max(pos2((row.left() + band).min(row.right()), row.top()), row.right_bottom())
    } else {
        Rect::from_min_max(row.left_top(), pos2((row.right() - band).max(row.left()), row.bottom()))
    };
    let wanted = ui.painter().layout_no_wrap(tl!(title).to_string(), font, Color32::WHITE).size().x;
    let drawn = Drawn { header: rect, title: title_rect, actions, truncated: wanted > title_rect.width() + 0.5 };
    (drawn, out.edit.response)
}

/// The area for the dock's content under its header, inside the dock padding.
fn body(ui: &mut Ui) -> Ui {
    let pad = egui::Vec2::splat(theme::with_root(ui.ctx(), |cx| cx.tokens.space.pad.regular));
    let r = ui.available_rect_before_wrap();
    let rect = Rect::from_min_max(r.min + pad, (r.max - pad).max(r.min + pad));
    ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(crate::i18n::column(ui)))
}

/// The body of a dock that shows a list, clipped to its own room so that the list takes no
/// press outside it (a floating dock's window edges).
fn list_body(ui: &mut Ui) -> Ui {
    let mut body = body(ui);
    body.set_clip_rect(body.max_rect().intersect(ui.clip_rect()));
    body
}

/// The labels of the four move commands, by their `to` parameter.
pub(crate) const MOVES: [(&str, &str); 4] = [("top", "Move to Top"), ("up", "Move Up"), ("down", "Move Down"), ("bottom", "Move to Bottom")];

pub fn scenes(ui: &mut Ui, studio: &Studio, st: &mut DockState, acts: &mut Vec<Act>) -> Drawn {
    let c = studio.collection();
    let edit = studio.engine().edit_scene();
    let program = c.program_scene;
    let studio_mode = studio.engine().studio_mode();
    let (drawn, _) = title_header(ui, "Scenes", &SCENE_HEADER);
    dock_menu(ui, &drawn, "scenes", st.place, acts);
    let mut body = list_body(ui);
    let source = keyed(&c.scenes, |s| s.id).with_text(|s| Cow::Borrowed(s.name.as_str()));
    let mut selection = Selection::single(edit);
    let shown = List::<SceneId>::new(tl!("Scenes").to_string())
        .select(SelectMode::Single)
        .selection(Bind::bound(&mut selection))
        .allow_empty(false)
        .rename(true)
        .show_items(&mut body, &source, &mut |_, icx| {
            let scene = &c.scenes[icx.at];
            let mut row = ListRow::new(scene.name.clone()).state(icx.row.clone());
            if studio_mode && scene.id == program {
                row = row.trailing_icon(IconSpec::svg(IconId::new("circle")), tl!("Program").to_string()).trailing_tone(Tone::Bad);
            }
            if icx.at < SCENE_HOTKEYS {
                row = row.tooltip(craft_i18n::fmt(tl!("Scene {n}"), &[("n", &(icx.at + 1).to_string())]));
            }
            row
        });
    let list = shown.response.id;
    // The scene being edited once the acts so far have run.
    let mut picked = edit;
    for event in shown.events {
        match event {
            CollectionEvent::Selection(SelectionChange::Set(keys)) => {
                if let Some(id) = keys.last().copied().filter(|id| *id != picked) {
                    acts.push(Act::run("scene.select", json!({"scene": id})));
                    picked = id;
                }
            }
            // A double click or Enter: in studio mode it sends the scene to the program, and
            // otherwise it renames the scene.
            CollectionEvent::Activate(id) => {
                if studio_mode {
                    if id != picked {
                        acts.push(Act::run("scene.select", json!({"scene": id})));
                        picked = id;
                    }
                    acts.push(Act::run("studio.transition", json!({})));
                } else {
                    request_rename(body.ctx(), list, &id);
                }
            }
            CollectionEvent::Rename(edit) => {
                if let Some((id, name)) = renamed(&edit)
                    && let Some(scene) = c.scene(id)
                    && name != scene.name
                {
                    acts.push(Act::run("scene.rename", json!({"scene": id, "name": name})));
                }
            }
            CollectionEvent::ContextMenu(request) => st.lists.scene_menu = Some(request.key),
            CollectionEvent::EmptyContextMenu { .. } => st.lists.scene_menu = None,
            _ => {}
        }
    }
    if let Some(scene) = st.lists.scene_menu.and_then(|id| c.scene(id)) {
        Menu::context(scene.name.clone(), &shown.response).show(&mut body, |ui, _| {
            if row(ui, tl!("Rename"), true) {
                request_rename(ui.ctx(), list, &scene.id);
            }
            if row(ui, tl!("Duplicate"), true) {
                acts.push(Act::run("scene.duplicate", json!({"scene": scene.id})));
            }
            if row(ui, tl!("Remove"), c.scenes.len() > 1) {
                acts.push(Act::run("scene.remove", json!({"scene": scene.id})));
            }
            Divider::horizontal().show(ui);
            for (to, label) in MOVES {
                if row(ui, tl!(label), true) {
                    acts.push(Act::run("scene.move", json!({"scene": scene.id, "to": to})));
                }
            }
        });
    }
    drawn
}

/// A menu row labeled `label` (shown as is) that can be `enabled`; whether it ran. A row that
/// runs closes the menu.
fn row(ui: &mut Ui, label: &str, enabled: bool) -> bool {
    MenuItem::new(label.to_string()).enabled(enabled).show(ui).ran
}

/// The item and new name of a finished rename, the name trimmed and not empty.
fn renamed<K: Clone>(edit: &EditEvent<craft_ui::contract::Rename<K>>) -> Option<(K, String)> {
    let (EditEvent::Commit { after, .. } | EditEvent::Set { after, .. }) = edit else { return None };
    let name = after.name.trim();
    (!name.is_empty()).then(|| (after.key.clone(), name.to_string()))
}

/// The rows of the Add menu: every source type, and the sources already made that the scene does
/// not hold, in a submenu.
fn add_source_menu(ui: &mut Ui, studio: &Studio, scene: SceneId, acts: &mut Vec<Act>) {
    for p in studio.registry().sources() {
        if row(ui, tl!(p.name()), true) {
            acts.push(Act::AddSource { kind: p.id().to_string() });
        }
    }
    let c = studio.collection();
    let in_scene: Vec<SourceId> = c.scene(scene).map(|s| s.items.iter().map(|i| i.source).collect()).unwrap_or_default();
    let existing: Vec<_> = c.sources.iter().filter(|s| !in_scene.contains(&s.id) && !c.global_audio.contains(&s.id)).collect();
    if !existing.is_empty() {
        Divider::horizontal().show(ui);
        Menu::submenu_row(ui, tl!("Existing Source").to_string(), |ui, _| {
            for s in &existing {
                if row(ui, &s.name, true) {
                    acts.push(Act::run("source.add_existing", json!({"source": s.id})));
                }
            }
        });
    }
}

/// The eye, lock and name columns of the Sources dock.
const EYE: ColumnKey = ColumnKey::new("visible");
const LOCK: ColumnKey = ColumnKey::new("locked");
const NAME: ColumnKey = ColumnKey::new("name");

pub fn sources(ui: &mut Ui, studio: &Studio, st: &mut DockState, acts: &mut Vec<Act>) -> Drawn {
    let c = studio.collection();
    let edit = studio.engine().edit_scene();
    let scene = c.scene(edit);
    let (drawn, header) = title_header(ui, "Sources", &SOURCE_HEADER);
    // The Add menu hangs under its button. The header row is its opener, so a press on the button
    // while the menu is open reaches the button, which closes it.
    if scene.is_none() {
        st.lists.add_open = false;
    }
    let mut menu = Menu::new(tl!("Add a source").to_string()).opened_by(&header, Trigger::Manual).open(Bind::bound(&mut st.lists.add_open));
    if let Some((_, at)) = drawn.actions.iter().find(|(name, _)| *name == "add") {
        menu = menu.anchor(Anchor::Rect(*at));
    }
    menu.show(ui, |ui, _| add_source_menu(ui, studio, edit, acts));
    dock_menu(ui, &drawn, "sources", st.place, acts);
    let Some(scene) = scene else { return drawn };
    // The top item is drawn first.
    let rows: Vec<(&SceneItem, &SourceConfig)> = scene.items.iter().rev().filter_map(|it| Some((it, c.source(it.source)?))).collect();
    let source = keyed(&rows, |(it, _)| it.id)
        .with_text(|(_, src)| Cow::Borrowed(src.name.as_str()))
        .with_toggled(|(it, _), column| (*column == EYE && it.visible) || (*column == LOCK && it.locked));
    let columns = vec![
        ColumnSpec::toggle(EYE, Label::Empty, ToggleColumn::sweepable()),
        ColumnSpec::toggle(LOCK, Label::Empty, ToggleColumn::sweepable()),
        ColumnSpec::new(NAME, Label::Empty, ColumnWidth::fill(1.0)),
    ];
    let mut selection = Selection::new(st.selected.into_iter().collect(), st.selected, st.selected);
    let mut body = list_body(ui);
    let shown = List::<ItemId>::new(tl!("Sources").to_string())
        .select(SelectMode::Single)
        .selection(Bind::bound(&mut selection))
        .header(false)
        .columns(columns)
        .rename(true)
        .empty(tl!("No sources in this scene. Add one with +.").to_string())
        .show_items(&mut body, &source, &mut |_, icx| {
            let (it, src) = rows[icx.at];
            let eye = if it.visible { "Hide" } else { "Show" };
            let lock = if it.locked { "Unlock" } else { "Lock (it can't be moved in the preview)" };
            let mut row = ListRow::new(src.name.clone())
                .cells(icx.cells())
                .state(icx.row.clone())
                .toggle(ToggleCell::eye(EYE, it.visible).tooltip(tl!(eye).to_string()))
                .toggle(ToggleCell::lock(LOCK, it.locked).tooltip(tl!(lock).to_string()));
            if let Some(e) = studio.source_error(src.id) {
                let error = crate::i18n::Msg::plain(e).shown();
                row = row.trailing_icon(IconSpec::svg(IconId::new("warning")), error.clone()).trailing_tone(Tone::Warn).tooltip(error);
            }
            row
        });
    let list = shown.response.id;
    let item = |id: ItemId| rows.iter().find(|(it, _)| it.id == id).copied();
    // A sweep and its commit can report the same cell in one frame: each cell changes once.
    let mut flipped: Vec<(ItemId, ColumnKey)> = Vec::new();
    for event in shown.events {
        match event {
            CollectionEvent::Selection(SelectionChange::Set(keys)) => acts.push(Act::Select(keys.last().copied())),
            CollectionEvent::Selection(SelectionChange::Clear) => acts.push(Act::Select(None)),
            CollectionEvent::Activate(id) => {
                if let Some((_, src)) = item(id) {
                    acts.push(Act::Properties(src.id));
                }
            }
            CollectionEvent::Toggle(edit) => {
                let Some(cell) = edit.landed() else { continue };
                let Some((it, _)) = item(cell.key) else { continue };
                if flipped.contains(&(cell.key, cell.column.clone())) {
                    continue;
                }
                if cell.column == EYE && cell.on != it.visible {
                    acts.push(Act::run("item.visible", json!({"item": it.id, "visible": cell.on})));
                } else if cell.column == LOCK && cell.on != it.locked {
                    acts.push(Act::run("item.lock", json!({"item": it.id, "locked": cell.on})));
                } else {
                    continue;
                }
                flipped.push((cell.key, cell.column.clone()));
            }
            CollectionEvent::Rename(edit) => {
                if let Some((id, name)) = renamed(&edit)
                    && let Some((_, src)) = item(id)
                    && name != src.name
                {
                    acts.push(Act::run("source.rename", json!({"source": src.id, "name": name})));
                }
            }
            CollectionEvent::ContextMenu(request) => {
                acts.push(Act::Select(Some(request.key)));
                st.lists.item_menu = Some(request.key);
            }
            CollectionEvent::EmptyContextMenu { .. } => st.lists.item_menu = None,
            _ => {}
        }
    }
    if let Some((it, src)) = st.lists.item_menu.and_then(item) {
        Menu::context(src.name.clone(), &shown.response).show(&mut body, |ui, _| item_menu(ui, it.id, src.id, &src.name, st.renaming, acts));
    }
    // A rename asked for by the item's menu, here or in the preview, opens the item's field.
    if let Some((Rename::Source(source), _)) = st.renaming
        && let Some((it, _)) = rows.iter().find(|(_, src)| src.id == *source)
    {
        request_rename(body.ctx(), list, &it.id);
        *st.renaming = None;
    }
    drawn
}

/// The rows of a scene item's context menu (Sources dock and preview), inside a menu body. A row
/// that runs closes the menu.
pub fn item_menu(ui: &mut Ui, item: ItemId, source: SourceId, name: &str, renaming: &mut Option<(Rename, String)>, acts: &mut Vec<Act>) {
    let row = |ui: &mut Ui, label: &str| MenuItem::new(label.to_string()).show(ui).ran;
    if row(ui, tl!("Rename")) {
        *renaming = Some((Rename::Source(source), name.to_string()));
    }
    if row(ui, tid!("selection.properties", "Properties…")) {
        acts.push(Act::Properties(source));
    }
    if row(ui, tid!("selection.filters", "Filters…")) {
        acts.push(Act::Filters(source));
    }
    Menu::submenu_row(ui, tlc!("menu", "Transform").to_string(), |ui, _| {
        for (id, label) in [
            ("selection.reset", "Reset Transform"),
            ("selection.rotate_cw", "Rotate 90° Clockwise"),
            ("selection.rotate_ccw", "Rotate 90° Counterclockwise"),
            ("selection.fit", "Fit to Screen"),
            ("selection.stretch", "Stretch to Screen"),
            ("selection.center", "Center to Screen"),
        ] {
            if row(ui, tid!(id, label)) {
                acts.push(Act::run(id, json!({})));
            }
        }
    });
    Menu::submenu_row(ui, tl!("Order").to_string(), |ui, _| {
        for (to, label) in MOVES {
            if row(ui, tl!(label)) {
                acts.push(Act::run("item.move", json!({"item": item, "to": to})));
            }
        }
    });
    Divider::horizontal().show(ui);
    if row(ui, tl!("Duplicate")) {
        acts.push(Act::run("item.duplicate", json!({"item": item})));
    }
    if row(ui, tl!("Remove")) {
        acts.push(Act::run("item.remove", json!({"item": item})));
    }
}

/// Meter colors by level: green, then yellow from -20 dB, then red from -9 dB.
pub const METER_YELLOW_DB: f32 = -20.0;
pub const METER_RED_DB: f32 = -9.0;

fn meter_x(db: f32, r: Rect) -> f32 {
    let t = ((db - METER_FLOOR_DB) / -METER_FLOOR_DB).clamp(0.0, 1.0);
    r.left() + t * r.width()
}

/// Two channel bars on the meter's track, with their peak marks, in the colors of the window's
/// light or dark mode.
fn meter(ui: &mut Ui, m: &MeterBallistics, muted: bool) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 14.0), Sense::hover());
    let painter = ui.painter_at(rect);
    let colors = MeterColors::of(craft_ui::runtime::env::Env::of(ui).view().mode());
    painter.rect_filled(rect, 0.0, paint(colors.track));
    let zones = [
        (METER_FLOOR_DB, METER_YELLOW_DB, paint(colors.green)),
        (METER_YELLOW_DB, METER_RED_DB, paint(colors.yellow)),
        (METER_RED_DB, 0.0, paint(colors.red)),
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
            painter.line_segment([pos2(x, bar.top()), pos2(x, bar.bottom())], (2.0, paint(colors.hold)));
        }
    }
}

fn gain_text(gain: f32) -> String {
    let db = to_db(gain);
    let value = if db <= METER_FLOOR_DB { "-inf".to_string() } else { format!("{db:.1}") };
    craft_i18n::fmt(tl!("{db} dB"), &[("db", &value)])
}

pub fn mixer(ui: &mut Ui, studio: &Studio, st: &mut DockState, acts: &mut Vec<Act>) -> Drawn {
    let drawn = header(ui, "Audio Mixer", |_, _| ());
    dock_menu(ui, &drawn, "mixer", st.place, acts);
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
            crate::i18n::horizontal(ui, |ui| {
                ui.label(RichText::new(&s.name).strong());
                ui.with_layout(crate::i18n::trailing(ui), |ui| {
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

/// Whether the dock warns that the program can hear a routed application twice: once through its
/// own capture and once through a desktop audio source that is not muted, when both record the
/// same output device. The routed application is running and its source is not muted.
pub fn desktop_doubles(lanes: &Lanes) -> bool {
    lanes.desktop_heard && lanes.lanes.iter().any(|l| l.kind == LaneKind::Application && l.routed_heard && !l.muted && l.running)
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
    let drawn = header(ui, "Application Audio", |_, _| ());
    dock_menu(ui, &drawn, "app_audio", st.place, acts);
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
        if desktop_doubles(lanes) {
            ui.colored_label(
                warn,
                tl!("Desktop audio also records a routed application that plays on the same device, so the program can hear it twice."),
            );
        }
        for (lane, m) in lanes.lanes.iter().zip(meters.iter_mut()) {
            m.update(lane_levels(lane, lane.source.and_then(|s| studio.levels(s))), dt);
            let gray = !lane.heard;
            crate::i18n::horizontal(ui, |ui| {
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
                ui.with_layout(crate::i18n::trailing(ui), |ui| {
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
    fn the_dock_warns_while_desktop_audio_also_plays_a_routed_application() {
        let mut lanes =
            Lanes { supported: true, reason: None, listing: false, error: None, lanes: vec![app_lane(Some(SourceId(3)), None)], desktop_heard: true };
        assert!(desktop_doubles(&lanes));
        lanes.lanes[0].routed_heard = false;
        assert!(!desktop_doubles(&lanes), "no routed application reaches the program");
        lanes.lanes[0].routed_heard = true;
        lanes.lanes[0].muted = true;
        assert!(!desktop_doubles(&lanes), "the routed source is muted");
        lanes.lanes[0].muted = false;
        lanes.lanes[0].running = false;
        assert!(!desktop_doubles(&lanes), "the routed application quit");
        lanes.lanes[0].running = true;
        lanes.desktop_heard = false;
        assert!(!desktop_doubles(&lanes), "desktop audio muted or not heard");
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
