//! The command palette (View › Command Palette…, Ctrl+K or ⌘K): a search over the window's
//! commands by label, menu path or command id, with each command's shortcut.
//!
//! The palette lists the menus' commands with their menu paths, one entry per scene by its name,
//! the commands Settings › Hotkeys lists, and the other window and engine commands that run
//! without parameters. A command that cannot run in the current state is left out. The entries are
//! built when the palette opens. While it is open, each frame checks again which of them can run,
//! and the list is built again when the interface language, the scenes, the saved dock layouts,
//! the profiles or the scene collections change. A pick closes the palette and runs the command as
//! a menu row does.

use std::collections::HashSet;

use craft_keymap::CommandRef;
use craft_menu::{LayoutOptions, Target};
use craft_ui::contract::Bind;
use craft_ui::menu::HelpEntry;
use craft_ui::patterns::panels::CommandPalette;
use egui::Ui;
use serde_json::json;
use studiocraft_model::SceneId;

use crate::{APP_NAME, StudioApp, commands};

/// What the palette's candidates are built from besides the command lists: the scenes in Scenes
/// list order, the saved dock layouts' names, and the profiles and scene collections the menus
/// list with the current ones, and the interface language the labels are in.
#[derive(Default, PartialEq)]
pub(crate) struct Basis {
    lang: Option<craft_i18n::Lang>,
    scenes: Vec<(SceneId, String)>,
    layouts: Vec<String>,
    profiles: (Vec<String>, String),
    collections: (Vec<String>, String),
}

/// A command the palette can list, with the command id whose state says whether it can run now
/// and, for a Present Staged Window entry, the slot it presents.
pub(crate) struct Candidate {
    entry: HelpEntry,
    state_id: String,
    staged_slot: Option<usize>,
}

/// The palette's state: whether it is open, the commands it can list with whether each could run
/// when last checked, the entries it lists, and the commands it ran last, newest first.
#[derive(Default)]
pub(crate) struct Palette {
    open: bool,
    basis: Basis,
    candidates: Vec<Candidate>,
    runnable: Vec<bool>,
    entries: Vec<HelpEntry>,
    recent: Vec<CommandRef>,
}

impl Palette {
    /// Whether the palette is open.
    pub(crate) fn is_open(&self) -> bool {
        self.open
    }

    /// The commands the palette lists while it is open.
    pub(crate) fn entries(&self) -> &[HelpEntry] {
        &self.entries
    }

    /// Lists the candidates for which `runnable` is true, and keeps `runnable` for the next check.
    fn list(&mut self, runnable: Vec<bool>) {
        self.entries = self.candidates.iter().zip(&runnable).filter(|(_, runs)| **runs).map(|(c, _)| c.entry.clone()).collect();
        self.runnable = runnable;
    }

    /// Shows the palette while it is open. A pick closes it, and the runtime hands the command
    /// back as a fired command.
    pub(crate) fn show(&mut self, ui: &mut Ui) {
        if !self.open {
            return;
        }
        let Palette { open, entries, recent, .. } = self;
        let _ = CommandPalette::new(tl!("Command Palette").to_string(), entries.as_slice(), Bind::bound(open)).recents(Bind::bound(recent)).show(ui);
        if !self.open {
            self.basis = Basis::default();
            self.candidates = Vec::new();
            self.runnable = Vec::new();
            self.entries = Vec::new();
        }
    }
}

/// Whether a command whose parameters are written as the command lists write them (`name?:
/// string, kind: plug-in id`) runs without any: each parameter it names ends in `?`.
pub(crate) fn runs_bare(params: &str) -> bool {
    params.split(", ").all(|part| {
        let name_len = part.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(part.len());
        let (name, rest) = part.split_at(name_len);
        // A part that does not start with a parameter name is the previous parameter's
        // description (`1 to 256 characters`).
        let named = !name.is_empty() && matches!(rest.chars().next(), None | Some(':' | '?' | ','));
        !named || rest.starts_with('?')
    })
}

/// Whether `cmd` names no parameters although its command needs some, as a menu's disabled None
/// row under Docks › Remove Layout does.
fn lacks_params(cmd: &CommandRef) -> bool {
    if !cmd.params.is_null() {
        return false;
    }
    let params = if commands::is_ui_command(&cmd.id) {
        commands::ui_params(&cmd.id)
    } else {
        studiocraft_engine::specs().iter().find(|s| s.id == cmd.id).map_or("", |s| s.params)
    };
    !runs_bare(params)
}

/// The commands the palette can list, from `menus` (the menu bar's commands in menu order, with
/// their paths), the scenes in Scenes list order, and the other commands. A command is listed once,
/// the first time it comes up: menu rows first, then the scenes, the commands Settings › Hotkeys
/// lists, the window's commands and the engine's. A command with parameters that has a shortcut is
/// listed under its shortcut's key, so the palette shows the shortcut. Each candidate names the
/// command id whose state says whether it can run now.
pub(crate) fn candidates(menus: Vec<HelpEntry>, scenes: &[(SceneId, String)]) -> Vec<Candidate> {
    let catalog = commands::runtime_catalog();
    let keyed: HashSet<String> = catalog.iter().filter(|def| def.command.params.is_null()).map(|def| def.command.id.clone()).collect();
    // A command with parameters that the catalog lists under its key, as the runtime fires it.
    let as_fired = |cmd: CommandRef| if !cmd.params.is_null() && keyed.contains(&cmd.key()) { CommandRef::new(cmd.key()) } else { cmd };
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    let mut add = |entry: HelpEntry, out: &mut Vec<Candidate>| {
        let cmd = commands::fired_command(entry.command());
        let listed = !commands::runtime_runs(&cmd) && !commands::unused_edit(&cmd) && cmd.id != "app.command_palette" && !lacks_params(&cmd);
        if listed && seen.insert(cmd.key()) {
            let staged_slot = (cmd.id == "present.window")
                .then(|| cmd.params.get("index").and_then(serde_json::Value::as_u64).and_then(|n| usize::try_from(n).ok()))
                .flatten();
            out.push(Candidate { entry, state_id: cmd.id, staged_slot });
        }
    };
    for e in menus {
        let mut entry = HelpEntry::new(as_fired(e.command().clone()), e.label().clone()).with_path(e.path().iter().cloned());
        if let Some(english) = e.english() {
            entry = entry.with_english(english.clone());
        }
        add(entry, &mut out);
    }
    let scenes_title = tl!("Scenes").to_string();
    for (at, (id, name)) in scenes.iter().enumerate() {
        let cmd = if at < commands::SCENE_HOTKEYS {
            as_fired(CommandRef::with_params("scene.select_index", json!({"index": at + 1})))
        } else {
            CommandRef::with_params("scene.select", json!({"scene": id.0}))
        };
        add(HelpEntry::new(cmd, name.clone()).with_path([scenes_title.clone()]), &mut out);
    }
    // The scenes are listed by name above, so the shortcut list's Show Scene 1 to 9 are not.
    for def in catalog.into_iter().filter(|def| !def.command.id.starts_with("scene.select_index")) {
        let label = crate::settings::hotkey_label(&commands::fired_command(&def.command), &def.label);
        let english = def.label.clone();
        let entry = HelpEntry::new(def.command, label.clone());
        add(if label == english { entry } else { entry.with_english(english) }, &mut out);
    }
    let bare_ui = commands::UI_COMMANDS.iter().filter(|(id, _)| runs_bare(commands::ui_params(id)));
    let bare_engine = studiocraft_engine::specs().iter().filter(|s| runs_bare(s.params)).map(|s| (s.id, s.label));
    for (id, english) in bare_ui.copied().chain(bare_engine) {
        let label = crate::i18n::command_label(id, english);
        let entry = HelpEntry::new(CommandRef::new(id), label.to_string());
        add(if label == english { entry } else { entry.with_english(english) }, &mut out);
    }
    out
}

impl StudioApp {
    /// What the palette's candidates are built from in the current state.
    fn palette_basis(&self) -> Basis {
        let scenes = self.studio.collection().scenes.iter().map(|s| (s.id, s.name.clone())).collect();
        Basis {
            lang: Some(crate::i18n::lang()),
            scenes,
            layouts: self.store.prefs().dock_layout_names(),
            profiles: (self.profiles.clone(), self.studio.profile().name.clone()),
            collections: (self.collections.clone(), self.studio.collection().name.clone()),
        }
    }

    /// The palette's candidates over `basis`: the menu bar's commands as the bar shows them, with
    /// their paths, and the other commands ([`candidates`]).
    fn palette_candidates(&self, basis: &Basis) -> Vec<Candidate> {
        let options = LayoutOptions::new(APP_NAME).label(&crate::i18n::menu_text);
        let menus = self.stated_menu().help_entries(Target::current(), &options, Some(&commands::english_label));
        candidates(menus, &basis.scenes)
    }

    /// Whether each of `candidates` can run now. A Present Staged Window entry also needs a window
    /// or display in its slot.
    fn palette_runnable(&self, candidates: &[Candidate]) -> Vec<bool> {
        candidates.iter().map(|c| self.item_state(&c.state_id).enabled && c.staged_slot.is_none_or(|n| self.staged_slot(Some(n)))).collect()
    }

    /// The palette's entries in the current state: the candidates that can run now.
    #[cfg(test)]
    pub(crate) fn palette_entries(&self) -> Vec<HelpEntry> {
        let candidates = self.palette_candidates(&self.palette_basis());
        let runnable = self.palette_runnable(&candidates);
        candidates.into_iter().zip(runnable).filter(|(_, runs)| *runs).map(|(c, _)| c.entry).collect()
    }

    /// Opens the command palette over the commands that can run now.
    pub(crate) fn open_palette(&mut self) {
        let basis = self.palette_basis();
        self.palette.candidates = self.palette_candidates(&basis);
        self.palette.basis = basis;
        let runnable = self.palette_runnable(&self.palette.candidates);
        self.palette.list(runnable);
        self.palette.open = true;
    }

    /// While the palette is open, lists again the candidates that can run now. When the interface
    /// language, the scenes, the saved dock layouts, the profiles or the scene collections changed
    /// since the candidates were built, builds them again first.
    pub(crate) fn refresh_palette(&mut self) {
        if !self.palette.open {
            return;
        }
        let basis = self.palette_basis();
        if basis != self.palette.basis {
            self.palette.candidates = self.palette_candidates(&basis);
            self.palette.basis = basis;
            self.palette.runnable = Vec::new();
        }
        let runnable = self.palette_runnable(&self.palette.candidates);
        if runnable != self.palette.runnable {
            self.palette.list(runnable);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_runs_bare_when_every_parameter_it_names_is_optional() {
        let cases = [
            ("", true),
            ("name?: string", true),
            ("kind?: transition plug-in id, ms?: milliseconds", true),
            ("name?: the marker's name, 1 to 256 characters", true),
            ("source?: id or name of a Staged windows source (all when absent)", true),
            ("item, scene?", false),
            ("scene: id or name", false),
            ("kind: plug-in id, scene?, name?, settings?, transform?", false),
            ("index: 1-based position in the Scenes list", false),
            ("scenes: up to 16 scene ids or names, [] to stop, fps?: 0.1 to 4 pictures a second (2 by default)", false),
            ("name?: string, enabled: boolean", false),
        ];
        for (params, bare) in cases {
            assert_eq!(runs_bare(params), bare, "{params}");
        }
    }

    #[test]
    fn a_menu_row_without_the_parameters_its_command_needs_is_not_a_candidate() {
        let path = ["Docks", "Remove Layout"];
        let menus = vec![
            HelpEntry::new(CommandRef::new("dock.layout_remove"), "None").with_path(path),
            HelpEntry::new(CommandRef::with_params("dock.layout_remove", json!({"name": "Talk"})), "Talk").with_path(path),
            HelpEntry::new(CommandRef::new("stream.destination_remove"), "Remove").with_path(["Streaming"]),
            HelpEntry::new(CommandRef::new("view.stats"), "Stats").with_path(["View"]),
        ];
        let keys: Vec<String> = candidates(menus, &[]).iter().map(|c| c.entry.command().key()).collect();
        let talk = CommandRef::with_params("dock.layout_remove", json!({"name": "Talk"})).key();
        assert!(keys.contains(&talk) && keys.contains(&"view.stats".to_string()), "{keys:?}");
        assert!(!keys.iter().any(|k| k == "dock.layout_remove" || k == "stream.destination_remove"), "{keys:?}");
    }
}
