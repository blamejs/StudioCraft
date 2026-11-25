//! What the control channel reads from the window and the menu items it runs: every command with
//! its state and shortcut, the window's layout and open dialogs, and the laid-out menu bar.

use craft_keymap::CommandRef;
use craft_menu::{ItemState, LanguageChoice, LanguageLabels, LanguageMenu, LayoutOptions, MenuBar, MenuRole, Node, Target};
use craft_ui::contract::Tracked;
use craft_ui::dialog::{HeldOk, StandardPrompt};
use craft_ui::patterns::form_dialogs::HostedParamDialog;
use serde_json::{Value, json};
use studiocraft_engine::command::Undo;
use studiocraft_model::staged;
use studiocraft_studio::{CaptionState, OutputKind};

use crate::i18n::Msg;
use crate::{APP_NAME, NAME_FIELD, Naming, StudioApp, commands};

/// The docks in the order they are laid out, by the names `ui.inspect` reports.
pub(crate) const DOCK_NAMES: [&str; crate::store::DOCK_COUNT] = ["scenes", "sources", "mixer", "app_audio", "health"];
/// The docks' titles in English, in the same order.
pub(crate) const DOCK_TITLES: [&str; crate::store::DOCK_COUNT] = ["Scenes", "Sources", "Audio Mixer", "Application Audio", "Health"];

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
            _ => None,
        })
        .collect()
}

impl StudioApp {
    /// Whether a Staged windows source of the collection holds a window or display in slot `slot`
    /// (1 is the first), or in any slot when `slot` is `None`.
    pub(crate) fn staged_slot(&self, slot: Option<usize>) -> bool {
        let mut sources = self.studio.collection().sources.iter().filter(|s| s.kind == staged::KIND);
        sources.any(|s| match slot {
            Some(n) => staged::staged(&s.settings, n).is_some(),
            None => (1..=staged::SLOTS).any(|n| staged::staged(&s.settings, n).is_some()),
        })
    }

    /// Whether a menu item for `id` is enabled and checked now.
    pub(crate) fn item_state(&self, id: &str) -> ItemState {
        let p = self.store.prefs();
        let engine = self.studio.engine();
        let on = |b: bool| ItemState::default().checked(Some(b));
        let enabled = |b: bool| ItemState::default().enabled(b);
        match id {
            "edit.undo" => enabled(engine.undo_label().is_some()),
            "edit.redo" => enabled(engine.redo_label().is_some()),
            id if id.starts_with("selection.") => enabled(self.selection().is_some()),
            "scenes.remove" => enabled(self.edit_scene_place().is_some_and(|(_, n)| n > 1)),
            "scenes.move_up" => enabled(self.edit_scene_place().is_some_and(|(i, _)| i > 0)),
            "scenes.move_down" => enabled(self.edit_scene_place().is_some_and(|(i, n)| i + 1 < n)),
            "sources.add" => enabled(self.edit_scene_place().is_some()),
            "sources.remove" | "sources.properties" => enabled(self.selection().is_some()),
            // The bottom item is first in its scene and is drawn last in the Sources dock.
            "sources.move_up" => enabled(self.selected_place().is_some_and(|(i, n)| i + 1 < n)),
            "sources.move_down" => enabled(self.selected_place().is_some_and(|(i, _)| i > 0)),
            "view.stats" => on(p.stats),
            "view.scopes" => on(p.scopes),
            "view.streaming" => on(self.streaming.is_some()),
            "view.status_bar" => on(p.status_bar),
            "view.lock_preview" => on(p.lock_preview),
            "scenes.view" => on(p.scene_grid),
            "view.fullscreen" => on(self.fullscreen),
            "studio.toggle" => on(engine.studio_mode()),
            "studio.transition" => enabled(engine.studio_mode()),
            "app.command_palette" => enabled(!self.modal_open()),
            "replay.toggle" => on(self.studio.output(OutputKind::Replay).is_some_and(|s| s.active)),
            "replay.save" => enabled(self.studio.output(OutputKind::Replay).is_some_and(|s| s.active)),
            "record.marker" => enabled(self.studio.output(OutputKind::Recording).is_some_and(|s| s.active)),
            "marker.add" => enabled(self.studio.output(OutputKind::Recording).is_some_and(|s| s.active) || self.studio.audio_tracks_running()),
            "captions.toggle" => on(matches!(self.studio.captions_state(), CaptionState::Loading | CaptionState::Running)),
            "pause.toggle" => on(self.studio.paused().is_some()),
            "pause.start" => enabled(self.studio.paused().is_none()),
            "pause.stop" => enabled(self.studio.paused().is_some()),
            "pause.extend" => enabled(self.studio.paused().is_some()),
            "present.next" | "present.previous" | "present.window" => enabled(self.staged_slot(None)),
            "audio_tracks.open_last" => enabled(self.studio.audio_tracks_last_session().is_some()),
            "audio_tracks.marker" => enabled(self.studio.audio_tracks_running()),
            "dock.scenes" => on(p.docks.scenes),
            "dock.sources" => on(p.docks.sources),
            "dock.mixer" => on(p.docks.mixer),
            "dock.app_audio" => on(p.docks.app_audio),
            "dock.health" => on(p.docks.health),
            "dock.transitions" => on(p.docks.transitions),
            "dock.controls" => on(p.docks.controls),
            // The built-in layouts can always be applied; only saved ones can be removed.
            "dock.layout_remove" => enabled(!p.dock_layouts.is_empty()),
            "profile.remove" => enabled(self.profiles.len() > 1),
            "collection.remove" => enabled(self.collections.len() > 1),
            _ => ItemState::default(),
        }
    }

    /// The menu bar as the window shows it: laid out for this platform, with shortcuts and the
    /// current enabled and checked states.
    pub(crate) fn laid_menu(&self) -> MenuBar {
        self.laid_menu_for(Target::current())
    }

    /// The menu bar laid out for `target`, with shortcuts and the current enabled and checked
    /// states.
    pub(crate) fn laid_menu_for(&self, target: Target) -> MenuBar {
        let options = LayoutOptions::new(APP_NAME).label(&crate::i18n::menu_text);
        craft_menu::layout(&self.stated_menu(), target, &options).bar
    }

    /// The menu bar before it is laid out for a platform, with shortcuts and the current enabled
    /// and checked states.
    pub(crate) fn stated_menu(&self) -> Tracked<MenuBar> {
        let layouts = self.store.prefs().dock_layout_names();
        let s = commands::MenuState {
            collections: &self.collections,
            collection: &self.studio.collection().name,
            profiles: &self.profiles,
            profile: &self.studio.profile().name,
            layouts: &layouts,
        };
        let mut menus = commands::menu_bar(&s);
        if let Some(view) = menus.menus.iter_mut().find(|m| m.role == MenuRole::View) {
            view.children.extend([Node::Separator, self.language_menu()]);
        }
        let mut bar = Tracked::new(menus);
        craft_menu::apply_keymap(&mut bar, &self.keymap);
        craft_menu::apply_state(&mut bar, |c| self.item_state(&c.id));
        bar
    }

    /// View › Language (in the app menu on macOS): Automatic, then every built-in language by
    /// its own name, with the saved setting chosen. Each row runs `app.language`.
    pub(crate) fn language_menu(&self) -> Node {
        let current = match self.store.prefs().language() {
            crate::i18n::AUTO => LanguageChoice::Automatic,
            code => LanguageChoice::Code(code.to_string()),
        };
        let catalogs = self.host.i18n().unwrap_or(&crate::i18n::I18N);
        LanguageMenu::from_i18n(catalogs, current).submenu(&self.words.set::<LanguageLabels>())
    }

    /// The first shortcut of a command without parameters, as `Cmd+Z` text. A window key (F6, F10)
    /// is listed only while none of the window's commands binds the same key, since such a binding
    /// takes the key.
    fn shortcut(&self, id: &str) -> Option<String> {
        let cmd = CommandRef::new(id);
        if let Some(binding) = self.keymap.bindings(&cmd).first() {
            return Some(binding.to_string());
        }
        let key = commands::window_key(&cmd)?;
        self.keymap.holders(&key.trigger.chord).is_empty().then(|| key.to_string())
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
                        _ => return false,
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

    /// Answer the open name prompt or exit confirmation on the window's dialog host, as its buttons
    /// do. `ok` presses OK in the name prompt and Stop and Exit in the exit confirmation; otherwise
    /// Cancel is pressed. `text` replaces the name in the prompt first; the exit confirmation takes
    /// no text. A name the prompt refuses leaves the prompt open with the error under its field,
    /// and the reply carries the error in English.
    pub fn answer_dialog(&mut self, ctx: &egui::Context, ok: bool, text: Option<&str>) -> Result<Value, String> {
        let host = self.window_dialogs.clone();
        if let Some(n) = &mut self.naming {
            let (what, id) = (n.what, n.dialog);
            let (Some(host), Some(id)) = (host, id) else {
                // Before the window's first frame the prompt waits for the dialog host: it is
                // answered here, and a refusal shows under its field once it opens.
                if let Some(t) = text {
                    n.text = t.to_string();
                }
                let name = n.text.trim().to_string();
                if !ok {
                    self.naming = None;
                    return Ok(json!({"dialog": "prompt", "closed": true}));
                }
                return match self.named(what, &name) {
                    Ok(()) => {
                        self.naming = None;
                        Ok(json!({"dialog": "prompt", "closed": true, "name": name}))
                    }
                    Err(e) => {
                        let english = e.english();
                        if let Some(n) = &mut self.naming {
                            n.refused = Some(e);
                        }
                        Err(english)
                    }
                };
            };
            // An OK pressed in the window is answered by the window first; until then the prompt
            // takes no other answer.
            if matches!(host.with(|h| h.held_state(id)), Some(HeldOk::Waiting | HeldOk::Accepted)) {
                return Err("the name prompt's last OK is still being answered; ui.inspect lists the open dialogs".into());
            }
            if let Some(t) = text {
                host.with(|h| h.set_json_field(ctx, id, NAME_FIELD, json!(t)))?;
            }
            if !ok {
                host.with(|h| {
                    h.cancel_by_id(ctx, id);
                    h.take_by_id::<HostedParamDialog>(id);
                });
                self.naming = None;
                return Ok(json!({"dialog": "prompt", "closed": true}));
            }
            // OK files the name for the window to check, as the button does.
            let filed = host.with(|h| {
                h.confirm_by_id(ctx, id);
                h.get_mut_by_id::<HostedParamDialog>(id).and_then(HostedParamDialog::take_ok)
            });
            let Some(filed) = filed else {
                return Err("the name prompt's last OK is still being answered; ui.inspect lists the open dialogs".into());
            };
            let name = filed.fields.text(NAME_FIELD).unwrap_or_default().trim().to_string();
            self.answer_name(&host, what, id, &name).map_err(|e| e.english())?;
            host.with(|h| {
                h.confirm_by_id(ctx, id);
                h.take_by_id::<HostedParamDialog>(id);
            });
            self.naming = None;
            return Ok(json!({"dialog": "prompt", "closed": true, "name": name}));
        }
        if self.confirm_exit {
            if text.is_some() {
                return Err("the exit confirmation takes no `text`".into());
            }
            if let (Some(host), Some(id)) = (host, self.exit_prompt) {
                host.with(|h| {
                    h.cancel_by_id(ctx, id);
                    h.take_by_id::<StandardPrompt>(id);
                });
            }
            self.exit_answered(ctx, ok);
            return Ok(json!({"dialog": "confirm_exit", "closed": true, "exiting": ok}));
        }
        Err("no dialog is waiting for an answer; ui.inspect lists the open dialogs".into())
    }

    /// The name prompt as `ui.inspect` reports it: its title, the name in its field, and the
    /// refusal shown under the field, in English.
    fn name_prompt_json(&self, n: &Naming) -> Value {
        let (mut text, mut refused) = (n.text.clone(), n.refused.as_ref().map(Msg::english));
        if let (Some(host), Some(id)) = (&self.window_dialogs, n.dialog) {
            refused = None;
            host.with(|h| {
                if let Some(t) = h.get_by_id::<HostedParamDialog>(id).and_then(|d| d.fields().text(NAME_FIELD)) {
                    text = t.to_string();
                }
                if matches!(h.held_state(id), Some(HeldOk::Refused(_))) {
                    refused = n.refused.as_ref().map(Msg::english);
                }
            });
        }
        json!({"title": n.what.title(), "text": text, "error": refused})
    }

    /// What the window shows: its size in points, the open dialogs and windows, where the docks,
    /// the output bar and the editable view were drawn last, the menu titles, the selected item
    /// and the notices.
    pub fn ui_inspect(&self, ctx: &egui::Context) -> Value {
        let window = ctx.viewport_rect();
        let dock = |i: usize, r: egui::Rect, d: &crate::docks::Drawn| {
            json!({
                "dock": DOCK_NAMES.get(i).copied().unwrap_or("unknown"),
                "region": u8::try_from(i).map_or("bottom", |i| self.store.prefs().docks.region(i).name()),
                "rect": rect(r),
                "header": rect(d.header),
                "title": rect(d.title),
                "title_truncated": d.truncated,
                "actions": d.actions.iter().rev().map(|(name, r)| json!({"name": name, "rect": rect(*r)})).collect::<Vec<_>>(),
            })
        };
        let docks: Vec<Value> = self
            .dock_rects
            .iter()
            .map(|(i, r, d, drawn)| {
                let mut v = dock(*i, *r, d);
                v["drawn_in"] = json!(drawn.name());
                v
            })
            .collect();
        let floating: Vec<Value> = self
            .floating_rects
            .iter()
            .map(|(i, r, d, bar)| {
                let mut v = dock(*i, *r, d);
                v["bar"] = rect(*bar);
                v
            })
            .collect();
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
                    "health": crate::strip::usable(d.health_width),
                },
                "order": d.order().iter().filter_map(|i| DOCK_NAMES.get(usize::from(*i)).copied()).collect::<Vec<_>>(),
            })
        });
        let column = |c: usize, saved: f32| {
            self.dock_columns.get(c).copied().flatten().map(|r| json!({"rect": rect(r), "saved_width": crate::strip::usable(saved)}))
        };
        let columns = json!({
            "left": column(0, d.left_width),
            "right": column(1, d.right_width),
            "collapsed": self.docks_collapsed,
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
                "command_palette": self.palette.is_open(),
                "stats": self.store.prefs().stats,
                "encoders": self.encoders.is_some(),
                "prompt": self.naming.as_ref().map(|n| self.name_prompt_json(n)),
                "confirm_exit": self.confirm_exit,
                "updates": self.updates.window_open || self.updates.updater.prompt_open(),
            },
            "docks": docks,
            "floating_docks": floating,
            "dock_strip": strip,
            "dock_columns": columns,
            "dock_layouts": self.store.prefs().dock_layout_names(),
            "built_in_dock_layouts": crate::store::BUILT_IN_LAYOUTS,
            "health": self.health_seen.json(),
            "output_bar": self.output_bar.json(),
            "preview": preview,
            "studio_mode": self.studio.engine().studio_mode(),
            "side": self.shown_side().map(crate::output_bar::Side::name),
            "menus": menus,
            "selected_item": self.selected,
            "notices": self.notices.iter().map(|(n, _)| json!({"text": n.msg.english(), "error": n.error})).collect::<Vec<_>>(),
            "frames": self.frames,
        })
    }
}
