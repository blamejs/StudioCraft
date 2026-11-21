use craft_keymap::CommandRef;
use serde_json::json;

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
    run(&mut a, &ctx, "stream.destination_add", json!({"server": "rtmp://a.example/live/secret"})).unwrap();
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
        Destination { id: "destination-2".into(), name: "Backup".into(), enabled: false, server: "rtmp://b.example/app".into(), key: KeyRef::None }
    );
    run(&mut a, &ctx, "stream.destination_add", json!({"key": "held", "held_name": "night"})).unwrap();
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
    let r = run(&mut a, &ctx, "stream.destination_set", json!({"id": "main", "name": "Main", "server": "rtmp://m.example/live", "key": "held"}));
    assert_eq!(r.unwrap(), json!({"id": "main", "changed": true}));
    let d = a.studio.profile().stream.destination("main").cloned().unwrap();
    assert_eq!((d.name.as_str(), d.server.as_str(), d.key), ("Main", "rtmp://m.example/live", KeyRef::Held("main".into())));
    // A held key keeps its name when `held_name` is left out, and takes a new one when it is given.
    run(&mut a, &ctx, "stream.destination_set", json!({"id": "main", "key": "held", "held_name": "evening"})).unwrap();
    run(&mut a, &ctx, "stream.destination_set", json!({"id": "main", "key": "held"})).unwrap();
    assert_eq!(a.studio.profile().stream.destination("main").map(|d| d.key.clone()), Some(KeyRef::Held("evening".into())));
    let r = run(&mut a, &ctx, "stream.destination_set", json!({"id": "main", "name": "Main", "enabled": true}));
    assert_eq!(r.unwrap(), json!({"id": "main", "changed": false}));
    run(&mut a, &ctx, "stream.destination_set", json!({"id": "main", "enabled": false, "key": "none"})).unwrap();
    let d = a.studio.profile().stream.destination("main").cloned().unwrap();
    assert!(!d.enabled && d.key == KeyRef::None);
}

#[test]
fn changes_reach_an_open_settings_window() {
    let mut a = app();
    let ctx = egui::Context::default();
    run(&mut a, &ctx, "app.settings", json!({})).unwrap();
    run(&mut a, &ctx, "stream.destination_add", json!({"server": "rtmp://a.example/live"})).unwrap();
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
    a.streaming =
        Some(StreamingPanel { editing: a.studio.profile().stream.destination("main").map(Edit::of), profile: a.studio.profile().name.clone() });
    assert_eq!(run(&mut a, &ctx, "stream.destination_remove", json!({"id": "main"})).unwrap(), json!({"removed": "main"}));
    assert!(ids(&a).is_empty());
    assert!(a.streaming.as_ref().is_some_and(|p| p.editing.is_none()));
}

#[test]
fn the_list_hides_keys_and_says_which_held_keys_are_set() {
    let mut a = app();
    let ctx = egui::Context::default();
    run(&mut a, &ctx, "stream.destination_add", json!({"server": "rtmp://a.example/live"})).unwrap();
    run(&mut a, &ctx, "stream.destination_add", json!({"server": "rtmp://b.example/live/secret-key?auth=token"})).unwrap();
    run(&mut a, &ctx, "stream.destination_add", json!({"server": "rtmp://c.example/live/room", "key": "held"})).unwrap();
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
    let r = run(&mut a, &ctx, "stream.destination_add", json!({"server": "rtmp://a.example/live"}));
    let after_failure = ids(&a);
    std::fs::remove_file(&profiles).unwrap();
    let again = run(&mut a, &ctx, "stream.destination_add", json!({"server": "rtmp://a.example/live"}));
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
fn a_change_reaches_only_its_own_destination_in_an_open_settings_draft() {
    let mut a = app();
    let ctx = egui::Context::default();
    run(&mut a, &ctx, "app.settings", json!({})).unwrap();
    // The Settings draft already holds an unsaved first destination.
    if let Some(w) = &mut a.settings {
        w.draft.stream.destinations = vec![Destination { id: "main".into(), server: "rtmp://typed.example/live".into(), ..Destination::default() }];
    }
    run(&mut a, &ctx, "stream.destination_add", json!({"server": "rtmp://a.example/live"})).unwrap();
    let draft = a.settings.as_ref().map(|w| w.draft.stream.destinations.clone()).unwrap();
    assert_eq!(draft, a.studio.profile().stream.destinations, "the added destination replaces the draft's one with its id");
}

#[test]
fn switching_profiles_closes_open_details() {
    let mut a = app();
    let ctx = egui::Context::default();
    run(&mut a, &ctx, "view.streaming", json!({})).unwrap();
    run(&mut a, &ctx, "stream.destination_add", json!({"server": "rtmp://a.example/live"})).unwrap();
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
        state("a", DestinationStatus::Failed("the server refused the stream".into())),
        state("b", DestinationStatus::Live),
        state("c", DestinationStatus::Retrying { attempt: 3, error: "the connection was lost".into() }),
    ];
    let held = |_: &str| false;
    let view = View { stream: &stream, running: true, live: &live, held: &held };
    let mut panel = StreamingPanel::default();
    let mut acts = Vec::new();
    let texts = drawn(|ui| show(ui, &view, &mut panel, &mut acts));
    for want in [
        "First",
        "Second",
        "c",
        "Failed: the server refused the stream",
        "Live",
        "Trying again (try 3): the connection was lost",
        "Not in this stream; it is sent to from the next start",
        "rtmp://a.example/live/•••",
        "No key is held for this session yet",
        "4 of 16 destinations",
    ] {
        assert!(texts.iter().any(|t| t == want), "{want} in {texts:?}");
    }
    assert!(!texts.iter().any(|t| t.contains("hidden-a")), "{texts:?}");
    // A failed destination offers Start, and the others that are sending offer Stop.
    assert_eq!(texts.iter().filter(|t| *t == "Start").count(), 1, "{texts:?}");
    assert_eq!(texts.iter().filter(|t| *t == "Stop").count(), 2, "{texts:?}");
    assert!(acts.is_empty(), "drawing does nothing");
    // Without a stream each destination says whether starting the stream sends to it.
    let view = View { stream: &stream, running: false, live: &[], held: &held };
    let texts = drawn(|ui| show(ui, &view, &mut panel, &mut acts));
    assert_eq!(texts.iter().filter(|t| *t == "Sent to when the stream starts").count(), 3, "{texts:?}");
    assert_eq!(texts.iter().filter(|t| *t == "Off").count(), 1, "{texts:?}");
    assert!(!texts.iter().any(|t| t == "Start" || t == "Stop"), "{texts:?}");
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
    assert_eq!(p, json!({"id": "b", "name": "B", "server": "rtmp://b2.example/live", "key": "held", "held_name": "night"}));
    assert!(!p.to_string().contains("typed"), "a key typed for the session is not a command parameter");
    e.key = KeyChoice::Profile;
    assert_eq!(e.params(), json!({"id": "b", "name": "B", "server": "rtmp://b2.example/live", "key": "profile"}));
    // The details of a destination are drawn with its fields, and the server field hides its text.
    let stream = StreamSettings { destinations: vec![d.clone()], ..StreamSettings::default() };
    let held = |n: &str| n == "night";
    let view = View { stream: &stream, running: false, live: &[], held: &held };
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
    let view = View { stream: &stream, running: false, live: &[], held: &held };
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
