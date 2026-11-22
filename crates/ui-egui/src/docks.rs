//! The docks under the preview: Scenes, Sources, Audio Mixer and Application Audio. The transition
//! and output controls are in the output bar ([`crate::output_bar`]). Each dock has a header with
//! its title and its actions and no footer. Every dock's header is a framework title row; the
//! Scenes and Sources docks show a framework list under it, and the Audio Mixer and Application
//! Audio docks a column of channels with framework text, buttons and menus around app meters.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::Debug;
use std::hash::Hash;

use craft_behavior::{ColumnKey, ColumnWidth, EditEvent, IconId, Selection, SelectionChange, keyed};
use craft_keymap::CommandRef;
use craft_theme::variant::{Emphasis, Tone};
use craft_theme::{CustomVariant, Tokens, Variant, VariantLook};
use craft_ui::buttons::IconButton;
use craft_ui::buttons::icon_button::IconButtonStyleKey;
use craft_ui::choice::Checkbox;
use craft_ui::contract::overlay::Anchor;
use craft_ui::contract::{
    Bind, CollectionEvent, ColumnSpec, PopupKind, Props as _, SelectMode, Show as _, ShowCollection as _, ShowContainer as _, ToggleColumn, Trigger,
    request_rename,
};
use craft_ui::display::{Divider, Elide, Shade, Text};
use craft_ui::list::{List, ListRow, ToggleCell};
use craft_ui::menu::{Menu, MenuItem};
use craft_ui::primitives::IconSpec;
use craft_ui::runtime::env::Label;
use craft_ui::sections::{HeaderCommand, HeaderStrength, TitleRow};
use craft_ui::style::TypeRole;
use egui::{Color32, Rect, Sense, Ui, pos2, vec2};
use serde_json::json;
use studiocraft_engine::lanes::{AppKey, Lane, LaneKind, Lanes};
use studiocraft_media::mix::{BlockLevels, METER_FLOOR_DB, MeterBallistics, from_db, to_db};
use studiocraft_model::collection::{MAX_GAIN, MAX_GAIN_DB};
use studiocraft_model::{ItemId, SceneId, SceneItem, SourceConfig, SourceId};
use studiocraft_studio::Studio;

use crate::commands::{SCENE_HEADER, SCENE_HOTKEYS, SOURCE_HEADER};
use crate::icons::Speaker;
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
    /// Where the dock being drawn is docked; `None` for a dock in a window of its own.
    pub place: Option<Place>,
}

/// Where a docked dock is drawn.
#[derive(Clone, Copy, Debug)]
pub struct Place {
    /// The dock's place among the docks drawn in its region, and how many are drawn there.
    pub k: usize,
    pub n: usize,
    /// Whether the region is a column, where the docks are stacked from top to bottom.
    pub column: bool,
    /// The region the dock sits in, which is not the strip it is drawn in while the columns are
    /// collapsed into the strip.
    pub region: crate::store::Region,
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

/// The menu a right-click on a dock's title opens, for `dock` (its name in
/// `control::DOCK_NAMES`). A docked dock offers Move Left and Move Right in the strip, or Move Up
/// and Move Down in a column (`dock.move`; the first dock in its region cannot move back and the
/// last cannot move on), a move to each other region (`dock.region`) and Undock; a dock in a window
/// of its own offers Dock in the Strip (both `dock.float`).
fn dock_menu(ui: &mut Ui, drawn: &Drawn, dock: &'static str, place: Option<Place>, acts: &mut Vec<Act>) {
    use crate::store::Region;
    // The title takes clicks only, so a drag there still reaches the strip's top edge.
    let r = ui.interact(drawn.title, ui.scope_id().with(("dock-title", dock)), Sense::click());
    r.context_menu(|ui| {
        let Some(Place { k, n, column, region }) = place else {
            if ui.button(tl!("Dock in the Strip")).clicked() {
                acts.push(Act::run("dock.float", json!({"dock": dock, "floating": false})));
                ui.close();
            }
            return;
        };
        let (back, on) = if column { (tl!("Move Up"), tl!("Move Down")) } else { (tl!("Move Left"), tl!("Move Right")) };
        let (back_to, on_to) = if column { ("up", "down") } else { ("left", "right") };
        if ui.add_enabled(k > 0, egui::Button::new(back)).clicked() {
            acts.push(Act::run("dock.move", json!({"dock": dock, "to": back_to})));
            ui.close();
        }
        if ui.add_enabled(k + 1 < n, egui::Button::new(on)).clicked() {
            acts.push(Act::run("dock.move", json!({"dock": dock, "to": on_to})));
            ui.close();
        }
        ui.separator();
        for (to, label) in [
            (Region::Left, tl!("Move to the Left Column")),
            (Region::Right, tl!("Move to the Right Column")),
            (Region::Bottom, tl!("Move Under the Canvas")),
        ] {
            if ui.add_enabled(region != to, egui::Button::new(label)).clicked() {
                acts.push(Act::run("dock.region", json!({"dock": dock, "region": to.name()})));
                ui.close();
            }
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
            ("selection.fill", "Fill Screen"),
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
            let color = if muted { paint(colors.muted) } else { color };
            let zone = Rect::from_min_max(pos2(meter_x(lo, bar), bar.top()), pos2(meter_x(hi, bar), bar.bottom()));
            painter.rect_filled(zone, 0.0, color.gamma_multiply(0.22));
            let lit = Rect::from_min_max(zone.min, pos2(meter_x(m.level_db[c], bar).min(zone.right()), zone.bottom()));
            if lit.width() > 0.0 {
                painter.rect_filled(lit, 0.0, color);
            }
        }
        let rms = meter_x(m.rms_db[c], bar);
        painter.line_segment([pos2(rms, bar.top()), pos2(rms, bar.bottom())], (1.0, paint(colors.rms)));
        if m.hold_db[c] > METER_FLOOR_DB {
            let x = meter_x(m.hold_db[c], bar);
            painter.line_segment([pos2(x, bar.top()), pos2(x, bar.bottom())], (2.0, paint(colors.hold)));
        }
    }
}

fn gain_text(gain: f32) -> String {
    let db = to_db(gain);
    let value = if db <= METER_FLOOR_DB {
        "-inf".to_string()
    } else if db >= 0.05 {
        format!("+{db:.1}")
    } else {
        format!("{db:.1}")
    };
    craft_i18n::fmt(tl!("{db} dB"), &[("db", &value)])
}

/// A line of a dock's body, wrapped at the dock's width: muted, or in `tone`'s color.
fn note(ui: &mut Ui, text: &str, tone: Option<Tone>) {
    let line = Text::new(text.to_string());
    match tone {
        Some(tone) => line.tone(tone).show(ui),
        None => line.shade(Shade::Muted).show(ui),
    };
}

/// A channel's or a lane's name, cut with an ellipsis when it does not fit, muted when `faint`.
fn channel_name(ui: &mut Ui, name: &str, faint: bool) -> egui::Response {
    let text = Text::new(name.to_string()).role(TypeRole::BodyStrong).elide(Elide::End);
    if faint { text.shade(Shade::Muted).show(ui) } else { text.show(ui) }
}

/// A channel's gain readout in the monospace role.
fn gain_readout(ui: &mut Ui, gain: f32) {
    Text::new(gain_text(gain)).role(TypeRole::Mono).show(ui);
}

/// The settings button named `label` of the channel or lane `key`, and the menu of `rows` it
/// opens under it. A row that runs closes the menu.
fn settings_menu<K: Hash + Debug + Clone>(ui: &mut Ui, key: K, label: &str, mut rows: impl FnMut(&mut Ui)) {
    let button = IconButton::new(IconSpec::svg(IconId::new("settings")))
        .label(label.to_string())
        .tooltip(label.to_string())
        .popup(PopupKind::Menu)
        .id_salt(("settings", key.clone()))
        .show(ui);
    Menu::new(label.to_string()).opened_by(&button.response, Trigger::Click).id_salt(("settings-menu", key)).show(ui, |ui, _| rows(ui));
}

/// The look of a muted channel's mute button: the warning tone's solid fill, with the glyph in the
/// text color the theme sets on it.
fn muted_look(tokens: &Tokens) -> VariantLook {
    VariantLook::of(Emphasis::Solid, Tone::Warn, tokens)
}

/// The icon button variant of a muted channel's or lane's mute button, in [`muted_look`].
const MUTED: CustomVariant = CustomVariant::new::<IconButtonStyleKey>("studiocraft.muted", muted_look);

/// The mute button of the channel or lane `key`: a speaker while it is heard, and the muted
/// speaker on a warning fill while it is `muted`. Its name and tooltip say what a click does.
/// Whether it was clicked.
fn mute_button(ui: &mut Ui, key: impl Hash + Debug, muted: bool) -> bool {
    let (icon, label) = if muted { (IconSpec::svg(IconId::new("track_mute")), "Unmute") } else { (IconSpec::glyph(&Speaker), "Mute") };
    let mut button = IconButton::new(icon).label(tl!(label).to_string()).tooltip(tl!(label).to_string()).id_salt(("mute", key));
    if muted {
        button = button.variant(Variant::Custom(MUTED));
    }
    button.show(ui).clicked()
}

/// The line of the channel or lane `key`: `lead` (its name, or its check box) from the reading
/// start, in the room the row's trailing controls leave, and `trail` (its settings, mute and gain)
/// from the reading end. The room comes from the width the trailing controls took in the previous
/// frame, or from `estimate` in a row's first frame, so a long name is cut or wrapped instead of
/// running under them.
fn channel_row<S>(
    ui: &mut Ui,
    key: impl Hash + Debug,
    estimate: f32,
    state: &mut S,
    lead: impl FnOnce(&mut Ui, &mut S),
    trail: impl FnOnce(&mut Ui, &mut S),
) {
    let id = ui.scope_id().with(("channel-trail", key));
    crate::i18n::horizontal(ui, |ui| {
        let reserve = ui.data(|d| d.get_temp::<TrailWidth>(id)).map_or(estimate, |w| w.0);
        let room = (ui.available_width() - reserve - ui.spacing().item_spacing.x).max(0.0);
        let size = egui::vec2(room, ui.available_height());
        let layout = crate::i18n::row(ui);
        ui.allocate_ui_with_layout(size, layout, |ui| lead(ui, state));
        let layout = crate::i18n::trailing(ui);
        let width = ui.with_layout(layout, |ui| trail(ui, state)).response.rect.width();
        ui.data_mut(|d| d.insert_temp(id, TrailWidth(width)));
    });
}

/// The width a row's trailing controls took in the last frame, kept in egui's memory.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TrailWidth(f32);

/// The width a row's trailing controls take before a frame has measured them: a square control
/// for each of its `buttons` and the text of its gain readout `gain`, with the gaps between them.
fn trail_estimate(ui: &Ui, buttons: usize, gain: Option<f32>) -> f32 {
    let gap = ui.spacing().item_spacing.x;
    let readout = gain.map_or(0.0, |g| {
        let text = egui::WidgetText::from(gain_text(g));
        text.into_galley(ui, Some(egui::TextWrapMode::Extend), f32::INFINITY, egui::TextStyle::Monospace).size().x + gap
    });
    buttons as f32 * (ui.spacing().interact_size.y + gap) + readout
}

/// The space between two channels or lanes.
fn stack_gap(ui: &Ui) -> f32 {
    theme::with_root(ui.ctx(), |cx| cx.tokens.space.gap.stack)
}

/// The gain for a volume slider at `db`: the meter floor is silence and the top is [`MAX_GAIN`].
fn fader_gain(db: f32) -> f32 {
    if db <= METER_FLOOR_DB { 0.0 } else { from_db(db).min(MAX_GAIN) }
}

/// Where 0 dB sits on a volume slider drawn in `r`. The slider keeps its round handle inside `r`,
/// so the ends of its rail are inset by the handle's radius, `r.height() / 2.5`.
fn fader_zero_x(r: Rect) -> f32 {
    let inset = (r.height() / 2.5).min(r.width() / 2.0);
    let t = -METER_FLOOR_DB / (MAX_GAIN_DB - METER_FLOOR_DB);
    r.left() + inset + t * (r.width() - 2.0 * inset)
}

/// A volume slider from the meter floor to [`MAX_GAIN_DB`] of boost, with a mark under its rail
/// at 0 dB. Returns the slider's response and the gain it was moved to.
fn volume_slider(ui: &mut Ui, gain: f32, enabled: bool) -> (egui::Response, f32) {
    let mark = ui.painter().add(egui::Shape::Noop);
    let mut db = to_db(gain).clamp(METER_FLOOR_DB, MAX_GAIN_DB);
    let slider = egui::Slider::new(&mut db, METER_FLOOR_DB..=MAX_GAIN_DB).show_value(false).trailing_fill(true);
    let r = ui.add_enabled(enabled, slider);
    let x = fader_zero_x(r.rect);
    let line = egui::Shape::line_segment([pos2(x, r.rect.top()), pos2(x, r.rect.bottom())], (1.0, ui.visuals().weak_text_color()));
    ui.painter().set(mark, line);
    (r, fader_gain(db))
}

pub fn mixer(ui: &mut Ui, studio: &Studio, st: &mut DockState, acts: &mut Vec<Act>) -> Drawn {
    let (drawn, _) = title_header(ui, "Audio Mixer", &[]);
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
    let gap = stack_gap(ui);
    let area = egui::ScrollArea::vertical().id_salt("mixer").auto_shrink([false, false]);
    area.show(&mut body(ui), |ui| {
        if audio.is_empty() {
            note(ui, tl!("No audio sources are heard in this scene."), None);
        }
        for (k, s) in audio.into_iter().enumerate() {
            if k > 0 {
                ui.add_space(gap);
            }
            let m = st.meters.entry(s.id).or_default();
            if let Some(l) = studio.levels(s.id) {
                m.update(l, st.dt);
            }
            let lead = |ui: &mut Ui, _: &mut Vec<Act>| {
                channel_name(ui, &s.name, false);
            };
            let estimate = trail_estimate(ui, 2, Some(s.audio.gain));
            channel_row(ui, ("mixer", s.id), estimate, acts, lead, |ui, acts| {
                settings_menu(ui, ("mixer", s.id), tl!("Audio source actions"), |ui| {
                    if row(ui, tid!("selection.properties", "Properties…"), true) {
                        acts.push(Act::Properties(s.id));
                    }
                    if row(ui, tid!("selection.filters", "Filters…"), true) {
                        acts.push(Act::Filters(s.id));
                    }
                    if row(ui, tl!("Rename"), true) {
                        acts.push(Act::Rename(s.id));
                    }
                });
                if mute_button(ui, ("mixer", s.id), s.audio.muted) {
                    acts.push(Act::run("source.mute", json!({"source": s.id, "muted": !s.audio.muted})));
                }
                gain_readout(ui, s.audio.gain);
            });
            if let Some(e) = studio.source_error(s.id) {
                note(ui, &crate::i18n::Msg::plain(e).shown(), Some(Tone::Bad));
            }
            meter(ui, m, s.audio.muted);
            let (r, gain) = volume_slider(ui, s.audio.gain, true);
            if r.changed() {
                acts.push(Act::run("source.volume", json!({"source": s.id, "gain": gain})));
            }
            if r.drag_stopped() {
                acts.push(Act::Seal);
            }
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
    settings_menu(ui, ("lane", lane_id(lane)), tl!("Lane settings"), |ui| {
        if lane.kind == LaneKind::Application
            && let Some(app) = &lane.app
        {
            let label = ROUTE_ITEMS[usize::from(lane_checked(lane))];
            if row(ui, tl!(label), true) {
                acts.push(Act::run("app_audio.route", json!({"app": app, "routed": !lane_checked(lane)})));
            }
        }
        if let Some(s) = lane.source {
            if lane.kind == LaneKind::Application {
                Divider::horizontal().show(ui);
            }
            if row(ui, tid!("selection.properties", "Properties…"), true) {
                acts.push(Act::Properties(s));
            }
            if row(ui, tid!("selection.filters", "Filters…"), true) {
                acts.push(Act::Filters(s));
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
    let (drawn, _) = title_header(ui, "Application Audio", &[]);
    dock_menu(ui, &drawn, "app_audio", st.place, acts);
    st.lanes.refresh(studio, std::time::Instant::now());
    let LaneCache { lanes: Some(lanes), meters, .. } = &mut *st.lanes else {
        return drawn;
    };
    let dt = st.dt;
    let gap = stack_gap(ui);
    let area = egui::ScrollArea::vertical().id_salt("app_audio").auto_shrink([false, false]);
    area.show(&mut body(ui), |ui| {
        match (&lanes.reason, &lanes.error) {
            (Some(reason), _) if !lanes.supported => note(ui, &crate::i18n::Msg::plain(reason.clone()).shown(), None),
            (_, Some(e)) => note(ui, &crate::i18n::Msg::plain(e.clone()).shown(), Some(Tone::Bad)),
            _ if lanes.listing => note(ui, tl!("Looking for applications with audio…"), None),
            _ => {}
        }
        if lanes.lanes.is_empty() {
            note(ui, tl!("No microphones or applications with audio."), None);
        }
        if desktop_doubles(lanes) {
            let warning = tl!("Desktop audio also records a routed application that plays on the same device, so the program can hear it twice.");
            note(ui, warning, Some(Tone::Warn));
        }
        for (k, (lane, m)) in lanes.lanes.iter().zip(meters.iter_mut()).enumerate() {
            if k > 0 {
                ui.add_space(gap);
            }
            m.update(lane_levels(lane, lane.source.and_then(|s| studio.levels(s))), dt);
            let gray = !lane.heard;
            let key = lane_id(lane);
            // An application lane's check box carries the lane's name; its tooltip says what a click
            // does, or that the application is not running.
            let lead = |ui: &mut Ui, acts: &mut Vec<Act>| {
                if lane.kind == LaneKind::Application
                    && let Some(app) = &lane.app
                {
                    let was = lane_checked(lane);
                    let mut on = was;
                    let tip = if lane.running { tl!(ROUTE_TIPS[usize::from(was)]) } else { tl!("This application is not running.") };
                    Checkbox::new(Bind::bound(&mut on)).label(lane.name.clone()).tooltip(tip.to_string()).id_salt(("route", key.clone())).show(ui);
                    if on != was {
                        acts.push(Act::run("app_audio.route", json!({"app": app, "routed": on})));
                    }
                } else {
                    channel_name(ui, &lane.name, gray);
                }
            };
            let estimate = if lane.source.is_some() { trail_estimate(ui, 2, Some(lane.gain)) } else { trail_estimate(ui, 1, None) };
            channel_row(ui, ("lane", key.clone()), estimate, acts, lead, |ui, acts| {
                lane_menu(ui, lane, acts);
                if let Some(s) = lane.source {
                    if mute_button(ui, ("lane", key.clone()), lane.muted) {
                        acts.push(Act::run("source.mute", json!({"source": s, "muted": !lane.muted})));
                    }
                    gain_readout(ui, lane.gain);
                }
            });
            if let Some(e) = lane.source.and_then(|s| studio.source_error(s)) {
                note(ui, &crate::i18n::Msg::plain(e).shown(), Some(Tone::Bad));
            }
            meter(ui, m, gray || lane.muted);
            let (r, gain) = volume_slider(ui, lane.gain, lane.source.is_some());
            if let Some(s) = lane.source {
                if r.changed() {
                    acts.push(Act::run("source.volume", json!({"source": s, "gain": gain})));
                }
                if r.drag_stopped() {
                    acts.push(Act::Seal);
                }
            }
        }
    });
    drawn
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every filled rectangle a frame draws, with its fill.
    fn fills(out: &egui::FullOutput) -> Vec<Color32> {
        fn walk(shape: &egui::Shape, found: &mut Vec<Color32>) {
            match shape {
                egui::Shape::Rect(r) => found.push(r.fill),
                egui::Shape::Vec(v) => v.iter().for_each(|s| walk(s, found)),
                _ => {}
            }
        }
        let mut found = Vec::new();
        out.shapes.iter().for_each(|c| walk(&c.shape, &mut found));
        found
    }

    #[test]
    fn a_muted_channel_shows_its_mute_button_on_the_warning_fill() {
        for muted in [false, true] {
            let ctx = crate::test_context();
            let mut warn = None;
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
                warn = Some(theme::with_root(ui.ctx(), |cx| theme::color(cx, cx.tokens.color.status.warn.solid)));
                mute_button(ui, "mic", muted);
            });
            out.textures_delta.clear();
            let Some(warn) = warn else { panic!("the frame did not run") };
            let filled = fills(&out).contains(&warn);
            assert_eq!(filled, muted, "muted {muted}: the button sits on the warning fill only while the channel is muted");
        }
    }

    #[test]
    fn the_muted_look_puts_the_status_text_color_on_the_warning_fill() {
        let ctx = crate::test_context();
        let mut seen = None;
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            seen = Some(theme::with_root(ui.ctx(), |cx| (muted_look(cx.tokens), cx.tokens.color.status.warn.solid, cx.tokens.color.text.on_status)));
        });
        out.textures_delta.clear();
        let Some((look, warn, on_status)) = seen else { panic!("the frame did not run") };
        assert_eq!(look.fill.rest, warn);
        assert_eq!(look.text.rest, on_status);
        let ratio = craft_theme::contrast_ratio(look.text.rest, look.fill.rest);
        assert!(ratio >= 3.0, "the muted glyph is {ratio:.2}:1 on the warning fill");
    }

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
        assert_eq!(gain_text(MAX_GAIN), "+20.0 dB");
    }

    #[test]
    fn volume_sliders_reach_the_top_boost_with_0_db_marked() {
        assert_eq!(fader_gain(METER_FLOOR_DB), 0.0);
        assert!((fader_gain(0.0) - 1.0).abs() < 1e-6);
        assert!((fader_gain(MAX_GAIN_DB) - MAX_GAIN).abs() < 1e-5);
        assert!(fader_gain(MAX_GAIN_DB) <= MAX_GAIN, "the top never asks for more than the engine accepts");
        // A 200 by 20 slider insets its rail by 8 at each end: -60..=+20 dB over 184 points puts
        // 0 dB three quarters of the way along.
        let r = Rect::from_min_size(pos2(10.0, 0.0), vec2(200.0, 20.0));
        assert!((fader_zero_x(r) - (10.0 + 8.0 + 0.75 * 184.0)).abs() < 1e-4);
        let narrow = Rect::from_min_size(pos2(0.0, 0.0), vec2(4.0, 20.0));
        assert!((narrow.left()..=narrow.right()).contains(&fader_zero_x(narrow)), "a slider narrower than its handle keeps the mark inside");
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
