use craft_keymap::CommandRef;
use serde_json::json;
use studiocraft_plugin::AddressOutcome;

use super::*;
use crate::store::Store;

fn app() -> StudioApp {
    StudioApp::boot(Store::memory(), None, Vec::new())
}

fn run(a: &mut StudioApp, ctx: &egui::Context, id: &str, params: Value) -> Result<Value, String> {
    a.perform(ctx, &CommandRef::with_params(id, params))
}

fn ids(a: &StudioApp) -> Vec<String> {
    a.studio.profile().stream.destinations.iter().map(|d| d.id.clone()).collect()
}

/// The texts `f` draws.
fn drawn(f: impl FnMut(&mut Ui)) -> Vec<String> {
    let ctx = egui::Context::default();
    let mut out = ctx.run_ui(egui::RawInput::default(), f);
    out.textures_delta.clear();
    out.shapes
        .iter()
        .filter_map(|s| match &s.shape {
            egui::Shape::Text(t) => Some(t.galley.text().to_string()),
            _ => None,
        })
        .collect()
}

fn state(id: &str, status: DestinationStatus) -> DestinationState {
    DestinationState { id: id.into(), target: String::new(), status, bytes: 0, dropped: 0, reconnects: 0 }
}

#[test]
fn the_window_opens_and_closes_from_the_view_menu() {
    let mut a = app();
    let ctx = egui::Context::default();
    assert_eq!(a.item_state("view.streaming").checked, Some(false));
    assert_eq!(run(&mut a, &ctx, "view.streaming", json!({})).unwrap(), json!({"on": true}));
    assert_eq!(a.hud()["ui"]["streaming_open"], json!(true));
    assert_eq!(a.item_state("view.streaming").checked, Some(true));
    run(&mut a, &ctx, "stream.destination_add", json!({"server": "rtmp://a.example/live/secret", "key": "none"})).unwrap();
    let raw = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 800.0))), ..Default::default() };
    let mut out = ctx.run_ui(raw, |ui| a.frame(ui));
    out.textures_delta.clear();
    assert_eq!(run(&mut a, &ctx, "view.streaming", json!({})).unwrap(), json!({"on": false}));
    assert_eq!(a.hud()["ui"]["streaming_open"], json!(false));
}

#[test]
fn destinations_are_added_up_to_the_limit() {
    let mut a = app();
    let ctx = egui::Context::default();
    assert!(ids(&a).is_empty());
    // The first one is the profile's main destination with the profile's key.
    assert_eq!(run(&mut a, &ctx, "stream.destination_add", json!({})).unwrap(), json!({"id": "main"}));
    assert_eq!(a.studio.profile().stream.destinations[0].key, KeyRef::Profile);
    // Later ones have no key of their own, so the profile's key is not sent to another service.
    let r = run(&mut a, &ctx, "stream.destination_add", json!({"name": " Backup ", "server": "rtmp://b.example/app", "enabled": false})).unwrap();
    assert_eq!(r, json!({"id": "destination-2"}));
    let d = a.studio.profile().stream.destination("destination-2").cloned().unwrap();
    assert_eq!(
        d,
        Destination {
            id: "destination-2".into(),
            name: "Backup".into(),
            enabled: false,
            server: "rtmp://b.example/app".into(),
            key: KeyRef::None,
            service: String::new()
        }
    );
    a.act(&ctx, Act::run("stream.destination_add", json!({"key": "held", "held_name": "night"})));
    assert_eq!(a.studio.profile().stream.destination("destination-3").map(|d| d.key.clone()), Some(KeyRef::Held("night".into())));
    while ids(&a).len() < MAX_DESTINATIONS {
        run(&mut a, &ctx, "stream.destination_add", json!({})).unwrap();
    }
    let e = run(&mut a, &ctx, "stream.destination_add", json!({})).unwrap_err();
    assert!(e.contains("at most 16 stream destinations"), "{e}");
    assert_eq!(ids(&a).len(), MAX_DESTINATIONS);
    let mut unique = ids(&a);
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), MAX_DESTINATIONS, "every id is different");
    // An id freed by a removal is used again.
    run(&mut a, &ctx, "stream.destination_remove", json!({"id": "destination-5"})).unwrap();
    assert_eq!(run(&mut a, &ctx, "stream.destination_add", json!({})).unwrap(), json!({"id": "destination-5"}));
}

#[test]
fn bad_parameters_are_refused_and_change_nothing() {
    let mut a = app();
    let ctx = egui::Context::default();
    run(&mut a, &ctx, "stream.destination_add", json!({})).unwrap();
    let before = a.studio.profile().clone();
    let long_name = "n".repeat(MAX_NAME + 1);
    let long_server = format!("rtmp://h/{}", "s".repeat(MAX_SERVER));
    let cases = [
        ("stream.destination_add", json!({"enabled": "yes"}), "`enabled` is true or false"),
        ("stream.destination_add", json!({"name": 3}), "`name` is a string"),
        ("stream.destination_add", json!({"name": long_name}), "`name` is longer than 128 characters"),
        ("stream.destination_add", json!({"server": long_server}), "`server` is longer than 4096 bytes"),
        ("stream.destination_add", json!({"server": "rtmp://h/app\nx"}), "`server` has a control character"),
        ("stream.destination_add", json!({"server": "rtmp://h/app\n"}), "`server` has a control character"),
        ("stream.destination_add", json!({"server": "rtmp://user:pass@h/app"}), "`server` has a user name or password"),
        ("stream.destination_add", json!({"server": "rtmps://p@ss@h:443/app/key"}), "`server` has a user name or password"),
        ("stream.destination_set", json!({"id": "main", "server": "rtmp://u@h"}), "`server` has a user name or password"),
        ("stream.destination_add", json!({"name": "\tBackup"}), "`name` has a control character"),
        ("stream.destination_add", json!({"key": "secret"}), "`key` is `profile`, `held` or `none`"),
        ("stream.destination_add", json!({"held_name": "x"}), "`held_name` goes with `key`: `held`"),
        ("stream.destination_add", json!({"key": "none", "held_name": "x"}), "`held_name` goes with `key`: `held`"),
        ("stream.destination_add", json!({"key": "held", "held_name": " "}), "`held_name` is 1 to 64 bytes"),
        ("stream.destination_set", json!({}), "needs `id`"),
        ("stream.destination_set", json!({"id": "nope", "enabled": false}), "there is no stream destination `nope`"),
        ("stream.destination_set", json!({"id": "main", "enabled": 1}), "`enabled` is true or false"),
        ("stream.destination_remove", json!({"id": "nope"}), "there is no stream destination `nope`"),
        ("stream.destination_remove", json!({"id": 4}), "needs `id`"),
    ];
    for (id, params, want) in cases {
        let e = run(&mut a, &ctx, id, params.clone()).unwrap_err();
        assert!(e.contains(want), "{id} {params}: {e}");
        assert_eq!(*a.studio.profile(), before, "{id} {params}");
    }
}

#[test]
fn a_destination_is_changed_and_an_unchanged_one_is_not_saved_again() {
    let mut a = app();
    let ctx = egui::Context::default();
    run(&mut a, &ctx, "stream.destination_add", json!({})).unwrap();
    let r = run(&mut a, &ctx, "stream.destination_set", json!({"id": "main", "name": "Main", "key": "held"}));
    assert_eq!(r.unwrap(), json!({"id": "main", "changed": true}));
    a.act(&ctx, Act::run("stream.destination_set", json!({"id": "main", "server": "rtmp://m.example/live"})));
    let d = a.studio.profile().stream.destination("main").cloned().unwrap();
    assert_eq!((d.name.as_str(), d.server.as_str(), d.key), ("Main", "rtmp://m.example/live", KeyRef::Held("main".into())));
    // A held key keeps its name when `held_name` is left out, and takes a new one when it is given.
    a.act(&ctx, Act::run("stream.destination_set", json!({"id": "main", "key": "held", "held_name": "evening"})));
    run(&mut a, &ctx, "stream.destination_set", json!({"id": "main", "key": "held"})).unwrap();
    run(&mut a, &ctx, "stream.destination_set", json!({"id": "main", "key": "held", "held_name": "evening"})).unwrap();
    assert_eq!(a.studio.profile().stream.destination("main").map(|d| d.key.clone()), Some(KeyRef::Held("evening".into())));
    let r = run(&mut a, &ctx, "stream.destination_set", json!({"id": "main", "name": "Main", "enabled": true}));
    assert_eq!(r.unwrap(), json!({"id": "main", "changed": false}));
    run(&mut a, &ctx, "stream.destination_set", json!({"id": "main", "enabled": false, "key": "none"})).unwrap();
    let d = a.studio.profile().stream.destination("main").cloned().unwrap();
    assert!(!d.enabled && d.key == KeyRef::None);
}

#[test]
fn reconnect_settings_are_changed_listed_and_checked() {
    let mut a = app();
    let ctx = egui::Context::default();
    let defaults = json!({"enabled": true, "first_delay_s": 2, "max_delay_s": 30, "max_tries": 10});
    let listed = run(&mut a, &ctx, "stream.destinations", json!({})).unwrap();
    assert_eq!(listed["reconnect"], defaults);
    assert!(listed.get("running_reconnect").is_none(), "no stream runs");
    run(&mut a, &ctx, "app.settings", json!({})).unwrap();
    let r = run(&mut a, &ctx, "stream.reconnect_set", json!({"first_delay_s": 5, "max_delay_s": 20, "max_tries": 4})).unwrap();
    let want = json!({"enabled": true, "first_delay_s": 5, "max_delay_s": 20, "max_tries": 4});
    assert_eq!(r, json!({"changed": true, "reconnect": want}));
    assert_eq!(run(&mut a, &ctx, "stream.destinations", json!({})).unwrap()["reconnect"], want);
    assert_eq!(a.settings.as_ref().map(|w| w.draft.stream.reconnect), Some(a.studio.profile().stream.reconnect), "an open Settings window follows");
    // An unsaved edit in Settings stays when the command changes another value.
    if let Some(w) = &mut a.settings {
        w.draft.stream.reconnect.first_delay_s = 7;
    }
    run(&mut a, &ctx, "stream.reconnect_set", json!({"max_tries": 6})).unwrap();
    let draft = a.settings.as_ref().map(|w| (w.draft.stream.reconnect.first_delay_s, w.draft.stream.reconnect.max_tries));
    assert_eq!(draft, Some((7, 6)));
    assert_eq!(a.studio.profile().stream.reconnect.first_delay_s, 5, "the unsaved edit is not saved");
    // A draft the change would put out of range takes the saved settings.
    if let Some(w) = &mut a.settings {
        w.draft.stream.reconnect.first_delay_s = 40;
    }
    run(&mut a, &ctx, "stream.reconnect_set", json!({"max_delay_s": 25})).unwrap();
    assert_eq!(a.settings.as_ref().map(|w| w.draft.stream.reconnect), Some(a.studio.profile().stream.reconnect));
    run(&mut a, &ctx, "stream.reconnect_set", json!({"enabled": false})).unwrap();
    assert!(!a.studio.profile().stream.reconnect.enabled);
    assert_eq!(a.studio.profile().stream.reconnect.max_tries, 6, "values left out are kept");
    for bad in [
        json!({"first_delay_s": 0}),
        json!({"first_delay_s": 61}),
        json!({"max_delay_s": 4}),
        json!({"max_delay_s": 601}),
        json!({"max_tries": 0}),
        json!({"max_tries": 101}),
        json!({"max_tries": -1}),
        json!({"max_tries": 2.5}),
        json!({"max_tries": "4"}),
        json!({"enabled": "no"}),
        json!({"first_delay_s": 30, "max_tries": 0}),
    ] {
        let before = a.studio.profile().stream.reconnect;
        assert!(run(&mut a, &ctx, "stream.reconnect_set", bad.clone()).is_err(), "{bad}");
        assert_eq!(a.studio.profile().stream.reconnect, before, "{bad} changed nothing");
    }
    let e = run(&mut a, &ctx, "stream.reconnect_set", json!({"max_tries": 101})).unwrap_err();
    assert!(e.contains("1 to 100"), "{e}");
    assert!(crate::commands::ui_params("stream.reconnect_set").contains("max_tries?"));
}

#[test]
fn an_unchanged_set_does_not_save() {
    let dir = std::env::temp_dir().join(format!("studiocraft-ui-reconnect-save-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let (store, _) = Store::open(craft_settings::Location::Dir(dir.clone()));
    let mut a = StudioApp::boot(store, None, Vec::new());
    let ctx = egui::Context::default();
    // A file where the profiles folder goes makes any save fail.
    let profiles = dir.join("profiles");
    let _ = std::fs::remove_dir_all(&profiles);
    std::fs::write(&profiles, b"").unwrap();
    let same = run(&mut a, &ctx, "stream.reconnect_set", json!({"enabled": true, "max_tries": 10}));
    let empty = run(&mut a, &ctx, "stream.reconnect_set", json!({}));
    let changed = run(&mut a, &ctx, "stream.reconnect_set", json!({"max_tries": 3}));
    let kept = a.studio.profile().stream.reconnect.max_tries;
    let _ = std::fs::remove_file(&profiles);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(same.map(|v| v["changed"].clone()), Ok(json!(false)), "nothing to save, so the failing save is not tried");
    assert_eq!(empty.map(|v| v["changed"].clone()), Ok(json!(false)));
    assert!(changed.unwrap_err().contains("could not be saved"));
    assert_eq!(kept, 10, "a change that could not be saved is put back");
}

#[test]
fn changes_reach_an_open_settings_window() {
    let mut a = app();
    let ctx = egui::Context::default();
    run(&mut a, &ctx, "app.settings", json!({})).unwrap();
    run(&mut a, &ctx, "stream.destination_add", json!({"server": "rtmp://a.example/live", "key": "none"})).unwrap();
    let draft = a.settings.as_ref().map(|w| w.draft.stream.destinations.clone()).unwrap();
    assert_eq!(draft, a.studio.profile().stream.destinations);
    // An unsaved edit in Settings stays when another destination changes.
    if let Some(w) = &mut a.settings
        && let Some(d) = w.draft.stream.destinations.first_mut()
    {
        d.server = "rtmp://typed.example/live".into();
    }
    run(&mut a, &ctx, "stream.destination_add", json!({"name": "Backup"})).unwrap();
    run(&mut a, &ctx, "stream.destination_set", json!({"id": "destination-2", "server": "rtmp://b.example/live"})).unwrap();
    let draft = a.settings.as_ref().map(|w| w.draft.stream.destinations.clone()).unwrap();
    assert_eq!(draft.len(), 2);
    assert_eq!(draft[0].server, "rtmp://typed.example/live");
    assert_eq!(a.studio.profile().stream.destinations[0].server, "rtmp://a.example/live");
    assert_eq!((draft[1].name.as_str(), draft[1].server.as_str()), ("Backup", "rtmp://b.example/live"));
    run(&mut a, &ctx, "stream.destination_remove", json!({"id": "destination-2"})).unwrap();
    let draft = a.settings.as_ref().map(|w| w.draft.stream.destinations.clone()).unwrap();
    assert_eq!(draft.iter().map(|d| d.server.as_str()).collect::<Vec<_>>(), ["rtmp://typed.example/live"]);
}

#[test]
fn destinations_start_and_stop_only_in_a_running_stream() {
    let mut a = app();
    let ctx = egui::Context::default();
    run(&mut a, &ctx, "stream.destination_add", json!({})).unwrap();
    for id in ["stream.destination_start", "stream.destination_stop"] {
        let e = run(&mut a, &ctx, id, json!({"id": "main"})).unwrap_err();
        assert!(e.contains("the stream is not running"), "{id}: {e}");
        assert!(run(&mut a, &ctx, id, json!({})).unwrap_err().contains("needs `id`"));
    }
}

#[test]
fn a_destination_is_removed_and_its_open_details_close() {
    let mut a = app();
    let ctx = egui::Context::default();
    run(&mut a, &ctx, "view.streaming", json!({})).unwrap();
    run(&mut a, &ctx, "stream.destination_add", json!({})).unwrap();
    // A destination just added opens its details.
    assert_eq!(a.streaming.as_ref().and_then(|p| p.editing.as_ref()).map(|e| e.id.as_str()), Some("main"));
    run(&mut a, &ctx, "stream.destination_add", json!({})).unwrap();
    run(&mut a, &ctx, "stream.destination_remove", json!({"id": "destination-2"})).unwrap();
    assert_eq!(ids(&a), ["main"]);
    a.streaming = Some(StreamingPanel {
        editing: a.studio.profile().stream.destination("main").map(Edit::of),
        profile: a.studio.profile().name.clone(),
        ..StreamingPanel::default()
    });
    assert_eq!(run(&mut a, &ctx, "stream.destination_remove", json!({"id": "main"})).unwrap(), json!({"removed": "main"}));
    assert!(ids(&a).is_empty());
    assert!(a.streaming.as_ref().is_some_and(|p| p.editing.is_none()));
}

#[test]
fn the_list_hides_keys_and_says_which_held_keys_are_set() {
    let mut a = app();
    let ctx = egui::Context::default();
    // The window's own action gives the first destination the profile's key with a server.
    a.act(&ctx, Act::run("stream.destination_add", json!({"server": "rtmp://a.example/live"})));
    run(&mut a, &ctx, "stream.destination_add", json!({"server": "rtmp://b.example/live/secret-key?auth=token"})).unwrap();
    a.act(&ctx, Act::run("stream.destination_add", json!({"server": "rtmp://c.example/live/room", "key": "held"})));
    let list = run(&mut a, &ctx, "stream.destinations", json!({})).unwrap();
    assert_eq!(list["running"], json!(false));
    assert_eq!(list["max"], json!(16));
    let d = list["destinations"].as_array().cloned().unwrap();
    assert_eq!(d.len(), 3);
    assert_eq!(d[0]["key"], "profile");
    // The profile's key is empty, so the address could end with a key.
    assert_eq!(d[0]["server"], "rtmp://a.example/live");
    assert_eq!(d[1]["server"], "rtmp://b.example/live/•••?•••");
    assert!(!list.to_string().contains("secret-key") && !list.to_string().contains("token"), "{list}");
    // The end of the address is hidden whichever key source the destination uses.
    assert_eq!(
        (d[2]["server"].as_str(), d[2]["held_name"].as_str(), d[2]["held"].as_bool()),
        (Some("rtmp://c.example/live/•••"), Some("destination-3"), Some(false))
    );
    assert!(!list.to_string().contains("room"), "{list}");
    assert!(d.iter().all(|d| d["status"] == "idle"));
    a.act(&ctx, Act::HoldStreamKey { name: "destination-3".into(), key: "k".into() });
    assert!(a.studio.stream_key_held("destination-3"));
    let list = run(&mut a, &ctx, "stream.destinations", json!({})).unwrap();
    assert_eq!(list["destinations"][2]["held"], json!(true));
    a.act(&ctx, Act::HoldStreamKey { name: "destination-3".into(), key: String::new() });
    assert!(!a.studio.stream_key_held("destination-3"), "an empty key forgets it");
}

#[test]
fn a_change_that_cant_be_saved_is_put_back_and_can_be_made_again() {
    let dir = std::env::temp_dir().join(format!("studiocraft-ui-streaming-save-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let (store, _) = Store::open(craft_settings::Location::Dir(dir.clone()));
    let mut a = StudioApp::boot(store, None, Vec::new());
    let ctx = egui::Context::default();
    // A file where the profiles folder goes makes the save fail.
    let profiles = dir.join("profiles");
    let _ = std::fs::remove_dir_all(&profiles);
    std::fs::write(&profiles, b"").unwrap();
    let r = run(&mut a, &ctx, "stream.destination_add", json!({"server": "rtmp://a.example/live", "key": "none"}));
    let after_failure = ids(&a);
    std::fs::remove_file(&profiles).unwrap();
    let again = run(&mut a, &ctx, "stream.destination_add", json!({"server": "rtmp://a.example/live", "key": "none"}));
    let saved = a.store.load_profile(&a.studio.profile().name).map(|p| p.stream.destinations.len());
    let _ = std::fs::remove_dir_all(&dir);
    assert!(r.unwrap_err().contains("could not be saved"));
    assert!(after_failure.is_empty(), "the change was put back: {after_failure:?}");
    assert_eq!(again.unwrap(), json!({"id": "main"}));
    assert_eq!(saved, Ok(1));
}

#[test]
fn a_new_destination_does_not_take_an_id_the_running_stream_uses() {
    let state =
        |id: &str| DestinationState { id: id.into(), target: String::new(), status: DestinationStatus::Stopped, bytes: 0, dropped: 0, reconnects: 0 };
    let empty = StreamSettings { destinations: Vec::new(), ..StreamSettings::default() };
    assert_eq!(new_id(&empty, &[]), "main");
    assert_eq!(new_id(&empty, &[state("main")]), "destination-2", "`main` was removed while the stream runs");
    let one = StreamSettings { destinations: vec![Destination { id: "main".into(), ..Destination::default() }], ..StreamSettings::default() };
    assert_eq!(new_id(&one, &[state("main"), state("destination-2")]), "destination-3");
}

#[test]
fn a_new_destination_finds_a_free_id_while_the_running_stream_keeps_many_removed_ones() {
    let state =
        |id: &str| DestinationState { id: id.into(), target: String::new(), status: DestinationStatus::Stopped, bytes: 0, dropped: 0, reconnects: 0 };
    // The stream keeps up to 64 destinations, removed ones included.
    let live: Vec<_> = (2..=65).map(|n| state(&format!("destination-{n}"))).collect();
    let dest = |id: &str| Destination { id: id.into(), ..Destination::default() };
    let s = StreamSettings { destinations: vec![dest("main"), dest("destination"), dest("destination-66")], ..StreamSettings::default() };
    let id = new_id(&s, &live);
    assert!(s.destination(&id).is_none() && !live.iter().any(|d| d.id == id), "`{id}` is in use");
    assert_eq!(id, "destination-67");
}

#[test]
fn a_change_reaches_only_its_own_destination_in_an_open_settings_draft() {
    let mut a = app();
    let ctx = egui::Context::default();
    run(&mut a, &ctx, "app.settings", json!({})).unwrap();
    // The Settings draft already holds an unsaved first destination.
    if let Some(w) = &mut a.settings {
        w.draft.stream.destinations = vec![Destination { id: "main".into(), server: "rtmp://typed.example/live".into(), ..Destination::default() }];
    }
    run(&mut a, &ctx, "stream.destination_add", json!({"server": "rtmp://a.example/live", "key": "none"})).unwrap();
    let draft = a.settings.as_ref().map(|w| w.draft.stream.destinations.clone()).unwrap();
    assert_eq!(draft, a.studio.profile().stream.destinations, "the added destination replaces the draft's one with its id");
}

#[test]
fn switching_profiles_closes_open_details() {
    let mut a = app();
    let ctx = egui::Context::default();
    run(&mut a, &ctx, "view.streaming", json!({})).unwrap();
    run(&mut a, &ctx, "stream.destination_add", json!({"server": "rtmp://a.example/live", "key": "none"})).unwrap();
    assert!(a.streaming.as_ref().is_some_and(|p| p.editing.is_some()));
    a.named(crate::Named::DuplicateProfile, "Other").unwrap();
    run(&mut a, &ctx, "stream.destinations", json!({})).unwrap();
    assert!(a.streaming.as_ref().is_some_and(|p| p.editing.is_none()), "the details belonged to the other profile");
    run(&mut a, &ctx, "stream.destination_add", json!({})).unwrap();
    assert!(a.streaming.as_ref().is_some_and(|p| p.editing.is_some() && p.profile == "Other"));
    // Renaming the profile keeps its open details.
    a.named(crate::Named::RenameProfile, "Renamed").unwrap();
    run(&mut a, &ctx, "stream.destinations", json!({})).unwrap();
    assert!(a.streaming.as_ref().is_some_and(|p| p.editing.is_some() && p.profile == "Renamed"));
    // A refused Save keeps the details open with what was typed; one that is made closes them.
    assert!(run(&mut a, &ctx, "stream.destination_set", json!({"id": "destination-2", "server": "rtmp://h/app\n"})).is_err());
    assert!(a.streaming.as_ref().is_some_and(|p| p.editing.as_ref().is_some_and(|e| e.id == "destination-2")));
    run(&mut a, &ctx, "stream.destination_set", json!({"id": "destination-2", "server": "rtmp://h/app"})).unwrap();
    assert!(a.streaming.as_ref().is_some_and(|p| p.editing.is_none()));
    // A stream that does not run belongs to no profile.
    assert_eq!(a.studio.stream_profile(), None);
}

#[test]
fn a_held_key_is_described_as_never_saved() {
    let held = panel_key_source(&KeyRef::Held("night".into()));
    assert!(held.contains("night") && held.contains("never saved"), "{held}");
    assert!(!held.contains("saved in this profile"), "{held}");
    assert_eq!(panel_key_source(&KeyRef::Profile), crate::settings::key_source(&KeyRef::Profile));
}

#[test]
fn servers_are_shown_without_their_key() {
    assert_eq!(shown_server("rtmp://h/app/key"), "rtmp://h/app/•••");
    assert_eq!(shown_server(" rtmp://h/app/key/ "), "rtmp://h/app/•••");
    assert_eq!(shown_server("rtmp://h/app"), "rtmp://h/app", "no key in the address");
    assert_eq!(shown_server("rtmp://h"), "rtmp://h");
    assert_eq!(shown_server("rtmps://h/app?auth=x"), "rtmps://h/app?•••");
    assert_eq!(shown_server(""), "");
    assert_eq!(shown_server("not a url/with/slashes"), "not a url/with/slashes");
    // A user name and password before the host are hidden up to the authority's last `@`.
    assert_eq!(shown_server("rtmp://user:pass@h/app"), "rtmp://•••@h/app");
    assert_eq!(shown_server("rtmps://u:p@ss@h:443/app/key"), "rtmps://•••@h:443/app/•••");
    assert_eq!(shown_server("rtmp://u:pa?ss@h"), "rtmp://•••@h");
    assert_eq!(shown_server("rtmp://u:pass@h?auth=x"), "rtmp://•••@h?•••");
    assert_eq!(shown_server("rtmp://h/app/a@b"), "rtmp://h/app/•••", "an `@` after the host is not a user name");
}

#[test]
fn commands_do_not_send_the_profile_key_to_a_new_server() {
    let mut a = app();
    let ctx = egui::Context::default();
    let refused = |a: &mut StudioApp, params: Value, want: &str| {
        let before = a.studio.profile().clone();
        let id = if params.get("id").is_some() { "stream.destination_set" } else { "stream.destination_add" };
        let e = run(a, &ctx, id, params.clone()).unwrap_err();
        assert!(e.contains(want), "{params}: {e}");
        assert_eq!(*a.studio.profile(), before, "{params}");
    };
    let new_server = "would send the profile's stream key to a new server";
    let only_main = "only the destination `main` takes `key`: `profile`";
    // A first destination added by a command with a server keeps the profile's key only from the window.
    refused(&mut a, json!({"server": "rtmp://other.example/live"}), new_server);
    refused(&mut a, json!({"server": "rtmp://other.example/live", "key": "profile"}), new_server);
    run(&mut a, &ctx, "stream.destination_add", json!({})).unwrap();
    assert_eq!(a.studio.profile().stream.destinations[0].key, KeyRef::Profile);
    refused(&mut a, json!({"id": "main", "server": "rtmp://other.example/live"}), new_server);
    refused(&mut a, json!({"id": "main", "server": " rtmp://other.example/live ", "key": "profile"}), new_server);
    // Other destinations never take the profile's key from a command.
    refused(&mut a, json!({"key": "profile"}), only_main);
    run(&mut a, &ctx, "stream.destination_add", json!({"server": "rtmp://b.example/live"})).unwrap();
    refused(&mut a, json!({"id": "destination-2", "key": "profile"}), only_main);
    // The primary destination's server changes with another key, and the profile's key does not come back to it.
    run(&mut a, &ctx, "stream.destination_set", json!({"id": "main", "server": "rtmp://m.example/live", "key": "none"})).unwrap();
    refused(&mut a, json!({"id": "main", "key": "profile"}), new_server);
    // Changes that leave the profile's key with the server it already goes to are made.
    a.act(&ctx, Act::run("stream.destination_set", json!({"id": "main", "key": "profile"})));
    assert_eq!(a.studio.profile().stream.destinations[0].key, KeyRef::Profile, "the window sets the key");
    let r = run(&mut a, &ctx, "stream.destination_set", json!({"id": "main", "server": "rtmp://m.example/live", "key": "profile", "name": "Main"}));
    assert_eq!(r.unwrap(), json!({"id": "main", "changed": true}));
    run(&mut a, &ctx, "stream.destination_set", json!({"id": "main", "server": ""})).unwrap();
    refused(&mut a, json!({"id": "main", "server": "rtmp://m.example/live"}), new_server);
    run(&mut a, &ctx, "stream.destination_set", json!({"id": "main", "server": "rtmp://m.example/live", "key": "none"})).unwrap();
    // The window's own Save sends the profile's key wherever the user typed.
    a.act(&ctx, Act::run("stream.destination_set", json!({"id": "destination-2", "server": "rtmp://b2.example/live", "key": "profile"})));
    let d = a.studio.profile().stream.destination("destination-2").cloned().unwrap();
    assert_eq!((d.server.as_str(), d.key), ("rtmp://b2.example/live", KeyRef::Profile));
    // A command after the window's action is a command again.
    refused(&mut a, json!({"id": "destination-2", "server": "rtmp://other.example/live"}), new_server);
}

#[test]
fn commands_do_not_send_a_held_key_to_a_new_server() {
    let mut a = app();
    let ctx = egui::Context::default();
    let refused = |a: &mut StudioApp, params: Value, want: &str| {
        let before = a.studio.profile().clone();
        let id = if params.get("id").is_some() { "stream.destination_set" } else { "stream.destination_add" };
        let e = run(a, &ctx, id, params.clone()).err().unwrap_or_else(|| panic!("{params} was made"));
        assert!(e.contains(want), "{params}: {e}");
        assert_eq!(*a.studio.profile(), before, "{params}");
    };
    let new_server = "would send its session key to a new server";
    let not_used = "does not use the session key `night`";
    let key = |a: &StudioApp, id: &str| a.studio.profile().stream.destination(id).map(|d| (d.server.clone(), d.key.clone()));
    let night = || ("rtmp://n.example/live".to_string(), KeyRef::Held("night".into()));
    a.act(&ctx, Act::run("stream.destination_add", json!({})));
    a.act(&ctx, Act::run("stream.destination_add", json!({"server": "rtmp://n.example/live", "key": "held", "held_name": "night"})));
    assert_eq!(key(&a, "destination-2"), Some(night()), "the window adds a held key with a server");
    // A new destination takes neither a held key with a server nor another destination's held key.
    refused(&mut a, json!({"server": "rtmp://other.example/live", "key": "held"}), new_server);
    refused(&mut a, json!({"key": "held", "held_name": "night"}), not_used);
    refused(&mut a, json!({"server": "rtmp://n.example/live", "key": "held", "held_name": "night"}), not_used);
    // A destination's held key does not go to a new server, and the destination does not take another held key.
    refused(&mut a, json!({"id": "destination-2", "server": "rtmp://other.example/live"}), new_server);
    refused(&mut a, json!({"id": "destination-2", "server": " rtmp://other.example/live ", "key": "held"}), new_server);
    refused(&mut a, json!({"id": "main", "key": "held", "held_name": "night"}), not_used);
    let r = run(
        &mut a,
        &ctx,
        "stream.destination_set",
        json!({"id": "destination-2", "server": " rtmp://n.example/live", "key": "held", "held_name": "night"}),
    );
    assert_eq!(r.unwrap(), json!({"id": "destination-2", "changed": false}), "the name the destination already uses is taken");
    run(&mut a, &ctx, "stream.destination_set", json!({"id": "destination-2", "name": "Night"})).unwrap();
    // An empty server and then a new one is refused.
    run(&mut a, &ctx, "stream.destination_set", json!({"id": "destination-2", "server": ""})).unwrap();
    refused(&mut a, json!({"id": "destination-2", "server": "rtmp://other.example/live"}), new_server);
    // No key and then the held key again is refused.
    a.act(&ctx, Act::run("stream.destination_set", json!({"id": "destination-2", "server": "rtmp://n.example/live"})));
    assert_eq!(key(&a, "destination-2"), Some(night()));
    run(&mut a, &ctx, "stream.destination_set", json!({"id": "destination-2", "key": "none"})).unwrap();
    refused(&mut a, json!({"id": "destination-2", "key": "held"}), new_server);
    refused(&mut a, json!({"id": "destination-2", "key": "held", "held_name": "night"}), not_used);
    // A removal and then an add with the same id is refused the held key with a server.
    run(&mut a, &ctx, "stream.destination_remove", json!({"id": "destination-2"})).unwrap();
    refused(&mut a, json!({"server": "rtmp://n.example/live", "key": "held"}), new_server);
    refused(&mut a, json!({"key": "held", "held_name": "night"}), not_used);
    assert_eq!(run(&mut a, &ctx, "stream.destination_add", json!({"key": "held"})).unwrap(), json!({"id": "destination-2"}));
    refused(&mut a, json!({"id": "destination-2", "server": "rtmp://n.example/live"}), new_server);
    // The window's own Save sends the held key wherever the user typed, and a command after it is a command again.
    a.act(
        &ctx,
        Act::run("stream.destination_set", json!({"id": "destination-2", "server": "rtmp://n.example/live", "key": "held", "held_name": "night"})),
    );
    assert_eq!(key(&a, "destination-2"), Some(night()));
    refused(&mut a, json!({"id": "destination-2", "server": "rtmp://other.example/live"}), new_server);
}

#[test]
fn each_destination_shows_its_own_status() {
    let stream = StreamSettings {
        destinations: vec![
            Destination { id: "a".into(), name: "First".into(), server: "rtmp://a.example/live/hidden-a".into(), ..Destination::default() },
            Destination {
                id: "b".into(),
                name: "Second".into(),
                server: "rtmp://b.example/live".into(),
                key: KeyRef::Held("b".into()),
                ..Destination::default()
            },
            Destination { id: "c".into(), server: "rtmp://c.example/live".into(), enabled: false, ..Destination::default() },
            Destination { id: "d".into(), server: "rtmp://d.example/live".into(), ..Destination::default() },
        ],
        ..StreamSettings::default()
    };
    let live = [
        state("a", DestinationStatus::Failed(Failure::new(Cause::RejectedPublish, "the server refused publish: NetStream.Publish.BadName"))),
        state("b", DestinationStatus::Live),
        state("c", DestinationStatus::Retrying { attempt: 3, failure: Failure::new(Cause::Lost, "the server closed the connection") }),
    ];
    let held = |_: &str| false;
    let view = View { stream: &stream, running: true, own: true, live: &live, held: &held, checks: &[], ingests: &[] };
    let mut panel = StreamingPanel::default();
    let mut acts = Vec::new();
    let texts = drawn(|ui| show(ui, &view, &mut panel, &mut acts));
    for want in [
        "First",
        "Second",
        "c",
        "Failed: the server refused the stream; check the stream key",
        "Live",
        "Trying again (try 3): the connection was lost",
        "Not in this stream yet; Start sends to it now",
        "A destination that is sending keeps its server and key until it is stopped and started again.",
        "rtmp://a.example/live/•••",
        "No key is held for this session yet",
        "4 of 16 destinations",
    ] {
        assert!(texts.iter().any(|t| t == want), "{want} in {texts:?}");
    }
    assert!(!texts.iter().any(|t| t.contains("hidden-a")), "{texts:?}");
    // A failed destination and one added after the stream started offer Start, and the others
    // that are sending offer Stop.
    assert_eq!(texts.iter().filter(|t| *t == "Start").count(), 2, "{texts:?}");
    assert_eq!(texts.iter().filter(|t| *t == "Stop").count(), 2, "{texts:?}");
    assert!(acts.is_empty(), "drawing does nothing");
    // While a stream of another profile runs, nothing here starts or stops it.
    let view = View { stream: &stream, running: true, own: false, live: &[], held: &held, checks: &[], ingests: &[] };
    let texts = drawn(|ui| show(ui, &view, &mut panel, &mut acts));
    assert_eq!(texts.iter().filter(|t| *t == "Not in this stream; it is sent to from the next start").count(), 4, "{texts:?}");
    assert!(texts.iter().any(|t| t == "Server and key changes are used from the next start of the stream."), "{texts:?}");
    assert!(!texts.iter().any(|t| t == "Start" || t == "Stop"), "{texts:?}");
    // Without a stream each destination says whether starting the stream sends to it.
    let view = View { stream: &stream, running: false, own: false, live: &[], held: &held, checks: &[], ingests: &[] };
    let texts = drawn(|ui| show(ui, &view, &mut panel, &mut acts));
    assert_eq!(texts.iter().filter(|t| *t == "Sent to when the stream starts").count(), 3, "{texts:?}");
    assert_eq!(texts.iter().filter(|t| *t == "Off").count(), 1, "{texts:?}");
    assert!(!texts.iter().any(|t| t == "Start" || t == "Stop"), "{texts:?}");
}

#[test]
fn a_failure_is_listed_with_its_cause_and_addresses() {
    let d = Destination { id: "a".into(), server: "rtmp://a.example/live".into(), ..Destination::default() };
    let failure = Failure {
        cause: Cause::TimedOut,
        detail: "could not connect to the server: timed out; 1 other address also failed".into(),
        addresses: vec![
            AddressOutcome { address: "[2001:db8::1]:1935".into(), cause: Cause::NoRoute, detail: "network unreachable".into() },
            AddressOutcome { address: "192.0.2.1:1935".into(), cause: Cause::TimedOut, detail: "timed out".into() },
        ],
    };
    let retrying = state("a", DestinationStatus::Retrying { attempt: 2, failure: failure.clone() });
    let v = listed(&d, true, Some(&retrying), false);
    assert_eq!(v["status"], json!("retrying"));
    assert_eq!(v["error"], json!(failure.detail), "the error keeps the connection's own text");
    assert_eq!(v["cause"], json!("timed_out"));
    assert_eq!(
        v["addresses"],
        json!([
            {"address": "[2001:db8::1]:1935", "cause": "no_route", "error": "network unreachable"},
            {"address": "192.0.2.1:1935", "cause": "timed_out", "error": "timed out"},
        ])
    );
    // The row names the cause, and its hover holds the error and each address.
    assert_eq!(status_text(&d, true, Some(&retrying)).0, "Trying again (try 2): the server did not answer in time");
    let detail = status_detail(true, Some(&retrying)).unwrap();
    assert_eq!(
        detail,
        "could not connect to the server: timed out; 1 other address also failed\n\
         [2001:db8::1]:1935: this computer has no network route to this address (network unreachable)\n\
         192.0.2.1:1935: the server did not answer in time (timed out)"
    );
    // A failure without addresses lists none, and a live destination has no cause.
    let failed = state("a", DestinationStatus::Failed(Failure::new(Cause::BadKey, "the stream key is not valid: empty")));
    let v = listed(&d, true, Some(&failed), false);
    assert_eq!((v["status"].clone(), v["cause"].clone()), (json!("failed"), json!("bad_key")));
    assert!(v.get("addresses").is_none(), "{v}");
    let live = state("a", DestinationStatus::Live);
    let v = listed(&d, true, Some(&live), false);
    assert!(v.get("cause").is_none() && v.get("error").is_none(), "{v}");
    assert!(status_detail(true, Some(&live)).is_none());
    // Every cause has its own text.
    let texts: std::collections::HashSet<&str> = Cause::ALL.iter().map(|c| cause_text(*c)).collect();
    assert_eq!(texts.len(), Cause::ALL.len());
}

#[test]
fn a_destination_waiting_for_the_network_says_so() {
    let d = Destination { id: "a".into(), server: "rtmp://a.example/live".into(), ..Destination::default() };
    let failure = Failure::new(Cause::LookupFailed, "the server's name did not resolve: no network");
    let waiting = state("a", DestinationStatus::WaitingForNetwork { attempt: 3, failure: failure.clone() });
    let v = listed(&d, true, Some(&waiting), false);
    assert_eq!(v["status"], json!("waiting_for_network"));
    assert_eq!((v["attempt"].clone(), v["cause"].clone()), (json!(3), json!("lookup_failed")));
    assert_eq!(v["error"], json!(failure.detail));
    assert_eq!(
        status_text(&d, true, Some(&waiting)),
        ("Waiting for the network (try 3): the server's name could not be looked up".into(), Tone::Warn)
    );
    assert_eq!(status_detail(true, Some(&waiting)).as_deref(), Some("the server's name did not resolve: no network"));
}

#[test]
fn open_details_save_what_was_typed() {
    let d = Destination {
        id: "b".into(),
        name: "B".into(),
        server: "rtmp://b.example/live".into(),
        key: KeyRef::Held("night".into()),
        ..Destination::default()
    };
    let mut e = Edit::of(&d);
    assert_eq!((e.key, e.held_name.as_str()), (KeyChoice::Held, "night"));
    e.server = "rtmp://b2.example/live".into();
    e.held_key = "typed".into();
    let p = e.params();
    assert_eq!(p, json!({"id": "b", "service": "", "name": "B", "server": "rtmp://b2.example/live", "key": "held", "held_name": "night"}));
    assert!(!p.to_string().contains("typed"), "a key typed for the session is not a command parameter");
    e.key = KeyChoice::Profile;
    assert_eq!(e.params(), json!({"id": "b", "service": "", "name": "B", "server": "rtmp://b2.example/live", "key": "profile"}));
    // The details of a destination are drawn with its fields, and the server field hides its text.
    let stream = StreamSettings { destinations: vec![d.clone()], ..StreamSettings::default() };
    let held = |n: &str| n == "night";
    let view = View { stream: &stream, running: false, own: false, live: &[], held: &held, checks: &[], ingests: &[] };
    let mut panel = StreamingPanel { editing: Some(Edit::of(&d)), ..StreamingPanel::default() };
    let mut acts = Vec::new();
    let texts = drawn(|ui| show(ui, &view, &mut panel, &mut acts));
    for want in ["Name", "Server", "Stream key", "Session key name", "Key for this session", "Hold", "Forget", "Save", "Cancel"] {
        assert!(texts.iter().any(|t| t == want), "{want} in {texts:?}");
    }
    // The list shows the address, and the field hides it until Show, whatever the key source.
    assert_eq!(texts.iter().filter(|t| *t == "rtmp://b.example/live").count(), 1, "{texts:?}");
    assert!(texts.iter().any(|t| t.chars().all(|c| c == '•') && !t.is_empty()), "{texts:?}");
    assert!(!texts.iter().any(|t| t == "No key is held for this session yet"), "{texts:?}");
    // A key at the end of the address is hidden in the list and in the field.
    let d = Destination { id: "c".into(), server: "rtmp://c.example/live/secret-c".into(), ..Destination::default() };
    let stream = StreamSettings { destinations: vec![d.clone()], ..StreamSettings::default() };
    let view = View { stream: &stream, running: false, own: false, live: &[], held: &held, checks: &[], ingests: &[] };
    let mut panel = StreamingPanel { editing: Some(Edit::of(&d)), ..StreamingPanel::default() };
    let texts = drawn(|ui| show(ui, &view, &mut panel, &mut acts));
    assert!(!texts.iter().any(|t| t.contains("secret-c")), "{texts:?}");
    assert!(texts.iter().any(|t| t == "rtmp://c.example/live/•••"), "{texts:?}");
    if let Some(e) = &mut panel.editing {
        e.show_server = true;
    }
    let texts = drawn(|ui| show(ui, &view, &mut panel, &mut acts));
    assert!(texts.iter().any(|t| t == "rtmp://c.example/live/secret-c"), "{texts:?}");
    assert!(acts.is_empty(), "drawing does nothing");
}

/// The preset of the service `id`.
fn preset(id: &str) -> &'static services::Service {
    services::service(id).unwrap_or_else(|| panic!("no {id} preset"))
}

#[test]
fn a_destination_added_for_a_service_takes_its_documented_server() {
    let mut a = app();
    let ctx = egui::Context::default();
    let twitch = preset("twitch");
    // The preset's server counts as a new server for the profile's key.
    let e = run(&mut a, &ctx, "stream.destination_add", json!({"service": "twitch"})).unwrap_err();
    assert!(e.contains("would send the profile's stream key to a new server"), "{e}");
    assert!(ids(&a).is_empty());
    run(&mut a, &ctx, "stream.destination_add", json!({"service": "twitch", "key": "none"})).unwrap();
    let d = a.studio.profile().stream.destinations[0].clone();
    assert_eq!((d.service.as_str(), Some(d.server.as_str()), d.name.as_str()), ("twitch", twitch.server, ""));
    // A service that shows each account its own server leaves the server empty.
    run(&mut a, &ctx, "stream.destination_add", json!({"service": "youtube"})).unwrap();
    let d = a.studio.profile().stream.destination("destination-2").cloned().unwrap();
    assert_eq!((d.service.as_str(), d.server.as_str()), ("youtube", ""));
    // A server given with the service is used.
    run(&mut a, &ctx, "stream.destination_add", json!({"service": "facebook", "server": "rtmp://mine.example/live"})).unwrap();
    let d = a.studio.profile().stream.destination("destination-3").cloned().unwrap();
    assert_eq!((d.service.as_str(), d.server.as_str()), ("facebook", "rtmp://mine.example/live"));
    let e = run(&mut a, &ctx, "stream.destination_add", json!({"service": "unknown"})).unwrap_err();
    assert!(e.contains("`service` is empty or one of") && e.contains("twitch"), "{e}");
    let e = run(&mut a, &ctx, "stream.destination_add", json!({"service": 3})).unwrap_err();
    assert!(e.contains("`service` is a string"), "{e}");
    assert_eq!(ids(&a).len(), 3);
    let list = run(&mut a, &ctx, "stream.destinations", json!({})).unwrap();
    let services: Vec<&str> = list["destinations"].as_array().unwrap().iter().filter_map(|d| d["service"].as_str()).collect();
    assert_eq!(services, ["twitch", "youtube", "facebook"]);
}

#[test]
fn changing_the_service_replaces_a_preset_server_and_keeps_a_typed_one() {
    let mut a = app();
    let ctx = egui::Context::default();
    let (twitch, facebook) = (preset("twitch"), preset("facebook"));
    run(&mut a, &ctx, "stream.destination_add", json!({"service": "twitch", "key": "none"})).unwrap();
    let main = |a: &StudioApp| a.studio.profile().stream.destinations[0].clone();
    run(&mut a, &ctx, "stream.destination_set", json!({"id": "main", "service": "facebook"})).unwrap();
    assert_eq!((main(&a).service.as_str(), Some(main(&a).server.as_str())), ("facebook", facebook.server));
    // A service without a shared server clears the preset's and turns the destination off.
    run(&mut a, &ctx, "stream.destination_set", json!({"id": "main", "service": "youtube"})).unwrap();
    assert_eq!((main(&a).server.as_str(), main(&a).enabled), ("", false));
    // A server and `enabled` given with the service win.
    run(&mut a, &ctx, "stream.destination_set", json!({"id": "main", "service": "kick", "server": "rtmp://k.example/app", "enabled": true})).unwrap();
    assert_eq!((main(&a).service.as_str(), main(&a).server.as_str(), main(&a).enabled), ("kick", "rtmp://k.example/app", true));
    // A server the user typed is kept through every change of service.
    for id in ["twitch", "youtube", ""] {
        run(&mut a, &ctx, "stream.destination_set", json!({"id": "main", "service": id})).unwrap();
        assert_eq!((main(&a).server.as_str(), main(&a).enabled), ("rtmp://k.example/app", true), "{id}");
    }
    assert_eq!(main(&a).service, "");
    run(&mut a, &ctx, "stream.destination_set", json!({"id": "main", "server": ""})).unwrap();
    run(&mut a, &ctx, "stream.destination_set", json!({"id": "main", "service": "twitch"})).unwrap();
    assert_eq!(Some(main(&a).server.as_str()), twitch.server, "an empty server takes the preset's");
    let e = run(&mut a, &ctx, "stream.destination_set", json!({"id": "main", "service": "unknown"})).unwrap_err();
    assert!(e.contains("`service` is empty or one of"), "{e}");
    // A key going with a preset's server counts as a new server.
    let e = run(&mut a, &ctx, "stream.destination_set", json!({"id": "main", "service": "facebook", "key": "held"})).unwrap_err();
    assert!(e.contains("would send its session key to a new server"), "{e}");
    assert_eq!(Some(main(&a).server.as_str()), twitch.server, "a refused change changes nothing");
    // A service without a shared server, saved with the account's server, stays on.
    assert!(main(&a).enabled);
    let mut e = Edit::of(&main(&a));
    e.set_service("youtube");
    assert_eq!(e.server, "");
    e.server = "rtmp://y.example/live".into();
    a.act(&ctx, Act::run("stream.destination_set", e.params()));
    assert_eq!((main(&a).service.as_str(), main(&a).server.as_str(), main(&a).enabled), ("youtube", "rtmp://y.example/live", true));
}

#[test]
fn saving_the_details_of_a_service_destination_keeps_it_on() {
    let mut a = app();
    let ctx = egui::Context::default();
    run(&mut a, &ctx, "stream.destination_add", json!({"service": "youtube"})).unwrap();
    let d = a.studio.profile().stream.destinations[0].clone();
    assert!(d.enabled && d.server.is_empty());
    let mut e = Edit::of(&d);
    e.name = "Channel".into();
    a.act(&ctx, Act::run("stream.destination_set", e.params()));
    let d = a.studio.profile().stream.destinations[0].clone();
    assert_eq!(
        (d.name.as_str(), d.service.as_str(), d.server.as_str(), d.enabled),
        ("Channel", "youtube", "", true),
        "the same service changes nothing"
    );
    // A service this version does not know is kept as it is.
    let cmd = CommandRef::with_params("stream.destination_set", json!({"service": "future"}));
    assert_eq!(service_param(&cmd, Some("future")).ok(), Some(Some("future".to_string())));
    assert!(service_param(&cmd, Some("")).is_err());
}

#[test]
fn the_details_change_the_server_with_the_service_and_link_to_its_page() {
    let (twitch, kick) = (preset("twitch"), preset("kick"));
    let d = Destination { id: "main".into(), service: "twitch".into(), server: twitch.server.unwrap_or_default().into(), ..Destination::default() };
    let mut e = Edit::of(&d);
    e.set_service("kick");
    assert_eq!((e.service.as_str(), e.server.as_str()), ("kick", ""));
    e.server = "rtmp://typed.example/app".into();
    e.set_service("twitch");
    assert_eq!(e.server, "rtmp://typed.example/app", "a typed server is kept");
    // A row names the service, and the details of a service without a shared server say where to
    // find the account's server and key.
    let d = Destination { id: "main".into(), service: "kick".into(), ..Destination::default() };
    let stream = StreamSettings { destinations: vec![d.clone()], ..StreamSettings::default() };
    let held = |_: &str| false;
    let view = View { stream: &stream, running: false, own: false, live: &[], held: &held, checks: &[], ingests: &[] };
    let mut panel = StreamingPanel { editing: Some(Edit::of(&d)), ..StreamingPanel::default() };
    let mut acts = Vec::new();
    let texts = drawn(|ui| show(ui, &view, &mut panel, &mut acts));
    let stream_to = craft_i18n::fmt(tl!("Stream to {service}"), &[("service", kick.name)]);
    let find = craft_i18n::fmt(tl!("{service} shows the server and key for your account."), &[("service", kick.name)]);
    for want in [kick.name, stream_to.as_str(), "Service", find.as_str(), "Where to find them", "Add Destination"] {
        assert!(texts.iter().any(|t| t == want), "{want} in {texts:?}");
    }
    assert!(acts.is_empty(), "drawing does nothing");
}

/// A finished check of `host` with `verdict`, which ran by itself when `automatic`.
fn diagnosis(verdict: Verdict, automatic: bool) -> Diagnosis {
    Diagnosis {
        verdict,
        host: "ingest.example".into(),
        port: 1935,
        tls: false,
        service: None,
        other: None,
        component: None,
        status: None,
        reason: None,
        detail: String::new(),
        handshake: None,
        evidence: vec![
            Evidence { check: Check::DefaultRoute, target: String::new(), outcome: Outcome::Ok, detail: String::new(), handshake: None },
            Evidence {
                check: Check::Connect,
                target: "192.0.2.1:1935".into(),
                outcome: Outcome::Failed(Cause::TimedOut),
                detail: "timed out".into(),
                handshake: None,
            },
            Evidence {
                check: Check::Connect443,
                target: "192.0.2.1:443".into(),
                outcome: Outcome::Unfinished,
                detail: String::new(),
                handshake: None,
            },
        ],
        at: UNIX_EPOCH + Duration::from_millis(1_700_000_000_123),
        automatic,
    }
}

#[test]
fn a_check_is_listed_and_shown_under_its_destination() {
    let d = diagnosis(Verdict::PortNotAnswering443Does, true);
    let v = listed_check(&CheckState::Done(Box::new(d.clone())));
    assert_eq!(
        (v["state"].clone(), v["automatic"].clone(), v["verdict"].clone()),
        (json!("done"), json!(true), json!("port_not_answering_443_does"))
    );
    assert_eq!(v["message"], json!("ingest.example answers on port 443 but not on port 1935. This network or the server may be blocking port 1935"));
    assert_eq!(v["template"], json!("{host} answers on port 443 but not on port {port}. This network or the server may be blocking port {port}"));
    assert_eq!(v["args"], json!({"host": "ingest.example", "port": "1935"}));
    assert_eq!(v["at_ms"], json!(1_700_000_000_123u64));
    assert_eq!(v["evidence"][1], json!({"check": "connect", "target": "192.0.2.1:1935", "outcome": "timed_out", "detail": "timed out"}));
    assert_eq!(v["evidence"][2]["outcome"], json!("unfinished"));
    assert_eq!(listed_check(&CheckState::Running { automatic: false }), json!({"state": "running", "automatic": false}));
    let lines: Vec<String> = d.evidence.iter().map(evidence_text).collect();
    assert_eq!(
        lines,
        [
            "Route to any address: works",
            "Connection to 192.0.2.1:1935: the server did not answer in time (timed out)",
            "Secure connection on port 443 to 192.0.2.1:443: not finished in time",
        ]
    );
    // The window shows the verdict, that it ran by itself, and the button that runs another.
    let dest = Destination { id: "main".into(), server: "rtmp://ingest.example/live".into(), ..Destination::default() };
    let stream = StreamSettings { destinations: vec![dest.clone()], ..StreamSettings::default() };
    let held = |_: &str| false;
    let checks = vec![("main".to_string(), CheckState::Done(Box::new(d.clone())))];
    let view = View { stream: &stream, running: false, own: false, live: &[], held: &held, checks: &checks, ingests: &[] };
    let mut acts = Vec::new();
    let texts = drawn(|ui| show(ui, &view, &mut StreamingPanel::default(), &mut acts));
    for want in [d.english().as_str(), "Checked by itself after the destination failed", "What the check found", "Check Connection"] {
        assert!(texts.iter().any(|t| t == want), "{want} in {texts:?}");
    }
    // While a check runs, the window says so.
    let checks = vec![("main".to_string(), CheckState::Running { automatic: false })];
    let view = View { stream: &stream, running: false, own: false, live: &[], held: &held, checks: &checks, ingests: &[] };
    let texts = drawn(|ui| show(ui, &view, &mut StreamingPanel::default(), &mut acts));
    assert!(texts.iter().any(|t| t == "Checking the connection…"), "{texts:?}");
    assert!(texts.iter().all(|t| *t != d.english()), "{texts:?}");
    assert!(acts.is_empty(), "drawing does nothing");
}

#[test]
fn a_certificate_verdict_names_the_reason_in_the_interface_language() {
    let d = Diagnosis { reason: Some(Cause::CertificateExpired), tls: true, ..diagnosis(Verdict::CertificateNotAccepted, false) };
    assert_eq!(verdict_msg(&d).english(), d.english(), "the window's reason is the same English text");
    assert!(d.english().contains("(the server's certificate has expired)"), "{}", d.english());
    let texts: std::collections::HashSet<&str> =
        [Outcome::Ok, Outcome::Set, Outcome::Problem, Outcome::NotRtmp, Outcome::Unknown, Outcome::Unfinished]
            .into_iter()
            .map(outcome_text)
            .collect();
    assert_eq!(texts.len(), 6, "every outcome has its own text");
    // What the port answered to the handshake is shown from its own template, which the window
    // translates, and not from the English text the control channel gets.
    let handshake = Evidence {
        check: Check::Handshake,
        target: "192.0.2.1:1935".into(),
        outcome: Outcome::NotRtmp,
        detail: "English text".into(),
        handshake: Some(Handshake::NotRtmp(0x48)),
    };
    assert_eq!(
        evidence_text(&handshake),
        "Stream server handshake with 192.0.2.1:1935: answers, but not as a stream server (the server's first byte is 0x48, where an RTMP server \
         sends its version, 0x03)"
    );
    let d =
        Diagnosis { detail: "English text".into(), handshake: Some(Handshake::Closed { received: 7 }), ..diagnosis(Verdict::NotStreamServer, false) };
    assert_eq!(
        verdict_msg(&d).english(),
        "ingest.example takes a connection on port 1935, but does not answer as a stream server: the server closed the connection after 7 of \
         the 3073 bytes of its handshake. The server address or port may be wrong, the stream server behind it may not be working, or something \
         on this network may be answering in its place"
    );
}

#[test]
fn every_check_message_has_an_english_row() {
    let en = include_str!("../../i18n/locales/en/studiocraft.tsv");
    let rows: std::collections::HashSet<(&str, &str)> =
        en.lines().filter_map(|l| l.split('\t').collect::<Vec<_>>().get(..2).map(|c| (c[0], c[1]))).collect();
    let templates = studiocraft_studio::diagnose::templates();
    assert_eq!(templates.len(), 23);
    let missing: Vec<String> = templates.iter().filter(|t| !rows.contains(&("@msg", **t))).map(|t| format!("@msg\t{t}\t{t}")).collect();
    assert!(missing.is_empty(), "check messages with no @msg row:\n{}", missing.join("\n"));
}

#[test]
fn check_connection_runs_a_check_and_the_service_setting_is_changed() {
    let mut a = app();
    let ctx = egui::Context::default();
    // An address with nothing behind it on this computer: the check stays on this computer.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = closed.local_addr().unwrap();
    drop(closed);
    run(&mut a, &ctx, "stream.destination_add", json!({"server": format!("rtmp://{addr}/live"), "key": "none"})).unwrap();
    run(&mut a, &ctx, "stream.destination_add", json!({"key": "none"})).unwrap();
    assert!(run(&mut a, &ctx, "stream.diagnose", json!({"id": "nope"})).unwrap_err().contains("no stream destination `nope`"));
    assert!(run(&mut a, &ctx, "stream.diagnose", json!({"id": "destination-2"})).unwrap_err().contains("has no server"));
    assert!(run(&mut a, &ctx, "stream.diagnose", json!({})).is_err());
    assert_eq!(run(&mut a, &ctx, "stream.diagnose", json!({"id": "main"})).unwrap(), json!({"id": "main", "started": true}));
    let listed = run(&mut a, &ctx, "stream.destinations", json!({})).unwrap();
    let state = listed["destinations"][0]["diagnosis"]["state"].clone();
    assert!(state == json!("running") || state == json!("done"), "{listed}");
    assert!(listed["destinations"][1].get("diagnosis").is_none(), "{listed}");
    // Another server drops the check of the one before it, finished or not.
    run(&mut a, &ctx, "stream.destination_set", json!({"id": "main", "server": "rtmp://127.0.0.1:1/live"})).unwrap();
    let listed = run(&mut a, &ctx, "stream.destinations", json!({})).unwrap();
    assert!(listed["destinations"][0].get("diagnosis").is_none(), "{listed}");
    // Whether the check after a failure contacts the service's other hosts: off by default.
    assert_eq!(listed["check_service"], json!(false));
    run(&mut a, &ctx, "app.settings", json!({})).unwrap();
    let r = run(&mut a, &ctx, "stream.check_service_set", json!({"enabled": true})).unwrap();
    assert_eq!(r, json!({"changed": true, "check_service": true}));
    assert!(a.studio.profile().stream.check_service);
    assert_eq!(a.settings.as_ref().map(|w| w.draft.stream.check_service), Some(true), "an open Settings window follows");
    assert_eq!(run(&mut a, &ctx, "stream.check_service_set", json!({"enabled": true})).unwrap()["changed"], json!(false));
    for bad in [json!({}), json!({"enabled": "yes"}), json!({"enabled": 1})] {
        assert!(run(&mut a, &ctx, "stream.check_service_set", bad.clone()).is_err(), "{bad}");
        assert!(a.studio.profile().stream.check_service, "{bad} changed nothing");
    }
    assert_eq!(crate::commands::ui_params("stream.diagnose"), "id: string");
    assert!(crate::commands::ui_params("stream.check_service_set").starts_with("enabled: boolean"));
}

#[test]
fn saving_a_copy_of_the_stream_is_set_by_command_and_listed() {
    let mut a = app();
    let ctx = egui::Context::default();
    let listed = run(&mut a, &ctx, "stream.destinations", json!({})).unwrap();
    assert_eq!((listed["save_copy"].clone(), listed["copy"].clone()), (json!(false), Value::Null), "off, and no stream has saved one");
    run(&mut a, &ctx, "app.settings", json!({})).unwrap();
    let r = run(&mut a, &ctx, "stream.save_copy_set", json!({"enabled": true})).unwrap();
    assert_eq!(r, json!({"changed": true, "save_copy": true}));
    assert!(a.studio.profile().stream.save_copy);
    assert_eq!(a.settings.as_ref().map(|w| w.draft.stream.save_copy), Some(true), "an open Settings window follows");
    assert_eq!(run(&mut a, &ctx, "stream.save_copy_set", json!({"enabled": true})).unwrap()["changed"], json!(false));
    for bad in [json!({}), json!({"enabled": "yes"}), json!({"enabled": 1}), json!({"enabled": null})] {
        assert!(run(&mut a, &ctx, "stream.save_copy_set", bad.clone()).is_err(), "{bad}");
        assert!(a.studio.profile().stream.save_copy, "{bad} changed nothing");
    }
    assert_eq!(run(&mut a, &ctx, "stream.destinations", json!({})).unwrap()["save_copy"], json!(true));
    assert!(crate::commands::ui_params("stream.save_copy_set").starts_with("enabled: boolean"));
    assert_eq!(a.studio.state_json()["streaming"]["copy"], Value::Null);
    let r = run(&mut a, &ctx, "stream.save_copy_set", json!({"enabled": false})).unwrap();
    assert_eq!(r, json!({"changed": true, "save_copy": false}));
}

fn server(name: &str, server: &str) -> IngestServer {
    IngestServer { name: name.into(), server: server.into() }
}

#[test]
fn the_details_offer_the_servers_of_the_services_list() {
    let twitch = preset("twitch");
    let documented = twitch.server.unwrap_or_default();
    let list: std::sync::Arc<[IngestServer]> = vec![server("West", "rtmps://west.example/app"), server("East", "rtmp://east.example/app")].into();
    // The service's documented server comes first when the list does not hold it, and once only
    // when it does.
    let choices: Vec<_> = ingest_choices(twitch, &list).collect();
    assert_eq!(choices, [(tl!("Default"), documented), ("West", "rtmps://west.example/app"), ("East", "rtmp://east.example/app")]);
    let with_default: Vec<IngestServer> = vec![server("West", "rtmps://west.example/app"), server("Here", documented)];
    let choices: Vec<_> = ingest_choices(twitch, &with_default).collect();
    assert_eq!(choices, [("West", "rtmps://west.example/app"), ("Here", documented)]);
    let draw = |server: &str, ingests: &[(&'static str, ListState)]| {
        let d = Destination { id: "main".into(), service: "twitch".into(), server: server.into(), ..Destination::default() };
        let stream = StreamSettings { destinations: vec![d.clone()], ..StreamSettings::default() };
        let held = |_: &str| false;
        let view = View { stream: &stream, running: false, own: false, live: &[], held: &held, checks: &[], ingests };
        let mut panel = StreamingPanel { editing: Some(Edit::of(&d)), ..StreamingPanel::default() };
        let mut acts = Vec::new();
        let texts = drawn(|ui| show(ui, &view, &mut panel, &mut acts));
        assert!(acts.is_empty(), "drawing does nothing");
        assert_eq!(panel.editing.map(|e| e.server).as_deref(), Some(server), "drawing keeps the server");
        texts
    };
    let has = |texts: &[String], want: &str| texts.iter().any(|t| t == want);
    // Before the list is asked for, and after it could not be read, List Servers asks for it.
    let texts = draw(documented, &[]);
    assert!(has(&texts, "Ingest server") && has(&texts, "List Servers"), "{texts:?}");
    let texts = draw(documented, &[("twitch", ListState::Loading)]);
    assert!(has(&texts, "Reading the list of servers…") && !has(&texts, "List Servers"), "{texts:?}");
    let failed = [("twitch", ListState::Failed("the server answered HTTP 503".into()))];
    let texts = draw(documented, &failed);
    assert!(has(&texts, "List Servers") && has(&texts, "The list could not be read: the server answered HTTP 503"), "{texts:?}");
    // Once it is read, the choice names the server in use, and a typed server is kept as it is.
    let ready = [("twitch", ListState::Ready(list.clone()))];
    assert!(has(&draw("rtmp://east.example/app", &ready), "East"));
    assert!(has(&draw(documented, &ready), tl!("Default")));
    assert!(has(&draw("rtmp://typed.example/live", &ready), "Typed server"));
    assert!(has(&draw("", &ready), "Choose a server"));
    // A service that publishes no list has no choice of server.
    let d = Destination { id: "main".into(), service: "kick".into(), ..Destination::default() };
    let stream = StreamSettings { destinations: vec![d.clone()], ..StreamSettings::default() };
    let held = |_: &str| false;
    let view = View { stream: &stream, running: false, own: false, live: &[], held: &held, checks: &[], ingests: &ready };
    let mut panel = StreamingPanel { editing: Some(Edit::of(&d)), ..StreamingPanel::default() };
    let texts = drawn(|ui| show(ui, &view, &mut panel, &mut Vec::new()));
    assert!(!has(&texts, "Ingest server") && !has(&texts, "List Servers"), "{texts:?}");
}

#[test]
fn listing_ingest_servers_takes_only_a_service_that_publishes_a_list() {
    let mut a = app();
    let ctx = egui::Context::default();
    assert!(run(&mut a, &ctx, "stream.ingest_servers", json!({})).unwrap_err().contains("needs `service`"));
    for bad in [json!({"service": "youtube"}), json!({"service": ""}), json!({"service": "unknown"}), json!({"service": "TWITCH"})] {
        let e = run(&mut a, &ctx, "stream.ingest_servers", bad.clone()).unwrap_err();
        assert!(e.contains("publishes a list of its ingest servers (twitch)"), "{bad}: {e}");
    }
    assert!(run(&mut a, &ctx, "stream.ingest_servers", json!({"service": 1})).unwrap_err().contains("is a string"));
    let e = run(&mut a, &ctx, "stream.ingest_servers", json!({"service": "twitch", "retry": "yes"})).unwrap_err();
    assert!(e.contains("`retry` is true or false"), "{e}");
    assert!(crate::commands::ui_params("stream.ingest_servers").starts_with("service: string"));
    assert!(crate::commands::ui_params("stream.ingest_servers").contains("retry?: boolean"));
    let twitch = preset("twitch");
    assert_eq!(listed_ingests(twitch, &ListState::Loading), json!({"service": "twitch", "state": "loading"}));
    let list: std::sync::Arc<[IngestServer]> = vec![server("West", "rtmps://west.example/app")].into();
    assert_eq!(
        listed_ingests(twitch, &ListState::Ready(list)),
        json!({"service": "twitch", "state": "ready", "servers": [{"name": "West", "server": "rtmps://west.example/app"}]})
    );
    assert_eq!(listed_ingests(twitch, &ListState::Failed("timed out".into())), json!({"service": "twitch", "state": "failed", "error": "timed out"}));
}
