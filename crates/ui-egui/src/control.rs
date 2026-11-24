//! What the control channel reads from the window and the menu items it runs: every command with
//! its state and shortcut, the window's layout and open dialogs, and the laid-out menu bar.

use craft_keymap::CommandRef;
use craft_menu::{ItemState, LayoutOptions, MenuBar, Node, Target};
use serde_json::{Value, json};
use studiocraft_engine::command::Undo;
use studiocraft_studio::{CaptionState, OutputKind};

use crate::i18n::Msg;
use crate::{APP_NAME, StudioApp, commands};

/// The docks in the order they are laid out, by the names `ui.inspect` reports.
pub(crate) const DOCK_NAMES: [&str; crate::store::DOCK_COUNT] = ["scenes", "sources", "mixer", "app_audio"];
/// The docks' titles in English, in the same order.
pub(crate) const DOCK_TITLES: [&str; crate::store::DOCK_COUNT] = ["Scenes", "Sources", "Audio Mixer", "Application Audio"];

/// Longest menu path accepted, in bytes.
pub const MAX_MENU_PATH: usize = 1024;

/// What separates the labels of a menu path: `Scene Collection > New…`.
const PATH_SEPARATOR: char = '>';

fn rect(r: egui::Rect) -> Value {
    json!({"x": r.min.x, "y": r.min.y, "width": r.width(), "height": r.height()})
}

/// A label as menu paths compare it: three dots match an ellipsis.
fn same_label(label: &str, asked: &str) -> bool {
    label == asked || label.replace('\u{2026}', "...") == asked.replace('\u{2026}', "...")
}

fn menu_nodes(nodes: &[Node], prefix: &str) -> Vec<Value> {
    nodes
        .iter()
        .filter_map(|n| match n {
            Node::Item(it) => {
                let path = format!("{prefix} {PATH_SEPARATOR} {}", it.label);
                let mut v = json!({
                    "label": it.label,
                    "path": path,
                    "id": it.command.id,
                    "enabled": it.enabled,
                    "checked": it.kind.checked(),
                    "shortcut": it.shortcut.map(|c| c.to_string()),
                });
                if !it.command.params.is_null() {
                    v["params"] = it.command.params.clone();
                }
                Some(v)
            }
            Node::Submenu { label, children, .. } => {
                let path = format!("{prefix} {PATH_SEPARATOR} {label}");
                let children = menu_nodes(children, &path);
                Some(json!({"label": label, "path": path, "children": children}))
            }
            Node::Separator | Node::Standard { .. } => None,
        })
        .collect()
}

impl StudioApp {
    /// Whether a menu item for `id` is enabled and checked now.
    pub(crate) fn item_state(&self, id: &str) -> ItemState {
        let p = self.store.prefs();
        let engine = self.studio.engine();
        let on = |b: bool| ItemState { enabled: true, checked: Some(b) };
        match id {
            "edit.undo" => ItemState { enabled: engine.undo_label().is_some(), checked: None },
            "edit.redo" => ItemState { enabled: engine.redo_label().is_some(), checked: None },
            id if id.starts_with("selection.") => ItemState { enabled: self.selection().is_some(), checked: None },
            "view.stats" => on(p.stats),
            "view.scopes" => on(p.scopes),
            "view.streaming" => on(self.streaming.is_some()),
            "view.status_bar" => on(p.status_bar),
            "view.lock_preview" => on(p.lock_preview),
            "view.fullscreen" => on(self.fullscreen),
            "studio.toggle" => on(engine.studio_mode()),
            "replay.toggle" => on(self.studio.output(OutputKind::Replay).is_some_and(|s| s.active)),
            "replay.save" => ItemState { enabled: self.studio.output(OutputKind::Replay).is_some_and(|s| s.active), checked: None },
            "record.marker" => ItemState { enabled: self.studio.can_add_recording_marker(), checked: None },
            "captions.toggle" => on(matches!(self.studio.captions_state(), CaptionState::Loading | CaptionState::Running)),
            "pause.toggle" => on(self.studio.paused().is_some()),
            "pause.start" => ItemState { enabled: self.studio.paused().is_none(), checked: None },
            "pause.stop" => ItemState { enabled: self.studio.paused().is_some(), checked: None },
            "pause.extend" => ItemState { enabled: self.studio.paused().is_some(), checked: None },
            "audio_tracks.open_last" => ItemState { enabled: self.studio.audio_tracks_last_session().is_some(), checked: None },
            "audio_tracks.marker" => ItemState { enabled: self.studio.audio_tracks_running(), checked: None },
            "dock.scenes" => on(p.docks.scenes),
            "dock.sources" => on(p.docks.sources),
            "dock.mixer" => on(p.docks.mixer),
            "dock.app_audio" => on(p.docks.app_audio),
            "dock.transitions" => on(p.docks.transitions),
            "dock.controls" => on(p.docks.controls),
            "dock.layout_apply" | "dock.layout_remove" => ItemState { enabled: !p.dock_layouts.is_empty(), checked: None },
            "profile.remove" => ItemState { enabled: self.profiles.len() > 1, checked: None },
            "collection.remove" => ItemState { enabled: self.collections.len() > 1, checked: None },
            _ => ItemState::default(),
        }
    }

    /// The menu bar as the window shows it: laid out for this platform, with shortcuts and the
    /// current enabled and checked states.
    pub(crate) fn laid_menu(&self) -> MenuBar {
        let layouts = self.store.prefs().dock_layout_names();
        let s = commands::MenuState {
            collections: &self.collections,
            collection: &self.studio.collection().name,
            profiles: &self.profiles,
            profile: &self.studio.profile().name,
            layouts: &layouts,
        };
        let mut bar = commands::menu_bar(&s);
        craft_menu::apply_keymap(&mut bar, &self.keymap);
        craft_menu::apply_state(&mut bar, |c| self.item_state(&c.id));
        let options = LayoutOptions { label: &crate::i18n::menu_text, ..LayoutOptions::new(APP_NAME) };
        craft_menu::layout(&bar, Target::current(), &options).bar
    }

    /// The first shortcut of a command without parameters, as `Cmd+Z` text.
    fn shortcut(&self, id: &str) -> Option<String> {
        self.keymap.bindings(&CommandRef::new(id)).first().map(ToString::to_string)
    }

    /// Every command: the engine's (`scope: engine`) and the window's (`scope: window`), each with
    /// its label, parameters, whether undo can take it back, whether it can run now and its
    /// shortcut. `filter` keeps the commands whose id or label contains it, ignoring case.
    pub fn command_list(&self, filter: Option<&str>) -> Vec<Value> {
        let filter = filter.map(str::to_lowercase);
        let keep = |id: &str, label: &str| filter.as_deref().is_none_or(|f| id.to_lowercase().contains(f) || label.to_lowercase().contains(f));
        let entry = |id: &str, label: &str, params: &str, undo: bool, scope: &str| {
            json!({
                "id": id,
                "label": label,
                "params": params,
                "undo": undo,
                "scope": scope,
                "enabled": self.item_state(id).enabled,
                "shortcut": self.shortcut(id),
            })
        };
        let engine = studiocraft_engine::specs()
            .iter()
            .filter(|s| keep(s.id, s.label))
            .map(|s| entry(s.id, s.label, s.params, s.undo == Undo::Undoable, "engine"));
        let window = commands::UI_COMMANDS
            .iter()
            .filter(|(id, label)| keep(id, label))
            .map(|(id, label)| entry(id, label, commands::ui_params(id), commands::ui_undoable(id), "window"));
        engine.chain(window).collect()
    }

    /// The laid-out menu bar as a tree: each menu and submenu with its `label`, `path` and
    /// `children`, each item with its `label`, `path`, command `id`, `params` (when it has any),
    /// `enabled`, `checked` (`null` for a plain item) and `shortcut`.
    pub fn menu_tree(&self) -> Value {
        let bar = self.laid_menu();
        Value::Array(bar.menus.iter().map(|m| json!({"label": m.title, "path": m.title, "children": menu_nodes(&m.children, &m.title)})).collect())
    }

    /// The command of the menu item at `path` (`Scene Collection > New…`). An unknown path, a
    /// submenu and a disabled item are errors.
    pub fn menu_command(&self, path: &str) -> Result<CommandRef, String> {
        if path.len() > MAX_MENU_PATH {
            return Err(format!("a menu path may be at most {MAX_MENU_PATH} bytes long"));
        }
        let labels: Vec<&str> = path.split(PATH_SEPARATOR).map(str::trim).collect();
        let (Some(first), Some(rest)) = (labels.first(), labels.get(1..)) else {
            return Err("the menu path is empty".into());
        };
        if labels.iter().any(|l| l.is_empty()) || rest.is_empty() {
            return Err(format!("`{path}` is not a menu path; write it as `Menu {PATH_SEPARATOR} Item`"));
        }
        let bar = self.laid_menu();
        let menu = bar.menus.iter().find(|m| same_label(&m.title, first)).ok_or_else(|| format!("there is no menu `{first}`"))?;
        let mut nodes = &menu.children;
        for (k, label) in rest.iter().enumerate() {
            let last = k + 1 == rest.len();
            // A label written exactly as the menu shows it wins over one that matches only with
            // three dots for an ellipsis, so names such as `A…` and `A...` both stay reachable.
            let matches = |exact: bool| {
                move |n: &&Node| {
                    let shown = match n {
                        Node::Item(it) => it.label.as_str(),
                        Node::Submenu { label: l, .. } => l.as_str(),
                        Node::Separator | Node::Standard { .. } => return false,
                    };
                    if exact { shown == *label } else { same_label(shown, label) }
                }
            };
            let found = nodes.iter().find(matches(true)).or_else(|| nodes.iter().find(matches(false)));
            match found {
                Some(Node::Item(it)) if last => {
                    if !it.enabled {
                        return Err(format!("the menu item `{path}` is disabled"));
                    }
                    return Ok(it.command.clone());
                }
                Some(Node::Submenu { children, .. }) if !last => nodes = children,
                Some(Node::Submenu { .. }) => return Err(format!("`{path}` is a submenu, not a menu item")),
                _ => return Err(format!("there is no menu item `{path}`")),
            }
        }
        Err(format!("there is no menu item `{path}`"))
    }

    /// Run the menu item at `path` as a click on it does.
    pub fn invoke_menu(&mut self, ctx: &egui::Context, path: &str) -> Result<Value, String> {
        let cmd = self.menu_command(path)?;
        let result = self.perform(ctx, &cmd)?;
        Ok(json!({"command": cmd.id, "result": result}))
    }

    /// Whether the window is asking to confirm closing while a recording, stream or replay buffer
    /// runs.
    pub fn confirming_exit(&self) -> bool {
        self.confirm_exit
    }

    /// Answer the open name prompt or exit confirmation as its buttons do. `ok` presses OK in the
    /// name prompt and Stop and Exit in the exit confirmation; otherwise Cancel is pressed. `text`
    /// replaces the name in the prompt first; the exit confirmation takes no text. A name the
    /// prompt refuses leaves the prompt open with the error.
    pub fn answer_dialog(&mut self, ctx: &egui::Context, ok: bool, text: Option<&str>) -> Result<Value, String> {
        if let Some(p) = &mut self.prompt {
            if let Some(t) = text {
                p.text = t.to_string();
            }
            let (what, name) = (p.what, p.text.trim().to_string());
            if !ok {
                self.prompt = None;
                return Ok(json!({"dialog": "prompt", "closed": true}));
            }
            return match self.named(what, &name) {
                Ok(()) => {
                    self.prompt = None;
                    Ok(json!({"dialog": "prompt", "closed": true, "name": name}))
                }
                Err(e) => {
                    let english = e.english();
                    if let Some(p) = &mut self.prompt {
                        p.error = Some(e);
                    }
                    Err(english)
                }
            };
        }
        if self.confirm_exit {
            if text.is_some() {
                return Err("the exit confirmation takes no `text`".into());
            }
            self.confirm_exit = false;
            if ok {
                self.exit_confirmed = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            return Ok(json!({"dialog": "confirm_exit", "closed": true, "exiting": ok}));
        }
        Err("no dialog is waiting for an answer; ui.inspect lists the open dialogs".into())
    }

    /// What the window shows: its size in points, the open dialogs and windows, where the docks,
    /// the output bar and the editable view were drawn last, the menu titles, the selected item
    /// and the notices.
    pub fn ui_inspect(&self, ctx: &egui::Context) -> Value {
        let window = ctx.viewport_rect();
        let dock = |(i, r, d): &(usize, egui::Rect, crate::docks::Drawn)| {
            json!({
                "dock": DOCK_NAMES.get(*i).copied().unwrap_or("unknown"),
                "rect": rect(*r),
                "header": rect(d.header),
                "title": rect(d.title),
                "title_truncated": d.truncated,
                "actions": d.actions.iter().rev().map(|(name, r)| json!({"name": name, "rect": rect(*r)})).collect::<Vec<_>>(),
            })
        };
        let docks: Vec<Value> = self.dock_rects.iter().map(dock).collect();
        let floating: Vec<Value> = self.floating_rects.iter().map(dock).collect();
        let d = self.store.prefs().docks;
        let strip = self.dock_strip.map(|r| {
            json!({
                "rect": rect(r),
                "height": r.height(),
                "saved_height": crate::strip::usable(d.height),
                "saved_widths": {
                    "scenes": crate::strip::usable(d.scenes_width),
                    "sources": crate::strip::usable(d.sources_width),
                    "mixer": crate::strip::usable(d.mixer_width),
                    "app_audio": crate::strip::usable(d.app_audio_width),
                },
                "order": d.order().iter().filter_map(|i| DOCK_NAMES.get(usize::from(*i)).copied()).collect::<Vec<_>>(),
            })
        });
        let preview = self.last_view.map(|(r, scale)| json!({"rect": rect(r), "scale": scale}));
        let menus: Vec<String> = self.laid_menu().menus.into_iter().map(|m| m.title).collect();
        json!({
            "window": {"width": window.width(), "height": window.height()},
            "dialogs": {
                "settings": self.settings.is_some(),
                "properties": self.properties.iter().map(|p| p.source).collect::<Vec<_>>(),
                "filters": self.filters.iter().map(|f| f.source).collect::<Vec<_>>(),
                "about": self.about,
                "stats": self.store.prefs().stats,
                "encoders": self.encoders.is_some(),
                "prompt": self.prompt.as_ref().map(|p| json!({"title": p.what.title(), "text": p.text, "error": p.error.as_ref().map(Msg::english)})),
                "confirm_exit": self.confirm_exit,
                "updates": self.updates.window_open || self.updates.updater.prompt_open(),
            },
            "docks": docks,
            "floating_docks": floating,
            "dock_strip": strip,
            "dock_layouts": self.store.prefs().dock_layout_names(),
            "output_bar": self.output_bar.json(),
            "preview": preview,
            "studio_mode": self.studio.engine().studio_mode(),
            "menus": menus,
            "selected_item": self.selected,
            "notices": self.notices.iter().map(|(n, _)| json!({"text": n.msg.english(), "error": n.error})).collect::<Vec<_>>(),
            "frames": self.frames,
        })
    }
}
