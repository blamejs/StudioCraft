//! The window's own commands, the shortcut catalog and the menu bar.
//!
//! Engine commands (`scene.add`, `record.toggle`, …) run through the studio; the ids here are the
//! window's (`view.stats`, `profile.switch`, …). Menus, shortcuts and the control channel all name
//! a command by id with JSON parameters.

use std::collections::HashMap;
use std::sync::LazyLock;

use craft_keymap::{Binding, CommandDef, CommandRef, Keymap, Overrides, Platform, Scope, Scopes};
use craft_menu::{Item, ItemRole, Labeled, Menu, MenuBar, MenuRole, Node};
use craft_ui::runtime::commands::{ChromeCommand, CommandGate, CommandSet as _, EditCommands, NavCommand};
use craft_ui::shell::ScreenModeCommands;
use serde_json::json;

/// Help › Community: (command id, link key in `studiocraft_engine::links`, label).
pub const COMMUNITY: [(&str, &str, &str); 4] = [
    ("help.discord", "discord", "Discord Server"),
    ("help.website", "website", "Website"),
    ("help.github", "github", "Source Code"),
    ("help.issues", "issues", "Report an Issue"),
];

/// The link a `help.*` opener command opens.
pub fn community_link(id: &str) -> Option<&'static str> {
    COMMUNITY.iter().find(|(c, _, _)| *c == id).and_then(|(_, key, _)| studiocraft_engine::links::get(key))
}

/// Scenes 1 to 9 get Cmd+1 to Cmd+9 (Ctrl on Windows and Linux).
pub const SCENE_HOTKEYS: usize = 9;

/// Commands the window carries out itself, with their English labels.
pub const UI_COMMANDS: &[(&str, &str)] = &[
    ("file.show_recordings", "Show Recordings"),
    ("file.show_settings_folder", "Show Settings Folder"),
    ("view.stats", "Stats"),
    ("view.scopes", "Scopes"),
    ("scopes.kind", "Scope Type"),
    ("view.status_bar", "Status Bar"),
    ("view.fullscreen", "Full-Screen Interface"),
    ("view.lock_preview", "Lock Preview"),
    ("view.streaming", "Streaming"),
    ("stream.destinations", "List Stream Destinations"),
    ("stream.destination_add", "Add Stream Destination"),
    ("stream.destination_remove", "Remove Stream Destination"),
    ("stream.destination_set", "Change Stream Destination"),
    ("stream.destination_start", "Start Stream Destination"),
    ("stream.destination_stop", "Stop Stream Destination"),
    ("dock.scenes", "Scenes"),
    ("dock.sources", "Sources"),
    ("dock.mixer", "Audio Mixer"),
    ("dock.app_audio", "Application Audio"),
    ("dock.controls", "Output Controls"),
    ("dock.transitions", "Transition Controls"),
    ("dock.move", "Move Dock"),
    ("dock.float", "Undock"),
    ("dock.layout_save", "Save Dock Layout…"),
    ("dock.layout_apply", "Apply Dock Layout"),
    ("dock.layout_remove", "Remove Dock Layout"),
    ("dock.reset", "Reset Docks"),
    ("profile.new", "New Profile…"),
    ("profile.duplicate", "Duplicate Profile…"),
    ("profile.rename", "Rename Profile…"),
    ("profile.remove", "Remove Profile"),
    ("profile.switch", "Switch Profile"),
    ("collection.new", "New Scene Collection…"),
    ("collection.duplicate", "Duplicate Scene Collection…"),
    ("collection.rename", "Rename Scene Collection…"),
    ("collection.remove", "Remove Scene Collection"),
    ("collection.switch", "Switch Scene Collection"),
    ("collection.import", "Import Scene Collection…"),
    ("collection.export", "Export Scene Collection…"),
    ("tools.encoders", "Available Encoders"),
    ("video.displays", "List Displays"),
    ("video.canvas_from_display", "Canvas Size from Display"),
    ("selection.properties", "Properties…"),
    ("selection.filters", "Filters…"),
    ("selection.remove", "Remove Selected Source"),
    ("selection.fit", "Fit to Screen"),
    ("selection.stretch", "Stretch to Screen"),
    ("selection.center", "Center to Screen"),
    ("selection.reset", "Reset Transform"),
    ("selection.rotate_cw", "Rotate 90° Clockwise"),
    ("selection.rotate_ccw", "Rotate 90° Counterclockwise"),
    ("app.about", "About StudioCraft"),
    ("app.licenses", "Licenses"),
    ("app.language", "Language"),
    ("app.check_updates", "Check for Updates…"),
    ("help.discord", "Discord Server"),
    ("help.website", "Website"),
    ("help.github", "Source Code"),
    ("help.issues", "Report an Issue"),
    ("window.next_pane", "Next Pane"),
    ("window.previous_pane", "Previous Pane"),
    ("window.focus_menu_bar", "Focus Menu Bar"),
    ("scenes.add", "Add Scene"),
    ("scenes.remove", "Remove Scene"),
    ("scenes.move_up", "Move Scene Up"),
    ("scenes.move_down", "Move Scene Down"),
    ("sources.add", "Add Source"),
    ("sources.remove", "Remove Source"),
    ("sources.move_up", "Move Source Up"),
    ("sources.move_down", "Move Source Down"),
    ("sources.properties", "Source Properties"),
];

/// The header buttons of the Scenes dock: (name in `ui.inspect`, icon, command, English tooltip),
/// in reading order. The `scenes.*` commands act on the scene being edited.
pub const SCENE_HEADER: [(&str, &str, &str, &str); 4] = [
    ("add", "plus", "scenes.add", "Add a scene"),
    ("remove", "minus", "scenes.remove", "Remove the selected scene"),
    ("up", "chevron_up", "scenes.move_up", "Move the scene up"),
    ("down", "chevron_down", "scenes.move_down", "Move the scene down"),
];

/// The header buttons of the Sources dock, as [`SCENE_HEADER`] lists those of the Scenes dock. The
/// `sources.*` commands act on the selected source of the scene being edited, and `sources.add`
/// opens the Add menu under its button.
pub const SOURCE_HEADER: [(&str, &str, &str, &str); 5] = [
    ("add", "plus", "sources.add", "Add a source"),
    ("remove", "minus", "sources.remove", "Remove the selected source from the scene"),
    ("up", "chevron_up", "sources.move_up", "Move the source up"),
    ("down", "chevron_down", "sources.move_down", "Move the source down"),
    ("properties", "settings", "sources.properties", "Properties"),
];

/// Whether the UI runtime carries out `cmd` itself: the window keys F6, Shift+F6 and F10, and the
/// panel toggles a bare Tab fires where a window lets Tab hide its panels. A key runs them before
/// the window sees it, and a menu or control channel run reaches the runtime through
/// `craft_ui::runtime::commands::fire`. StudioCraft lets Tab move the keyboard focus, so it lists
/// no panel toggle among its commands.
pub fn runtime_runs(cmd: &CommandRef) -> bool {
    ChromeCommand::from_ref(cmd).is_some() || NavCommand::from_ref(cmd).is_some()
}

/// The gate over the window's commands: the window has one screen mode, so the shell's
/// `screenMode.*` commands, and with them F and Shift+F, do not fire.
pub fn gate() -> CommandGate {
    CommandGate::new((), |_: &(), cmd| ScreenModeCommands::from_ref(cmd).is_none())
}

/// The first default key of a window key command ([`runtime_runs`]), such as F6. The key runs the
/// command only while none of the window's own commands binds it.
pub fn window_key(cmd: &CommandRef) -> Option<Binding> {
    let mut defs = [NavCommand::defs(), ChromeCommand::defs()].into_iter().flatten().flatten();
    defs.find(|def| def.command == *cmd)?.defaults.first().cloned()
}

/// Whether `cmd` is a command of the framework's edit set that the window does nothing with:
/// Cut, Copy, Paste, Select All and Delete. A focused text field takes their keys itself; with
/// nothing focused, the window drops them. Undo and Redo are engine commands.
pub fn unused_edit(cmd: &CommandRef) -> bool {
    EditCommands::from_ref(cmd).is_some() && !studiocraft_engine::specs().iter().any(|s| s.id == cmd.id)
}

/// Whether a command of the framework's linked command sets has the id of `cmd`.
fn linked(cmd: &CommandRef) -> bool {
    EditCommands::from_ref(cmd).is_some() || runtime_runs(cmd)
}

/// Whether `id` is one of the window's commands.
pub fn is_ui_command(id: &str) -> bool {
    UI_COMMANDS.iter().any(|(c, _)| *c == id)
}

/// The parameters a window command takes, written as the engine's command list writes them.
pub fn ui_params(id: &str) -> &'static str {
    match id {
        "profile.switch" | "collection.switch" => "name: string",
        "app.language" => "lang: string (auto, or the code of a built-in language)",
        "collection.import" | "collection.export" => "path?: string",
        "scopes.kind" => "kind: string (waveform, parade, histogram, vectorscopeYuv or vectorscopeHls)",
        "stream.destination_add" => {
            "name?: string, server?: string, enabled?: boolean, key?: string (profile, held or none), held_name?: string (with key held)"
        }
        "stream.destination_set" => {
            "id: string, name?: string, server?: string, enabled?: boolean, key?: string (profile, held or none), held_name?: string (with key held)"
        }
        "stream.destination_remove" | "stream.destination_start" | "stream.destination_stop" => "id: string",
        "dock.move" => "dock: string (scenes, sources, mixer or app_audio), to: string (left or right)",
        "dock.float" => "dock: string (scenes, sources, mixer or app_audio), floating: boolean (true for a window of its own, false for the strip)",
        "dock.layout_save" => "name?: string (without it the window asks for one)",
        "dock.layout_apply" | "dock.layout_remove" => "name: string",
        "video.canvas_from_display" => "display?: string (an id from video.displays; without it, the display the window is on)",
        _ => "",
    }
}

/// Whether a window command changes the scene being edited through a command undo takes back.
pub fn ui_undoable(id: &str) -> bool {
    matches!(
        id,
        "selection.remove"
            | "scenes.add"
            | "scenes.remove"
            | "scenes.move_up"
            | "scenes.move_down"
            | "sources.remove"
            | "sources.move_up"
            | "sources.move_down"
            | "selection.fit"
            | "selection.stretch"
            | "selection.center"
            | "selection.reset"
            | "selection.rotate_cw"
            | "selection.rotate_ccw"
    )
}

fn bindable(cmd: CommandRef, label: &str, defaults: &[&str]) -> CommandDef {
    defaults.iter().filter_map(|d| Binding::parse(d).ok()).fold(CommandDef::new(cmd, label, Scope::app()), CommandDef::with_default)
}

/// Every command a shortcut can be given, with the default shortcuts.
pub fn catalog() -> Vec<CommandDef> {
    let c = CommandRef::new;
    let mut list = vec![
        bindable(c("edit.undo"), "Undo", &["Cmd+Z"]),
        bindable(c("edit.redo"), "Redo", &["Cmd+Y", "Cmd+Shift+Z"]),
        bindable(c("record.toggle"), "Start or Stop Recording", &[]),
        bindable(c("stream.toggle"), "Start or Stop Streaming", &[]),
        bindable(c("replay.toggle"), "Start or Stop Replay Buffer", &[]),
        bindable(c("replay.save"), "Save Replay", &[]),
        bindable(c("captions.toggle"), "Start or Stop Live Captions", &[]),
        bindable(c("captions.download"), "Download Speech Model", &[]),
        bindable(c("audio_tracks.open_last"), "Open Last Audio Tracks in Editor", &[]),
        bindable(c("audio_tracks.marker"), "Add Audio Tracks Marker", &[]),
        bindable(c("studio.toggle"), "Studio Mode", &[]).toggle(),
        bindable(c("studio.transition"), "Transition to Program", &[]),
        bindable(CommandRef::with_params("studio.transition", json!({"kind": "cut"})), "Cut to Program", &[]),
        bindable(c("scene.add"), "Add Scene", &[]),
        bindable(c("selection.remove"), "Remove Selected Source", &["Delete"]),
        bindable(c("selection.properties"), "Source Properties", &[]),
        bindable(c("selection.fit"), "Fit to Screen", &["Cmd+F"]),
        bindable(c("selection.reset"), "Reset Transform", &["Cmd+R"]),
        bindable(c("view.stats"), "Stats", &[]).toggle(),
        bindable(c("view.scopes"), "Scopes", &[]).toggle(),
        bindable(c("view.fullscreen"), "Full-Screen Interface", &["F11"]).toggle(),
        bindable(c("view.lock_preview"), "Lock Preview", &[]).toggle(),
        bindable(c("app.settings"), "Settings", &[]),
        bindable(c("app.exit"), "Exit", &[]),
    ];
    for i in 1..=SCENE_HOTKEYS {
        let cmd = CommandRef::with_params("scene.select_index", json!({"index": i}));
        list.push(bindable(cmd, &format!("Show Scene {i}"), &[&format!("Cmd+{i}")]));
    }
    list
}

pub fn keymap(overrides: Overrides) -> Keymap {
    Keymap::new(catalog(), Scopes::new(), Platform::current()).with_overrides(overrides)
}

/// [`catalog`] as the runtime's command catalog takes it: one entry per id, without the commands
/// a linked framework set defines ([`linked_defaults`] gives those their shortcuts). An entry with
/// parameters goes under its binding key (`scene.select_index {"index":1}`), which is also the key
/// its saved shortcuts are stored under; [`fired_command`] turns it back. The docks' header
/// buttons ([`SCENE_HEADER`], [`SOURCE_HEADER`]) are in it too, with no shortcut.
pub fn runtime_catalog() -> Vec<CommandDef> {
    let header = SCENE_HEADER.iter().chain(&SOURCE_HEADER).map(|(_, _, id, _)| {
        let label = UI_COMMANDS.iter().find(|(c, _)| c == id).map_or(*id, |(_, l)| *l);
        CommandDef::new(CommandRef::new(*id), label, Scope::app())
    });
    catalog()
        .into_iter()
        .filter(|def| !linked(&def.command))
        .map(|mut def| {
            if def.command.params.is_null() {
                def
            } else {
                def.command = CommandRef::new(def.command.key());
                def
            }
        })
        .chain(header)
        .collect()
}

/// The default shortcuts the window gives commands of the framework's linked sets, in place of
/// the sets' own: Undo is Cmd+Z, Redo is Cmd+Y and Cmd+Shift+Z, and Delete keeps only Backspace,
/// so a bare Delete removes the selected source (`selection.remove`).
pub fn linked_defaults() -> Vec<(String, Vec<Binding>)> {
    let mut list: Vec<(String, Vec<Binding>)> =
        catalog().into_iter().filter(|def| linked(&def.command)).map(|def| (def.command.id, def.defaults)).collect();
    list.push(("edit.delete".to_string(), Binding::parse("Backspace").into_iter().collect()));
    list
}

/// The command a fired catalog entry stands for: the entry of [`runtime_catalog`] with its
/// parameters back.
pub fn fired_command(fired: &CommandRef) -> CommandRef {
    if fired.params.is_null() && fired.id.contains(' ') { CommandRef::parse_key(&fired.id).unwrap_or_else(|| fired.clone()) } else { fired.clone() }
}

/// What the menus show for the current state.
pub struct MenuState<'a> {
    pub collections: &'a [String],
    pub collection: &'a str,
    pub profiles: &'a [String],
    pub profile: &'a str,
    /// The names of the saved dock layouts.
    pub layouts: &'a [String],
}

/// A menu item, its label in the interface language.
fn item(id: &str, label: &str) -> Node {
    Node::Item(Item::new(CommandRef::new(id), crate::i18n::command_label(id, label)))
}

/// A menu or submenu title in the interface language.
fn title(text: &str) -> &str {
    tlc!("menu", text)
}

/// The mnemonic letters the menus prefer, by command id, as desktop menus write them: E&xit,
/// &Undo, &Redo and &About. A row whose label in the interface language lacks its letter takes
/// the first free letter of the label, as every other row does.
pub const MNEMONICS: &[(&str, char)] = &[("app.exit", 'x'), ("edit.undo", 'U'), ("edit.redo", 'R'), ("app.about", 'A')];

/// The menu bar's English labels: every menu and submenu title, and each item's label by its
/// command's key.
struct EnglishLabels {
    titles: Vec<String>,
    items: HashMap<String, String>,
}

static ENGLISH: LazyLock<EnglishLabels> = LazyLock::new(|| {
    fn walk(nodes: &[Node], english: &mut EnglishLabels) {
        for node in nodes {
            match node {
                Node::Item(it) => {
                    english.items.entry(it.command.key()).or_insert_with(|| it.label.clone());
                }
                Node::Submenu { label, children, .. } => {
                    english.titles.push(label.clone());
                    walk(children, english);
                }
                _ => {}
            }
        }
    }
    let lang = crate::i18n::lang();
    crate::i18n::set_lang(craft_i18n::Lang::EN);
    let bar = menu_bar(&MenuState { collections: &[], collection: "", profiles: &[], profile: "", layouts: &[] });
    crate::i18n::set_lang(lang);
    // The window adds View › Language after the menus are built; its title is the catalog's.
    let mut english = EnglishLabels { titles: vec!["Language".to_string()], items: HashMap::new() };
    for menu in &bar.menus {
        english.titles.push(menu.title.clone());
        walk(&menu.children, &mut english);
    }
    english
});

/// The English label of a menu title or row, which the menu bar takes its mnemonic from when the
/// label in the interface language has no letter a keyboard types (Japanese, Chinese, Korean):
/// `File` for `ファイル`, shown as `ファイル(F)`. Rows the menus make from names (profiles, scene
/// collections, dock layouts, languages) have none.
pub fn english_label(labeled: Labeled<'_>) -> Option<String> {
    match labeled {
        Labeled::Command(cmd) => ENGLISH.items.get(&cmd.key()).cloned(),
        Labeled::Title(shown) => ENGLISH.titles.iter().find(|t| t.as_str() == shown || title(t) == shown).cloned(),
        _ => None,
    }
}

/// One item per saved dock layout running `id` with its name, or a disabled None when there is
/// no saved layout.
fn layout_items(id: &str, names: &[String]) -> Vec<Node> {
    if names.is_empty() {
        return vec![Node::Item(Item::new(CommandRef::new(id), title("None")).enabled(false))];
    }
    names.iter().map(|n| Node::Item(Item::new(CommandRef::with_params(id, json!({"name": n})), n.clone()))).collect()
}

/// The menu bar before platform layout, in the interface language: File, Edit, View, Docks,
/// Profile, Scene Collection, Tools and Help.
pub fn menu_bar(s: &MenuState) -> MenuBar {
    let file = Menu::new(
        title("File"),
        vec![
            item("file.show_recordings", "Show Recordings"),
            Node::Separator,
            Node::Item(Item::new(CommandRef::new("replay.toggle"), crate::i18n::command_label("replay.toggle", "Replay Buffer")).check(false)),
            item("replay.save", "Save Replay"),
            item("audio_tracks.marker", "Add Audio Tracks Marker"),
            item("audio_tracks.open_last", "Open Last Audio Tracks in Editor"),
            Node::Separator,
            item("app.settings", "Settings"),
            item("file.show_settings_folder", "Show Settings Folder"),
            Node::Separator,
            item("app.exit", "Exit"),
        ],
    )
    .role(MenuRole::File);
    let transform = Node::submenu(
        title("Transform"),
        vec![
            item("selection.reset", "Reset Transform"),
            Node::Separator,
            item("selection.rotate_cw", "Rotate 90° Clockwise"),
            item("selection.rotate_ccw", "Rotate 90° Counterclockwise"),
            Node::Separator,
            item("selection.fit", "Fit to Screen"),
            item("selection.stretch", "Stretch to Screen"),
            item("selection.center", "Center to Screen"),
        ],
    );
    let edit = Menu::new(
        title("Edit"),
        vec![
            item("edit.undo", "Undo"),
            item("edit.redo", "Redo"),
            Node::Separator,
            transform,
            item("selection.properties", "Properties…"),
            item("selection.filters", "Filters…"),
            item("selection.remove", "Remove Selected Source"),
            Node::Separator,
            Node::Item(Item::new(CommandRef::new("view.lock_preview"), crate::i18n::command_label("view.lock_preview", "Lock Preview")).check(false)),
        ],
    )
    .role(MenuRole::Edit);
    let check = |id: &str, label: &str| Node::Item(Item::new(CommandRef::new(id), crate::i18n::command_label(id, label)).check(false));
    let view = Menu::new(
        title("View"),
        vec![
            check("view.fullscreen", "Full-Screen Interface"),
            Node::Separator,
            check("studio.toggle", "Studio Mode"),
            check("view.stats", "Stats"),
            check("view.scopes", "Scopes"),
            check("view.streaming", "Streaming"),
            check("view.status_bar", "Status Bar"),
        ],
    )
    .role(MenuRole::View);
    let docks = Menu::new(
        title("Docks"),
        vec![
            check("dock.scenes", "Scenes"),
            check("dock.sources", "Sources"),
            check("dock.mixer", "Audio Mixer"),
            check("dock.app_audio", "Application Audio"),
            // The two groups of the output bar: Edit and Studio, Record and Go Live; and the
            // transition picker with its duration.
            check("dock.controls", "Output Controls"),
            check("dock.transitions", "Transition Controls"),
            Node::Separator,
            item("dock.layout_save", "Save Dock Layout…"),
            Node::submenu(title("Layouts"), layout_items("dock.layout_apply", s.layouts)),
            Node::submenu(title("Remove Layout"), layout_items("dock.layout_remove", s.layouts)),
            Node::Separator,
            item("dock.reset", "Reset Docks"),
        ],
    );
    let list = |id: &str, names: &[String], current: &str| -> Vec<Node> {
        names.iter().map(|n| Node::Item(Item::new(CommandRef::with_params(id, json!({"name": n})), n.clone()).radio(n == current))).collect()
    };
    let mut profile = vec![
        item("profile.new", "New…"),
        item("profile.duplicate", "Duplicate…"),
        item("profile.rename", "Rename…"),
        Node::Item(Item::new(CommandRef::new("profile.remove"), title("Remove")).enabled(s.profiles.len() > 1)),
        Node::Separator,
    ];
    profile.extend(list("profile.switch", s.profiles, s.profile));
    let mut collection = vec![
        item("collection.new", "New…"),
        item("collection.duplicate", "Duplicate…"),
        item("collection.rename", "Rename…"),
        Node::Item(Item::new(CommandRef::new("collection.remove"), title("Remove")).enabled(s.collections.len() > 1)),
        Node::Separator,
        item("collection.import", "Import…"),
        item("collection.export", "Export…"),
        Node::Separator,
    ];
    collection.extend(list("collection.switch", s.collections, s.collection));
    let tools = Menu::new(
        title("Tools"),
        vec![
            Node::Item(Item::new(CommandRef::new("captions.toggle"), crate::i18n::command_label("captions.toggle", "Live Captions")).check(false)),
            item("captions.download", "Download Speech Model"),
            Node::Separator,
            item("tools.encoders", "Available Encoders"),
            item("hardware_encoders.retest", "Test Hardware Encoders Again"),
        ],
    );
    let community = COMMUNITY.iter().map(|(id, _, label)| item(id, label)).collect();
    let help = Menu::new(
        title("Help"),
        vec![
            Node::submenu(title("Community"), community),
            Node::Separator,
            item("app.check_updates", "Check for Updates…"),
            item("app.about", "About StudioCraft"),
        ],
    )
    .role(MenuRole::Help);
    let mut bar = MenuBar::new(vec![
        file,
        edit,
        view,
        docks,
        Menu::new(title("Profile"), profile),
        Menu::new(title("Scene Collection"), collection),
        tools,
        help,
    ]);
    bar.tag_item(&CommandRef::new("app.about"), ItemRole::About);
    bar.tag_item(&CommandRef::new("app.settings"), ItemRole::Settings);
    bar.tag_item(&CommandRef::new("app.exit"), ItemRole::Quit);
    bar
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_catalog_entry_is_a_known_command() {
        for b in catalog() {
            let id = b.command.id.as_str();
            assert!(is_ui_command(id) || studiocraft_engine::specs().iter().any(|s| s.id == id), "{id}");
        }
    }

    #[test]
    fn default_shortcuts_parse_and_do_not_clash() {
        let km = keymap(Overrides::new());
        let undo = km.bindings(&CommandRef::new("edit.undo"));
        assert_eq!(undo.len(), 1);
        let scene3 = km.bindings(&CommandRef::with_params("scene.select_index", json!({"index": 3})));
        assert_eq!(scene3.len(), 1);
        assert!(km.problems().is_empty(), "{:?}", km.problems());
        // The linked sets' commands leave the runtime catalog and take these shortcuts instead.
        assert!(runtime_catalog().iter().all(|d| !linked(&d.command)));
        let text = |id: &str| -> Vec<String> {
            linked_defaults().into_iter().find(|(c, _)| c == id).map(|(_, b)| b.iter().map(ToString::to_string).collect()).unwrap_or_default()
        };
        assert_eq!(text("edit.undo"), ["Cmd+Z"]);
        assert_eq!(text("edit.redo"), ["Cmd+Y", "Cmd+Shift+Z"]);
        assert_eq!(text("edit.delete"), ["Backspace"]);
    }

    #[test]
    fn menus_reach_every_command_they_name() {
        let names = vec!["A".to_string(), "B".to_string()];
        let bar = menu_bar(&MenuState { collections: &names, collection: "A", profiles: &names, profile: "B", layouts: &names });
        let titles: Vec<_> = bar.menus.iter().map(|m| m.title.as_str()).collect();
        assert_eq!(titles, ["File", "Edit", "View", "Docks", "Profile", "Scene Collection", "Tools", "Help"]);
        for it in bar.items() {
            let id = it.command.id.as_str();
            assert!(is_ui_command(id) || studiocraft_engine::specs().iter().any(|s| s.id == id), "{id}");
        }
    }

    /// A menu row as the tests compare it: command id, label, parameters, enabled and checked.
    type Row = (String, String, serde_json::Value, bool, Option<bool>);

    /// The item rows of `menu`, or of its submenu `submenu`, without separators and submenus.
    fn rows(bar: &MenuBar, menu: &str, submenu: Option<&str>) -> Vec<Row> {
        let Some(m) = bar.menus.iter().find(|m| m.title == menu) else {
            panic!("no {menu} menu");
        };
        let nodes = match submenu {
            None => &m.children,
            Some(name) => match m.children.iter().find(|n| matches!(n, Node::Submenu { label, .. } if label == name)) {
                Some(Node::Submenu { children, .. }) => children,
                _ => panic!("no {name} submenu in {menu}"),
            },
        };
        nodes
            .iter()
            .filter_map(|n| match n {
                Node::Item(i) => Some((i.command.id.clone(), i.label.clone(), i.command.params.clone(), i.enabled, i.kind.checked())),
                _ => None,
            })
            .collect()
    }

    fn row(id: &str, label: &str, params: serde_json::Value, enabled: bool, checked: Option<bool>) -> Row {
        (id.to_string(), label.to_string(), params, enabled, checked)
    }

    #[test]
    fn menus_hold_the_rows_the_control_channel_lists() {
        let null = serde_json::Value::Null;
        let plain = |id: &str, label: &str| row(id, label, serde_json::Value::Null, true, None);
        let check = |id: &str, label: &str| row(id, label, serde_json::Value::Null, true, Some(false));
        let none = |id: &str| row(id, "None", serde_json::Value::Null, false, None);
        let layout = |id: &str, name: &str| row(id, name, json!({"name": name}), true, None);
        let saved = ["Talk".to_string(), "Game".to_string()];
        // (saved dock layouts, menu, submenu, rows)
        let cases: [(&[String], &str, Option<&str>, Vec<Row>); 6] = [
            (
                &[],
                "Docks",
                None,
                vec![
                    check("dock.scenes", "Scenes"),
                    check("dock.sources", "Sources"),
                    check("dock.mixer", "Audio Mixer"),
                    check("dock.app_audio", "Application Audio"),
                    check("dock.controls", "Output Controls"),
                    check("dock.transitions", "Transition Controls"),
                    plain("dock.layout_save", "Save Dock Layout…"),
                    plain("dock.reset", "Reset Docks"),
                ],
            ),
            (&[], "Docks", Some("Layouts"), vec![none("dock.layout_apply")]),
            (&[], "Docks", Some("Remove Layout"), vec![none("dock.layout_remove")]),
            (&saved, "Docks", Some("Layouts"), vec![layout("dock.layout_apply", "Talk"), layout("dock.layout_apply", "Game")]),
            (&[], "Help", Some("Community"), COMMUNITY.iter().map(|(id, _, label)| row(id, label, null.clone(), true, None)).collect()),
            (&[], "Help", None, vec![plain("app.check_updates", "Check for Updates…"), plain("app.about", "About StudioCraft")]),
        ];
        for (layouts, menu, submenu, expected) in cases {
            let bar = menu_bar(&MenuState { collections: &[], collection: "", profiles: &[], profile: "", layouts });
            let got = rows(&bar, menu, submenu);
            assert_eq!(got, expected, "{menu} › {submenu:?} with {} saved layouts", layouts.len());
            // A row that names a window command shows the command's own label when it has no
            // parameters and is not a placeholder.
            for (id, label, ..) in got.iter().filter(|r| r.2.is_null() && r.3 && is_ui_command(&r.0)) {
                assert_eq!(UI_COMMANDS.iter().find(|(c, _)| c == id).map(|(_, l)| *l), Some(label.as_str()), "{id}");
            }
        }
        let with_layouts = menu_bar(&MenuState { collections: &[], collection: "", profiles: &[], profile: "", layouts: &saved });
        assert_eq!(rows(&with_layouts, "Docks", Some("Remove Layout")).len(), 2);
        for (id, _, _) in COMMUNITY {
            assert!(is_ui_command(id), "{id}");
            assert!(community_link(id).is_some_and(|u| u.starts_with("https://")), "{id}");
        }
        assert_eq!(community_link("help.github"), Some(studiocraft_engine::links::GITHUB));
        assert_eq!(community_link("help.unknown"), None);
        assert!(!is_ui_command("app.links"), "app.links runs in the engine");
        assert!(is_ui_command("app.check_updates"));
        assert_eq!(ui_params("app.check_updates"), "");
        assert!(!ui_undoable("app.check_updates"));
        // The header buttons that change the scene collection are undone like the engine
        // commands they run.
        for (_, _, id, _) in SCENE_HEADER.iter().chain(&SOURCE_HEADER) {
            assert_eq!(ui_undoable(id), !matches!(*id, "sources.add" | "sources.properties"), "{id}");
        }
    }

    #[test]
    fn the_window_keys_are_the_frameworks_commands() {
        let keys: Vec<(&str, Option<String>)> = UI_COMMANDS
            .iter()
            .filter(|(id, _)| id.starts_with("window."))
            .map(|(id, _)| (*id, window_key(&CommandRef::new(*id)).map(|b| b.to_string())))
            .collect();
        assert_eq!(
            keys,
            [
                ("window.next_pane", Some("F6".to_string())),
                ("window.previous_pane", Some("Shift+F6".to_string())),
                ("window.focus_menu_bar", Some("F10".to_string())),
            ]
        );
        for (id, _) in keys {
            assert!(runtime_runs(&CommandRef::new(id)), "{id}");
            assert!(!unused_edit(&CommandRef::new(id)), "{id}");
        }
        assert!(!runtime_runs(&CommandRef::new("view.stats")));
        assert_eq!(window_key(&CommandRef::new("view.stats")), None);
        for id in ["edit.cut", "edit.copy", "edit.paste", "edit.select_all", "edit.delete"] {
            assert!(unused_edit(&CommandRef::new(id)), "{id}");
        }
        for id in ["edit.undo", "edit.redo", "selection.remove"] {
            assert!(!unused_edit(&CommandRef::new(id)), "{id}");
        }
    }

    #[test]
    fn menu_labels_have_english_letters_for_the_bar() {
        assert_eq!(english_label(Labeled::Title("File")), Some("File".to_string()));
        assert_eq!(english_label(Labeled::Title("Community")), Some("Community".to_string()));
        assert_eq!(english_label(Labeled::Title("Nothing")), None);
        assert_eq!(english_label(Labeled::Command(&CommandRef::new("app.exit"))), Some("Exit".to_string()));
        assert_eq!(english_label(Labeled::Command(&CommandRef::new("profile.new"))), Some("New…".to_string()));
        assert_eq!(english_label(Labeled::Command(&CommandRef::with_params("profile.switch", json!({"name": "Main"})))), None);
        let ja = crate::i18n::I18N.from_code("ja").unwrap();
        crate::i18n::set_lang(ja);
        let shown = title("File").to_string();
        let exit = english_label(Labeled::Command(&CommandRef::new("app.exit")));
        crate::i18n::set_lang(craft_i18n::Lang::EN);
        assert_ne!(shown, "File", "the Japanese catalog translates File");
        assert_eq!(english_label(Labeled::Title(&shown)), None, "the title is looked up in the language it is shown in");
        crate::i18n::set_lang(ja);
        let found = english_label(Labeled::Title(&shown));
        crate::i18n::set_lang(craft_i18n::Lang::EN);
        assert_eq!(found, Some("File".to_string()));
        assert_eq!(exit, Some("Exit".to_string()), "the English labels stay English in every language");
        for (id, letter) in MNEMONICS {
            let label = english_label(Labeled::Command(&CommandRef::new(*id))).unwrap_or_default();
            assert!(label.to_lowercase().contains(letter.to_ascii_lowercase()), "{id}: `{label}` has no {letter}");
        }
    }
}
