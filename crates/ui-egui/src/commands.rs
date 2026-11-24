//! The window's own commands, the shortcut catalog and the menu bar.
//!
//! Engine commands (`scene.add`, `record.toggle`, …) run through the studio; the ids here are the
//! window's (`view.stats`, `profile.switch`, …). Menus, shortcuts and the control channel all name
//! a command by id with JSON parameters.

use craft_keymap::{Bindable, Binding, CommandRef, Keymap, Overrides, Platform, Scope, Scopes};
use craft_menu::{Item, ItemRole, Menu, MenuBar, MenuRole, Node};
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
    ("pause.settings", "Pause Screen Settings…"),
    ("stream.destinations", "List Stream Destinations"),
    ("stream.destination_add", "Add Stream Destination"),
    ("stream.destination_remove", "Remove Stream Destination"),
    ("stream.destination_set", "Change Stream Destination"),
    ("stream.destination_start", "Start Stream Destination"),
    ("stream.destination_stop", "Stop Stream Destination"),
    ("stream.reconnect_set", "Change Automatic Reconnect"),
    ("stream.diagnose", "Check Stream Destination Connection"),
    ("stream.check_service_set", "Change Connection Checks After a Failure"),
    ("stream.save_copy_set", "Change Saving a Copy of the Stream"),
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
    ("selection.fill", "Fill Screen"),
    ("selection.stretch", "Stretch to Screen"),
    ("selection.center", "Center to Screen"),
    ("selection.reset", "Reset Transform"),
    ("selection.rotate_cw", "Rotate 90° Clockwise"),
    ("selection.rotate_ccw", "Rotate 90° Counterclockwise"),
    ("app.about", "About StudioCraft"),
    ("app.licenses", "Licenses"),
    ("app.check_updates", "Check for Updates…"),
    ("help.discord", "Discord Server"),
    ("help.website", "Website"),
    ("help.github", "Source Code"),
    ("help.issues", "Report an Issue"),
];

/// Whether `id` is one of the window's commands.
pub fn is_ui_command(id: &str) -> bool {
    UI_COMMANDS.iter().any(|(c, _)| *c == id)
}

/// The parameters a window command takes, written as the engine's command list writes them.
pub fn ui_params(id: &str) -> &'static str {
    match id {
        "profile.switch" | "collection.switch" => "name: string",
        "collection.import" | "collection.export" => "path?: string",
        "scopes.kind" => "kind: string (waveform, parade, histogram, vectorscopeYuv or vectorscopeHls)",
        "stream.destination_add" => {
            "service?: string (a streaming service id, or empty for a custom server), name?: string, server?: string, enabled?: boolean, key?: string (profile, held or none), held_name?: string (with key held), video_encoder?: string (an encoder id, or empty for the stream's), video_bitrate_kbps?: number (100 to 200000, or 0 for the stream's)"
        }
        "stream.destination_set" => {
            "id: string, service?: string (a streaming service id, or empty for a custom server), name?: string, server?: string, enabled?: boolean, key?: string (profile, held or none), held_name?: string (with key held), video_encoder?: string (an encoder id, or empty for the stream's), video_bitrate_kbps?: number (100 to 200000, or 0 for the stream's)"
        }
        "stream.destination_remove" | "stream.destination_start" | "stream.destination_stop" | "stream.diagnose" => "id: string",
        "stream.check_service_set" => {
            "enabled: boolean (whether the check after a failure also reads the service's list of ingest servers and status summary and connects to two of its other ingest servers)"
        }
        "stream.save_copy_set" => "enabled: boolean (whether the next stream also writes what it sends to an MKV file in the recording folder)",
        "stream.reconnect_set" => {
            "enabled?: boolean, first_delay_s?: integer (1 to 60), max_delay_s?: integer (first_delay_s to 600), max_tries?: integer (1 to 100)"
        }
        "dock.move" => "dock: string (scenes, sources, mixer or app_audio), to: string (left or right)",
        "dock.float" => "dock: string (scenes, sources, mixer or app_audio), floating: boolean (true for a window of its own, false for the strip)",
        "dock.layout_save" => "name?: string (without it the window asks for one)",
        "dock.layout_apply" | "dock.layout_remove" => "name: string",
        "video.canvas_from_display" => {
            "display?: string (an id from video.displays; without it, the display the window is on). The canvas then follows the display's resolution and scaling"
        }
        _ => "",
    }
}

/// Whether a window command changes the scene being edited through a command undo takes back.
pub fn ui_undoable(id: &str) -> bool {
    matches!(
        id,
        "selection.remove"
            | "selection.fit"
            | "selection.fill"
            | "selection.stretch"
            | "selection.center"
            | "selection.reset"
            | "selection.rotate_cw"
            | "selection.rotate_ccw"
    )
}

fn bindable(cmd: CommandRef, label: &str, defaults: &[&str]) -> Bindable {
    defaults.iter().filter_map(|d| Binding::parse(d).ok()).fold(Bindable::new(cmd, label, Scope::app()), Bindable::with_default)
}

/// Every command a shortcut can be given, with the default shortcuts.
pub fn catalog() -> Vec<Bindable> {
    let c = CommandRef::new;
    let mut list = vec![
        bindable(c("edit.undo"), "Undo", &["Cmd+Z"]),
        bindable(c("edit.redo"), "Redo", &["Cmd+Y", "Cmd+Shift+Z"]),
        bindable(c("record.toggle"), "Start or Stop Recording", &[]),
        bindable(c("record.marker"), "Add Recording Marker", &[]),
        bindable(c("stream.toggle"), "Start or Stop Streaming", &[]),
        bindable(c("replay.toggle"), "Start or Stop Replay Buffer", &[]),
        bindable(c("replay.save"), "Save Replay", &[]),
        bindable(c("pause.toggle"), "Pause or Resume Program", &[]),
        bindable(CommandRef::with_params("pause.extend", json!({"seconds": 60})), "Add a Minute to the Pause", &[]),
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
    list.push(bindable(c("present.next"), "Present Next Staged Window", &[]));
    list.push(bindable(c("present.previous"), "Present Previous Staged Window", &[]));
    for i in 1..=studiocraft_model::staged::SLOTS {
        let cmd = CommandRef::with_params("present.window", json!({"index": i}));
        list.push(bindable(cmd, &format!("Present Staged Window {i}"), &[]));
    }
    list
}

pub fn keymap(overrides: Overrides) -> Keymap {
    Keymap::new(catalog(), Scopes::new(), Platform::current()).with_overrides(overrides)
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
            item("record.marker", "Add Recording Marker"),
            item("audio_tracks.marker", "Add Audio Tracks Marker"),
            item("audio_tracks.open_last", "Open Last Audio Tracks in Editor"),
            Node::Separator,
            item("pause.start", "Pause Program"),
            item("pause.stop", "Resume Program"),
            Node::Item(Item::new(
                CommandRef::with_params("pause.extend", json!({"seconds": 60})),
                crate::i18n::command_label("pause.extend", "Add a Minute to the Pause"),
            )),
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
            item("selection.fill", "Fill Screen"),
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
    let mut bar = MenuBar {
        menus: vec![file, edit, view, docks, Menu::new(title("Profile"), profile), Menu::new(title("Scene Collection"), collection), tools, help],
        ..MenuBar::default()
    };
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
    }

    #[test]
    fn staged_windows_can_be_given_shortcuts() {
        let list = catalog();
        let listed = |cmd: &CommandRef| list.iter().any(|b| &b.command == cmd);
        assert!(listed(&CommandRef::new("present.next")) && listed(&CommandRef::new("present.previous")));
        for i in 1..=studiocraft_model::staged::SLOTS {
            assert!(listed(&CommandRef::with_params("present.window", json!({"index": i}))), "slot {i}");
        }
        let km = keymap(Overrides::new());
        assert!(km.bindings(&CommandRef::new("present.next")).is_empty(), "no shortcut until one is set");
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

    #[test]
    fn the_docks_menu_names_the_output_bar_groups() {
        let bar = menu_bar(&MenuState { collections: &[], collection: "", profiles: &[], profile: "", layouts: &[] });
        let docks = bar.menus.iter().find(|m| m.title == "Docks").unwrap();
        let items: Vec<(&str, &str, Option<bool>)> = docks
            .children
            .iter()
            .filter_map(|n| if let Node::Item(i) = n { Some((i.command.id.as_str(), i.label.as_str(), i.kind.checked())) } else { None })
            .collect();
        assert_eq!(
            items,
            [
                ("dock.scenes", "Scenes", Some(false)),
                ("dock.sources", "Sources", Some(false)),
                ("dock.mixer", "Audio Mixer", Some(false)),
                ("dock.app_audio", "Application Audio", Some(false)),
                ("dock.controls", "Output Controls", Some(false)),
                ("dock.transitions", "Transition Controls", Some(false)),
                ("dock.layout_save", "Save Dock Layout…", None),
                ("dock.reset", "Reset Docks", None),
            ]
        );
        for (id, label, _) in &items {
            assert_eq!(UI_COMMANDS.iter().find(|(c, _)| c == id).map(|(_, l)| *l), Some(*label), "{id}");
        }
    }

    #[test]
    fn the_layout_submenus_list_the_saved_layouts() {
        let submenu = |layouts: &[String], name: &str| -> Vec<(String, serde_json::Value, bool)> {
            let bar = menu_bar(&MenuState { collections: &[], collection: "", profiles: &[], profile: "", layouts });
            let docks = bar.menus.into_iter().find(|m| m.title == "Docks").unwrap();
            let Some(Node::Submenu { children, .. }) = docks.children.into_iter().find(|n| matches!(n, Node::Submenu { label, .. } if label == name))
            else {
                panic!("no {name} submenu");
            };
            children.into_iter().filter_map(|n| if let Node::Item(i) = n { Some((i.label, i.command.params, i.enabled)) } else { None }).collect()
        };
        assert_eq!(submenu(&[], "Layouts"), [("None".to_string(), serde_json::Value::Null, false)]);
        assert_eq!(submenu(&[], "Remove Layout"), [("None".to_string(), serde_json::Value::Null, false)]);
        let names = ["Talk".to_string(), "Game".to_string()];
        assert_eq!(
            submenu(&names, "Layouts"),
            [("Talk".to_string(), json!({"name": "Talk"}), true), ("Game".to_string(), json!({"name": "Game"}), true)]
        );
        assert_eq!(submenu(&names, "Remove Layout").len(), 2);
    }

    #[test]
    fn help_community_holds_the_link_openers() {
        let bar = menu_bar(&MenuState { collections: &[], collection: "", profiles: &[], profile: "", layouts: &[] });
        let help = bar.menus.iter().find(|m| m.title == "Help").unwrap();
        let mut ids = Vec::new();
        for n in &help.children {
            if let Node::Submenu { label, children, .. } = n
                && label == "Community"
            {
                for c in children {
                    if let Node::Item(i) = c {
                        ids.push(i.command.id.clone());
                    }
                }
            }
        }
        assert_eq!(ids, ["help.discord", "help.website", "help.github", "help.issues"]);
        for (id, _, _) in COMMUNITY {
            assert!(is_ui_command(id), "{id}");
            assert!(community_link(id).is_some_and(|u| u.starts_with("https://")), "{id}");
        }
        assert_eq!(community_link("help.github"), Some(studiocraft_engine::links::GITHUB));
        assert_eq!(community_link("help.unknown"), None);
        assert!(!is_ui_command("app.links"), "app.links runs in the engine");
    }

    #[test]
    fn help_menu_checks_for_updates() {
        assert!(is_ui_command("app.check_updates"));
        assert_eq!(ui_params("app.check_updates"), "");
        assert!(!ui_undoable("app.check_updates"));
        let bar = menu_bar(&MenuState { collections: &[], collection: "", profiles: &[], profile: "", layouts: &[] });
        let help = bar.menus.iter().find(|m| m.title == "Help").unwrap();
        let ids: Vec<_> = help.children.iter().filter_map(|n| if let Node::Item(i) = n { Some(i.command.id.as_str()) } else { None }).collect();
        assert_eq!(ids, ["app.check_updates", "app.about"]);
    }
}
