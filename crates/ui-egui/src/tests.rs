use egui::{Event, Key, Modifiers, RawInput};

use super::*;

fn app() -> StudioApp {
    StudioApp::boot(Store::memory(), None, Vec::new())
}

/// Waits until `source` has published its first content, so its size is known.
fn wait_shown(a: &StudioApp, source: SourceId) {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    assert!(a.studio().notify_source_shown(source, tx), "the source is running");
    rx.recv().unwrap();
}

fn frame(app: &mut StudioApp, ctx: &egui::Context, events: Vec<Event>) {
    let raw = RawInput { events, screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 800.0))), ..RawInput::default() };
    let mut out = ctx.run_ui(raw, |ui| app.frame(ui));
    out.textures_delta.clear();
}

fn frame_at(app: &mut StudioApp, ctx: &egui::Context, size: egui::Vec2) {
    let raw = RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)), ..RawInput::default() };
    let mut out = ctx.run_ui(raw, |ui| app.frame(ui));
    out.textures_delta.clear();
}

/// The output bar's parts as `ui.inspect` reports them: name, left, top, right, bottom.
fn bar_parts(app: &StudioApp, ctx: &egui::Context) -> Vec<(String, [f32; 4])> {
    let v = app.ui_inspect(ctx);
    let f = |r: &Value, k: &str| r[k].as_f64().unwrap_or(f64::NAN) as f32;
    v["output_bar"]["parts"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|p| {
                    let r = &p["rect"];
                    let (x, y) = (f(r, "x"), f(r, "y"));
                    (p["name"].as_str().unwrap_or("").to_string(), [x, y, x + f(r, "width"), y + f(r, "height")])
                })
                .collect()
        })
        .unwrap_or_default()
}

fn press(key: Key, modifiers: Modifiers) -> Vec<Event> {
    vec![
        Event::Key { key, physical_key: Some(key), pressed: true, repeat: false, modifiers },
        Event::Key { key, physical_key: Some(key), pressed: false, repeat: false, modifiers },
    ]
}

fn run(app: &mut StudioApp, ctx: &egui::Context, id: &str, params: Value) -> Result<Value, String> {
    app.perform(ctx, &CommandRef::with_params(id, params))
}

#[test]
fn boots_with_a_default_collection_and_profile() {
    let a = app();
    assert_eq!(a.studio().collection().name, DEFAULT_NAME);
    assert_eq!(a.studio().profile().name, DEFAULT_NAME);
    let hud = a.hud();
    assert_eq!(hud["ui"]["collections"], json!([DEFAULT_NAME]));
    assert_eq!(hud["ui"]["profiles"], json!([DEFAULT_NAME]));
    assert_eq!(hud["collection"]["global_audio"].as_array().map(Vec::len), Some(2));
}

#[test]
fn frames_draw_in_both_modes() {
    let mut a = app();
    let ctx = egui::Context::default();
    for _ in 0..3 {
        frame(&mut a, &ctx, vec![]);
    }
    run(&mut a, &ctx, "studio.toggle", json!({})).unwrap();
    frame(&mut a, &ctx, vec![]);
    run(&mut a, &ctx, "view.stats", json!({})).unwrap();
    run(&mut a, &ctx, "app.about", json!({})).unwrap();
    run(&mut a, &ctx, "app.settings", json!({})).unwrap();
    frame(&mut a, &ctx, vec![]);
    let hud = a.hud();
    assert_eq!(hud["studio_mode"], json!(true));
    assert_eq!(hud["ui"]["settings_open"], json!(true));
    assert_eq!(hud["ui"]["about_open"], json!(true));
    assert!(a.frames() >= 5);
}

#[test]
fn view_scopes_opens_the_window_and_its_scope_is_a_command() {
    let mut a = app();
    let ctx = egui::Context::default();
    assert_eq!(a.hud()["ui"]["scopes"], json!({"open": false, "kind": "waveform", "computed": 0, "dropped": 0, "skipped": 0}));
    assert_eq!(run(&mut a, &ctx, "view.scopes", json!({})), Ok(json!({"on": true})));
    assert!(a.scopes.is_some(), "the scopes thread runs while the window is open");
    assert_eq!(a.item_state("view.scopes").checked, Some(true));
    assert_eq!(run(&mut a, &ctx, "scopes.kind", json!({"kind": "vectorscopeHls"})), Ok(json!({"kind": "vectorscopeHls"})));
    assert_eq!(a.store().prefs().scope_kind, "vectorscopeHls");
    let err = run(&mut a, &ctx, "scopes.kind", json!({"kind": "oscilloscope"})).unwrap_err();
    assert!(err.contains("unknown scope") && err.contains("waveform"), "{err}");
    // Only the five ids the params text lists are accepted.
    for other in ["YUV", "vectorscope", "WAVEFORM", "Vector-Scope HLS", "Histogram"] {
        assert!(run(&mut a, &ctx, "scopes.kind", json!({"kind": other})).is_err(), "{other}");
    }
    assert!(run(&mut a, &ctx, "scopes.kind", json!({})).is_err(), "the kind is required");
    assert_eq!(a.store().prefs().scope_kind, "vectorscopeHls", "a refused kind changes nothing");
    frame(&mut a, &ctx, vec![]);
    assert_eq!(a.hud()["ui"]["scopes"]["kind"], json!("vectorscopeHls"));
    assert_eq!(run(&mut a, &ctx, "view.scopes", json!({})), Ok(json!({"on": false})));
    assert!(a.scopes.is_none(), "closing the window stops the thread");
    frame(&mut a, &ctx, vec![]);
}

#[test]
fn the_scopes_window_shows_the_picture_the_thread_computed() {
    use studiocraft_media::pool::Pool;
    use studiocraft_media::video::{FramePool, PixelFormat, VideoFrame};
    let mut a = app();
    let ctx = egui::Context::default();
    run(&mut a, &ctx, "view.scopes", json!({})).unwrap();
    let pool: Arc<FramePool> = Pool::new(2, VideoFrame::default);
    let mut f = pool.take();
    assert!(f.reshape(32, 16, PixelFormat::Nv12));
    f.data.fill(128);
    // Stand in for the compositor: hand one copy over, and wait for the thread's picture.
    let s = a.scopes.as_mut().unwrap();
    s.feed_queue().push(Arc::new(f));
    s.pump(scopes::kind_of("waveform"));
    assert!(s.wait());
    frame(&mut a, &ctx, vec![]);
    let s = a.scopes.as_ref().unwrap();
    assert_eq!(s.latest.as_ref().map(|p| (p.width, p.height)), Some((32, 256)));
    assert!(s.texture.is_some() && !s.fresh, "the window uploaded the picture");
    assert_eq!(a.hud()["ui"]["scopes"]["computed"], json!(1));
}

#[test]
fn the_menu_strip_is_the_title_bar_when_the_app_draws_the_frame() {
    let mut a = app();
    assert!(!a.draws_title_bar(), "the desktop draws the frame by default");
    a.set_title_bar(Some(craft_window::Layout { left: vec![craft_window::Button::Close], right: vec![craft_window::Button::Minimize] }));
    assert!(a.draws_title_bar());
    let ctx = egui::Context::default();
    for _ in 0..3 {
        frame(&mut a, &ctx, vec![]);
    }
    // The menus still run their commands from the strip.
    run(&mut a, &ctx, "app.settings", json!({})).unwrap();
    frame(&mut a, &ctx, vec![]);
    assert_eq!(a.hud()["ui"]["settings_open"], json!(true));
}

#[test]
fn answer_dialog_presses_the_exit_confirmation_buttons() {
    let mut a = app();
    let ctx = egui::Context::default();
    a.confirm_exit = true;
    assert!(a.answer_dialog(&ctx, true, Some("x")).is_err_and(|e| e.contains("no `text`")));
    assert!(a.confirming_exit(), "a refused answer leaves the confirmation open");
    assert_eq!(a.answer_dialog(&ctx, false, None), Ok(json!({"dialog": "confirm_exit", "closed": true, "exiting": false})));
    assert!(!a.confirming_exit() && !a.exit_confirmed);
    a.confirm_exit = true;
    assert_eq!(a.answer_dialog(&ctx, true, None), Ok(json!({"dialog": "confirm_exit", "closed": true, "exiting": true})));
    assert!(!a.confirming_exit() && a.exit_confirmed);
    assert!(a.answer_dialog(&ctx, true, None).is_err(), "nothing is left to answer");
}

#[test]
fn the_settings_window_starts_from_the_saved_title_bar_setting() {
    for on in [false, true] {
        let mut a = app();
        a.store.update(|p| p.system_title_bar = on);
        a.open_settings(crate::settings::Tab::General);
        assert_eq!(a.settings.as_ref().map(|w| w.system_title_bar), Some(on));
    }
}

#[test]
fn a_new_graphics_adapter_choice_is_saved_and_forgets_the_old_losses() {
    use studiocraft_render::device::Preference;
    let mut a = app();
    a.store.update(|p| {
        p.graphics_adapter = Preference::LowPower;
        p.graphics_losses = vec![1, 2];
    });
    a.set_window_gpu(Preference::LowPower, vec![tmsg!("Nothing can be drawn until StudioCraft starts again")], false);
    a.open_settings(crate::settings::Tab::Video);
    let w = a.settings.as_mut().unwrap();
    assert_eq!(w.graphics_adapter, Preference::LowPower, "the window starts from the saved choice");
    assert_eq!(w.gpu_notices.len(), 1, "and shows what the user was told at start");
    let apply = |a: &mut StudioApp, choice: Preference| {
        let w = a.settings.as_ref().unwrap();
        a.apply_settings(crate::settings::Apply {
            profile: w.draft.clone(),
            overrides: a.keymap.overrides().clone(),
            language: w.language.clone(),
            check_updates: w.check_updates,
            system_title_bar: w.system_title_bar,
            graphics_adapter: choice,
            canvas_display: None,
        })
    };
    // the same choice keeps the losses
    assert!(apply(&mut a, Preference::LowPower));
    assert_eq!(a.store.prefs().graphics_losses, [1, 2]);
    assert!(apply(&mut a, Preference::Software));
    assert_eq!(a.store.prefs().graphics_adapter, Preference::Software);
    assert!(a.store.prefs().graphics_losses.is_empty());
    assert!(!a.relaunch_requested(), "nothing was lost");
}

#[test]
fn a_lost_window_device_does_not_start_the_app_again_when_saving_fails() {
    use craft_settings::Location;
    use studiocraft_render::device::Preference;
    // a settings folder that is a file, so nothing can be saved in it
    let blocked = std::env::temp_dir().join(format!("studiocraft-unsaved-{}", std::process::id()));
    std::fs::write(&blocked, b"not a folder").unwrap();
    let (store, _) = Store::open(Location::Dir(blocked.clone()));
    let mut a = StudioApp::boot(store, None, Vec::new());
    let ctx = egui::Context::default();
    let gpu = window_gpu(wgpu::DeviceType::DiscreteGpu);
    a.studio.set_gpu(Some(gpu.clone()));
    a.set_window_gpu(Preference::Auto, Vec::new(), true);
    frame(&mut a, &ctx, vec![]);
    gpu.device.destroy();
    a.studio.gpu().unwrap().poll();
    frame(&mut a, &ctx, vec![]);
    assert!(!a.relaunch_requested(), "the window stays open with the unsaved work");
    assert!(!a.exit_confirmed);
    let _ = std::fs::remove_file(&blocked);
}

#[test]
fn a_device_loss_is_seen_while_the_window_is_hidden() {
    use studiocraft_render::device::Preference;
    let mut a = app();
    let ctx = egui::Context::default();
    let gpu = window_gpu(wgpu::DeviceType::DiscreteGpu);
    a.studio.set_gpu(Some(gpu.clone()));
    a.set_window_gpu(Preference::Auto, Vec::new(), true);
    frame(&mut a, &ctx, vec![]);
    gpu.device.destroy();
    a.studio.gpu().unwrap().poll();
    // a hidden window gets `logic` calls and no frames
    a.logic(&ctx);
    assert!(a.relaunch_requested());
    assert_eq!(a.store.prefs().graphics_losses.len(), 1);
    a.logic(&ctx);
    frame(&mut a, &ctx, vec![]);
    assert_eq!(a.store.prefs().graphics_losses.len(), 1, "one loss is counted once");
}

/// A window device on wgpu's noop adapter, shown to the app as `device_type`.
fn window_gpu(device_type: wgpu::DeviceType) -> Gpu {
    let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
    desc.backends = wgpu::Backends::NOOP;
    desc.backend_options.noop = wgpu::NoopBackendOptions { enable: true, ..Default::default() };
    let (opened, _) = Gpu::open_on(wgpu::Instance::new(desc), &studiocraft_render::device::Preference::Software).unwrap();
    let mut adapter = opened.adapter().cloned().unwrap();
    adapter.device_type = device_type;
    Gpu::new(opened.device.clone(), opened.queue.clone()).with_adapter(adapter)
}

#[test]
fn a_lost_window_device_starts_the_app_again_unless_that_would_end_the_same_way() {
    use studiocraft_render::device::Preference;
    use wgpu::DeviceType::{Cpu, DiscreteGpu};
    let ctx = egui::Context::default();
    let now = crate::hardware::now_secs();
    // (adapter drawn on, preference opened with, earlier losses, may start itself) -> starts again
    for (kind, opened_with, earlier, may, again) in [
        (DiscreteGpu, Preference::Auto, vec![], true, true),
        (DiscreteGpu, Preference::Auto, vec![], false, false),
        // a third loss moves the next start to software
        (DiscreteGpu, Preference::Auto, vec![now - 20, now - 10], true, true),
        // software asked for on a machine without a software adapter
        (DiscreteGpu, Preference::Software, vec![], true, true),
        (DiscreteGpu, Preference::Software, vec![now - 20, now - 10], true, false),
        (Cpu, Preference::Auto, vec![], true, false),
    ] {
        let mut a = app();
        a.store.update(|p| p.graphics_losses = earlier.clone());
        let gpu = window_gpu(kind);
        a.studio.set_gpu(Some(gpu.clone()));
        a.set_window_gpu(opened_with.clone(), Vec::new(), may);
        frame(&mut a, &ctx, vec![]);
        assert!(!a.relaunch_requested() && a.store.prefs().graphics_losses.len() == earlier.len());
        gpu.device.destroy();
        // the studio notices the loss on its next probe
        a.studio.gpu().unwrap().poll();
        frame(&mut a, &ctx, vec![]);
        let case = format!("{kind:?}, opened for {opened_with:?}, {} earlier losses, may start itself {may}", earlier.len());
        assert_eq!(a.relaunch_requested(), again, "{case}");
        assert_eq!(a.exit_confirmed, again, "{case}");
        assert_eq!(a.store.prefs().graphics_losses.len(), earlier.len() + 1, "the loss is kept for the next start: {case}");
        frame(&mut a, &ctx, vec![]);
        assert_eq!(a.store.prefs().graphics_losses.len(), earlier.len() + 1, "one loss is counted once: {case}");
    }
}

#[test]
fn scene_shortcuts_switch_scenes() {
    let mut a = app();
    let ctx = egui::Context::default();
    run(&mut a, &ctx, "scene.add", json!({"name": "Second"})).unwrap();
    run(&mut a, &ctx, "scene.select", json!({"scene": "Scene"})).unwrap();
    frame(&mut a, &ctx, vec![]);
    frame(&mut a, &ctx, press(Key::Num2, Modifiers::COMMAND));
    let c = a.studio().collection();
    assert_eq!(c.scene(c.program_scene).map(|s| s.name.as_str()), Some("Second"));
    frame(&mut a, &ctx, press(Key::Num1, Modifiers::COMMAND));
    let c = a.studio().collection();
    assert_eq!(c.scene(c.program_scene).map(|s| s.name.as_str()), Some("Scene"));
}

#[test]
fn undo_shortcut_undoes_the_last_change() {
    let mut a = app();
    let ctx = egui::Context::default();
    run(&mut a, &ctx, "scene.add", json!({"name": "Extra"})).unwrap();
    a.studio.seal_history();
    frame(&mut a, &ctx, vec![]);
    frame(&mut a, &ctx, press(Key::Z, Modifiers::COMMAND));
    assert_eq!(a.studio().collection().scenes.len(), 1);
}

#[test]
fn collections_are_made_switched_renamed_and_removed() {
    let mut a = app();
    let ctx = egui::Context::default();
    run(&mut a, &ctx, "scene.add", json!({"name": "Only in the first"})).unwrap();
    a.named(Named::NewCollection, "Second").unwrap();
    assert_eq!(a.studio().collection().name, "Second");
    assert_eq!(a.studio().collection().scenes.len(), 1);
    assert_eq!(a.hud()["ui"]["collections"], json!(["Second", DEFAULT_NAME]));
    run(&mut a, &ctx, "collection.switch", json!({"name": DEFAULT_NAME})).unwrap();
    assert!(a.studio().collection().scene_by_name("Only in the first").is_some(), "kept while away");
    assert!(a.named(Named::NewCollection, "second").is_err(), "names are compared without case");
    a.named(Named::RenameCollection, "Main").unwrap();
    assert_eq!(a.hud()["ui"]["collections"], json!(["Main", "Second"]));
    run(&mut a, &ctx, "collection.remove", json!({})).unwrap();
    assert_eq!(a.studio().collection().name, "Second");
    assert!(run(&mut a, &ctx, "collection.remove", json!({})).is_err(), "the last one stays");
    assert_eq!(a.store().prefs().collection, "Second");
}

#[test]
fn a_failed_save_keeps_the_current_collection_open() {
    let dir = temp_dir("save-fails");
    let (store, _) = Store::open(craft_settings::Location::Dir(dir.clone()));
    let mut a = StudioApp::boot(store, None, Vec::new());
    let ctx = egui::Context::default();
    a.named(Named::NewCollection, "Second").unwrap();
    run(&mut a, &ctx, "collection.switch", json!({"name": DEFAULT_NAME})).unwrap();
    run(&mut a, &ctx, "scene.add", json!({"name": "Unsaved"})).unwrap();
    // A folder where the collection's file goes makes every save of it fail.
    let file = dir.join("scenes").join(format!("{DEFAULT_NAME}.json"));
    std::fs::remove_file(&file).unwrap();
    std::fs::create_dir(&file).unwrap();

    assert!(run(&mut a, &ctx, "collection.switch", json!({"name": "Second"})).is_err());
    let import = dir.join("show.json");
    studiocraft_studio::files::write_collection_file(&import, &SceneCollection::new("Show", "Opening")).unwrap();
    assert!(run(&mut a, &ctx, "collection.import", json!({"path": import.to_string_lossy()})).is_err());
    for what in [Named::NewCollection, Named::DuplicateCollection, Named::RenameCollection] {
        assert!(a.named(what, "Fourth").is_err(), "{what:?}");
    }
    assert_eq!(a.studio().collection().name, DEFAULT_NAME);
    assert!(a.studio().collection().scene_by_name("Unsaved").is_some(), "the changes stay");
    assert!(a.studio().engine().is_dirty());
    let exported = dir.join("exported.json");
    run(&mut a, &ctx, "collection.export", json!({"path": exported.to_string_lossy()})).unwrap();
    assert!(exported.is_file());
    assert!(a.studio().engine().is_dirty(), "an export does not stand in for the save that failed");
    assert_eq!(a.store().collections(), ["Second", DEFAULT_NAME]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_failed_profile_save_keeps_the_current_profile_open() {
    let dir = temp_dir("profile-save-fails");
    let (store, _) = Store::open(craft_settings::Location::Dir(dir.clone()));
    let mut a = StudioApp::boot(store, None, Vec::new());
    let ctx = egui::Context::default();
    a.named(Named::NewProfile, "Second").unwrap();
    run(&mut a, &ctx, "profile.switch", json!({"name": DEFAULT_NAME})).unwrap();
    // A folder where the profile's file goes makes every save of it fail.
    let file = dir.join("profiles").join(format!("{DEFAULT_NAME}.json"));
    std::fs::remove_file(&file).unwrap();
    std::fs::create_dir(&file).unwrap();
    assert!(run(&mut a, &ctx, "profile.switch", json!({"name": "Second"})).is_err());
    assert_eq!(a.studio().profile().name, DEFAULT_NAME);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_rename_that_changes_only_the_case_keeps_one_file() {
    let dir = temp_dir("case-rename");
    let (store, _) = Store::open(craft_settings::Location::Dir(dir.clone()));
    let mut a = StudioApp::boot(store, None, Vec::new());
    a.named(Named::NewCollection, "Show").unwrap();
    a.named(Named::RenameCollection, "show").unwrap();
    assert_eq!(a.store().collections(), ["show".to_string(), DEFAULT_NAME.to_string()]);
    assert!(a.store().load_collection("show").is_ok());
    a.named(Named::NewProfile, "Live").unwrap();
    a.named(Named::RenameProfile, "LIVE").unwrap();
    assert!(a.store().profiles().iter().any(|p| p == "LIVE"));
    assert!(!a.store().profiles().iter().any(|p| p == "Live"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_new_collection_whose_first_save_failed_is_saved_at_exit() {
    let dir = temp_dir("first-save-fails");
    std::fs::create_dir_all(&dir).unwrap();
    // A file where the collections folder goes makes the save at start fail.
    let scenes = dir.join("scenes");
    std::fs::write(&scenes, b"").unwrap();
    let (store, _) = Store::open(craft_settings::Location::Dir(dir.clone()));
    let mut a = StudioApp::boot(store, None, Vec::new());
    std::fs::remove_file(&scenes).unwrap();
    let saved = a.save_all();
    let on_disk = a.store().load_collection(DEFAULT_NAME);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(saved);
    assert!(on_disk.is_ok(), "{on_disk:?}");
}

#[test]
fn an_unreadable_collection_is_kept_at_startup() {
    let dir = temp_dir("unreadable");
    let scenes = dir.join("scenes");
    std::fs::create_dir_all(&scenes).unwrap();
    let file = scenes.join(format!("{DEFAULT_NAME}.json"));
    std::fs::write(&file, "{").unwrap();
    let (store, _) = Store::open(craft_settings::Location::Dir(dir.clone()));
    let a = StudioApp::boot(store, None, Vec::new());
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "{", "the file is left as it was");
    assert_eq!(a.studio().collection().name, format!("{DEFAULT_NAME} 2"));
    assert_eq!(a.store().collections(), [DEFAULT_NAME.to_string(), format!("{DEFAULT_NAME} 2")]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_collection_repaired_on_reading_is_not_written_back_at_startup() {
    let dir = temp_dir("repaired");
    let scenes = dir.join("scenes");
    std::fs::create_dir_all(&scenes).unwrap();
    let c = default_collection(DEFAULT_NAME);
    // An item whose source is missing: reading the file drops it.
    let mut doc: Value = serde_json::from_str(&studiocraft_model::format::write_collection(&c).unwrap()).unwrap();
    let Some(items) = doc["data"]["scenes"][0]["items"].as_array_mut() else { panic!("no scene in {doc}") };
    items.push(json!({"id": 999_998, "source": 999_999}));
    let text = doc.to_string();
    let file = scenes.join(format!("{DEFAULT_NAME}.json"));
    std::fs::write(&file, &text).unwrap();
    let (store, _) = Store::open(craft_settings::Location::Dir(dir.clone()));
    let a = StudioApp::boot(store, None, Vec::new());
    let on_disk = std::fs::read_to_string(&file).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(a.studio().collection().name, DEFAULT_NAME);
    assert_eq!(on_disk, text, "the file is left as it was");
}

#[test]
fn free_names_count_up_from_the_base() {
    assert_eq!(free_name("Show", |_| false), "Show");
    assert_eq!(free_name("Show", |n| n == "Show" || n == "Show 2"), "Show 3");
    let long = "x".repeat(100);
    assert_eq!(free_name(&long, |n| n == long), format!("{DEFAULT_NAME} 2"));
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("studiocraft-ui-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn importing_a_collection_adds_and_selects_it() {
    let dir = temp_dir("import");
    let path = dir.join("show.json");
    let mut c = SceneCollection::new("Show", "Opening");
    c.scenes[0].name = "Opening".into();
    studiocraft_studio::files::write_collection_file(&path, &c).unwrap();

    let mut a = app();
    let ctx = egui::Context::default();
    let r = run(&mut a, &ctx, "collection.import", json!({"path": path.to_string_lossy()})).unwrap();
    assert_eq!(r, json!({"name": "Show"}));
    assert_eq!(a.studio().collection().name, "Show");
    assert!(a.studio().collection().scene_by_name("Opening").is_some());
    assert_eq!(a.store().prefs().collection, "Show");
    assert_eq!(a.hud()["ui"]["collections"], json!(["Show", DEFAULT_NAME]));

    let r = run(&mut a, &ctx, "collection.import", json!({"path": path.to_string_lossy()})).unwrap();
    assert_eq!(r, json!({"name": "Show 2"}), "a taken name gets a number");
    assert_eq!(a.studio().collection().name, "Show 2");
    assert_eq!(a.hud()["ui"]["collections"], json!(["Show", "Show 2", DEFAULT_NAME]));
    assert!(a.store().load_collection("Show 2").is_ok());

    std::fs::write(dir.join("broken.json"), "{").unwrap();
    assert!(run(&mut a, &ctx, "collection.import", json!({"path": dir.join("broken.json").to_string_lossy()})).is_err());
    assert_eq!(a.studio().collection().name, "Show 2", "a failed import keeps the collection");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn exporting_writes_a_file_that_reads_back() {
    let dir = temp_dir("export");
    let path = dir.join("out.json");
    let mut a = app();
    let ctx = egui::Context::default();
    run(&mut a, &ctx, "scene.add", json!({"name": "Exported scene"})).unwrap();
    let r = run(&mut a, &ctx, "collection.export", json!({"path": path.to_string_lossy()})).unwrap();
    assert_eq!(r["path"], json!(path.to_string_lossy()));
    let back = studiocraft_studio::files::read_collection_file(&path).unwrap();
    assert_eq!(&back, a.studio().collection());
    assert!(back.scene_by_name("Exported scene").is_some());
    assert!(!a.studio().engine().is_dirty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn collection_files_without_a_path_or_dialog_are_errors() {
    let mut a = app();
    let ctx = egui::Context::default();
    for id in ["collection.import", "collection.export"] {
        let e = run(&mut a, &ctx, id, json!({})).unwrap_err();
        assert!(e.contains("needs `path`"), "{id}: {e}");
        assert!(run(&mut a, &ctx, id, json!({"path": 7})).is_err(), "{id}");
        assert!(run(&mut a, &ctx, id, json!({"path": ""})).is_err(), "{id}");
    }
    assert_eq!(a.studio().collection().name, DEFAULT_NAME);
}

#[test]
fn profiles_keep_their_own_settings() {
    let mut a = app();
    let ctx = egui::Context::default();
    a.named(Named::DuplicateProfile, "Stream").unwrap();
    let mut p = a.studio().profile().clone();
    p.stream.primary_mut().server = "rtmp://example.invalid/live".into();
    a.studio.set_profile(p).unwrap();
    run(&mut a, &ctx, "profile.switch", json!({"name": DEFAULT_NAME})).unwrap();
    assert_eq!(a.studio().profile().stream.primary_server(), "");
    run(&mut a, &ctx, "profile.switch", json!({"name": "Stream"})).unwrap();
    assert_eq!(a.studio().profile().stream.primary_server(), "rtmp://example.invalid/live");
    assert!(a.named(Named::NewProfile, "bad/name").is_err());
}

#[test]
fn available_encoders_lists_the_built_in_ones_without_ffmpeg() {
    let mut a = app();
    let ctx = egui::Context::default();
    let dir = temp_dir("no-ffmpeg");
    let mut p = a.studio().profile().clone();
    p.output.ffmpeg_path = dir.join("ffmpeg-missing").to_string_lossy().into_owned();
    a.studio.set_profile(p).unwrap();
    run(&mut a, &ctx, "tools.encoders", json!({})).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    let list = a.encoders.clone().unwrap();
    assert!(list.iter().any(|(name, _, ok)| name == "H.264 (built in)" && *ok), "{list:?}");
    // The built-in NVENC, Media Foundation, VideoToolbox and VA-API encoders are ready where their
    // hardware encoder opens.
    assert!(list.iter().filter(|(name, _, _)| !name.ends_with("(built in)")).all(|(_, _, ok)| !ok), "{list:?}");
    let missing = a.encoders_ffmpeg.clone().map(|m| (m.english(), m.shown_in(&crate::i18n::I18N, crate::i18n::I18N.from_code("de").unwrap())));
    let (english, german) = missing.unwrap_or_default();
    assert!(english.starts_with("ffmpeg at ") && english.contains("ffmpeg-missing") && english.ends_with(" was not found"), "{english:?}");
    assert!(german.contains("ffmpeg-missing") && german.ends_with("wurde nicht gefunden"), "{german:?}");
    // Without a configured path the error's own text is shown, and its plain row translates it.
    let unset = Msg::plain("ffmpeg (set its location in Settings › Output) was not found");
    assert!(unset.shown_in(&crate::i18n::I18N, crate::i18n::I18N.from_code("de").unwrap()).starts_with("ffmpeg wurde nicht gefunden"));
}

#[test]
fn dock_and_view_toggles_flip_the_preferences() {
    let mut a = app();
    let ctx = egui::Context::default();
    assert_eq!(run(&mut a, &ctx, "dock.mixer", json!({})).unwrap(), json!({"on": false}));
    assert!(!a.store().prefs().docks.mixer);
    run(&mut a, &ctx, "dock.reset", json!({})).unwrap();
    assert!(a.store().prefs().docks.mixer);
    assert_eq!(run(&mut a, &ctx, "view.lock_preview", json!({})).unwrap(), json!({"on": true}));
    frame(&mut a, &ctx, vec![]);
}

#[test]
fn the_output_bar_fits_the_menu_row_at_each_window_size() {
    for (w, h, transitions) in [(1280.0, 800.0, Some("inline")), (1024.0, 640.0, None), (760.0, 480.0, Some("overflow"))] {
        let mut a = app();
        let ctx = egui::Context::default();
        for _ in 0..3 {
            frame_at(&mut a, &ctx, egui::vec2(w, h));
        }
        let v = a.ui_inspect(&ctx);
        let bar = &v["output_bar"];
        assert_eq!(bar["rect"]["height"], json!(32.0), "{w}: the menu row is 32 points tall");
        if let Some(t) = transitions {
            assert_eq!(bar["transitions"], json!(t), "{w}");
        }
        let parts = bar_parts(&a, &ctx);
        let names: Vec<&str> = parts.iter().map(|(n, _)| n.as_str()).collect();
        for want in ["menus", "mode", "record", "stream", "pause", "pause_menu"] {
            assert!(names.contains(&want), "{w}: {want} missing from {names:?}");
        }
        assert!(names.contains(&"transition") || names.contains(&"transition_overflow"), "{w}: {names:?}");
        for (name, [l, t, r, b]) in &parts {
            assert!(*l >= 0.0 && *r <= w + 0.5 && *t >= 0.0 && *b <= 32.5, "{w}: {name} at {l},{t},{r},{b} is cut off");
            if ["record", "stream", "pause", "pause_menu"].contains(&name.as_str()) {
                assert_eq!(b - t, 28.0, "{w}: {name} is 28 points tall");
                assert!((t - 2.0).abs() <= 0.5, "{w}: {name} starts 2 points below the top, at {t}");
            }
            if name == "menus" {
                assert_eq!(b - t, 32.0, "{w}: the menus fill the 32 point row");
            }
        }
        // From left to right, nothing overlaps.
        for pair in parts.windows(2) {
            let ((a_name, a), (b_name, b)) = (&pair[0], &pair[1]);
            assert!(a[2] <= b[0] + 0.5, "{w}: {a_name} ends at {} after {b_name} starts at {}", a[2], b[0]);
        }
        let end = |part: &str| parts.iter().find(|(n, _)| n == part).map(|(_, r)| (r[0], r[2])).unwrap_or_default();
        let (stream, (face_start, face_end), (arrow_start, arrow_end)) = (end("stream").1, end("pause"), end("pause_menu"));
        assert!((face_start - stream - 8.0).abs() <= 1.0, "{w}: Pause starts 8 points after Go Live, at {face_start}");
        assert!((arrow_start - face_end).abs() <= 0.5, "{w}: the arrow is joined to the Pause face");
        assert!((w - arrow_end - 8.0).abs() <= 1.0, "{w}: the Pause arrow ends 8 points from the right edge, at {arrow_end}");
    }
}

#[test]
fn the_docks_menu_shows_and_hides_the_output_bar_groups() {
    let mut a = app();
    let ctx = egui::Context::default();
    let names = |a: &StudioApp| bar_parts(a, &ctx).into_iter().map(|(n, _)| n).collect::<Vec<_>>();
    frame(&mut a, &ctx, vec![]);
    assert_eq!(run(&mut a, &ctx, "dock.transitions", json!({})).unwrap(), json!({"on": false}));
    frame(&mut a, &ctx, vec![]);
    assert_eq!(a.ui_inspect(&ctx)["output_bar"]["transitions"], json!("hidden"));
    assert_eq!(names(&a), ["menus", "mode", "record", "stream", "pause", "pause_menu"]);
    assert_eq!(run(&mut a, &ctx, "dock.controls", json!({})).unwrap(), json!({"on": false}));
    frame(&mut a, &ctx, vec![]);
    assert_eq!(names(&a), ["menus"]);
    run(&mut a, &ctx, "dock.transitions", json!({})).unwrap();
    frame(&mut a, &ctx, vec![]);
    // Fade has settings (its Easing), so its settings button shows too.
    assert_eq!(names(&a), ["menus", "transition", "duration", "transition_settings"]);
    run(&mut a, &ctx, "dock.reset", json!({})).unwrap();
    frame(&mut a, &ctx, vec![]);
    assert!(names(&a).contains(&"record".to_string()));
    // The docks under the preview are only Scenes, Sources and Audio Mixer.
    let docks: Vec<Value> = a.ui_inspect(&ctx)["docks"].as_array().cloned().unwrap_or_default().into_iter().map(|d| d["dock"].clone()).collect();
    assert_eq!(docks, [json!("scenes"), json!("sources"), json!("mixer")]);
}

#[test]
fn the_application_audio_dock_reads_its_lanes_again_when_the_collection_changes() {
    let mut a = app();
    let ctx = egui::Context::default();
    let names = |a: &StudioApp| -> Vec<String> { a.lanes.lanes().map(|l| l.lanes.iter().map(|l| l.name.clone()).collect()).unwrap_or_default() };
    let now = std::time::Instant::now();
    a.lanes.refresh(&a.studio, now);
    assert!(names(&a).contains(&"Microphone".to_string()), "{:?}", names(&a));
    run(&mut a, &ctx, "source.rename", json!({"source": "Microphone", "name": "Desk mic"})).unwrap();
    // The same instant: only the collection's change makes the lanes due.
    a.lanes.refresh(&a.studio, now);
    assert!(names(&a).contains(&"Desk mic".to_string()), "{:?}", names(&a));
}

#[test]
fn the_application_audio_dock_is_hidden_until_the_docks_menu_shows_it() {
    let mut a = app();
    let ctx = egui::Context::default();
    let docks = |a: &mut StudioApp| -> Vec<Value> {
        a.ui_inspect(&ctx)["docks"].as_array().cloned().unwrap_or_default().into_iter().map(|d| d["dock"].clone()).collect()
    };
    frame(&mut a, &ctx, vec![]);
    assert!(!docks(&mut a).contains(&json!("app_audio")));
    assert_eq!(run(&mut a, &ctx, "dock.app_audio", json!({})).unwrap(), json!({"on": true}));
    for _ in 0..2 {
        frame(&mut a, &ctx, vec![]);
    }
    assert_eq!(docks(&mut a), [json!("scenes"), json!("sources"), json!("mixer"), json!("app_audio")]);
    // The new collection's microphone is its first lane, heard by the program.
    let lanes = run(&mut a, &ctx, "app_audio.lanes", json!({})).unwrap();
    assert_eq!(lanes["lanes"][0]["kind"], json!("microphone"), "{lanes}");
    assert_eq!(lanes["lanes"][0]["routed"], json!(true), "{lanes}");
    assert_eq!(run(&mut a, &ctx, "dock.app_audio", json!({})).unwrap(), json!({"on": false}));
    frame(&mut a, &ctx, vec![]);
    assert!(!docks(&mut a).contains(&json!("app_audio")));
}

fn rect_of(v: &Value) -> egui::Rect {
    let f = |k: &str| v[k].as_f64().unwrap_or(f64::NAN) as f32;
    egui::Rect::from_min_size(egui::pos2(f("x"), f("y")), egui::vec2(f("width"), f("height")))
}

/// A drag from `from` to `to`, one step per frame: the pointer arrives, presses, moves and releases.
fn drag(from: egui::Pos2, to: egui::Pos2) -> [Vec<Event>; 4] {
    let b = |at, pressed| Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
    [vec![Event::PointerMoved(from)], vec![b(from, true)], vec![Event::PointerMoved(to)], vec![b(to, false)]]
}

fn frame_with(app: &mut StudioApp, ctx: &egui::Context, size: egui::Vec2, events: Vec<Event>) {
    let raw = RawInput { events, screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)), ..RawInput::default() };
    let mut out = ctx.run_ui(raw, |ui| app.frame(ui));
    out.textures_delta.clear();
}

#[test]
fn the_dock_strip_is_200_tall_at_1280_and_the_canvas_takes_the_rest() {
    let mut a = app();
    let ctx = egui::Context::default();
    for _ in 0..3 {
        frame_at(&mut a, &ctx, egui::vec2(1280.0, 800.0));
    }
    let v = a.ui_inspect(&ctx);
    let strip = rect_of(&v["dock_strip"]["rect"]);
    assert_eq!(strip.height(), 200.0, "{strip:?}");
    assert_eq!(strip.bottom(), 800.0 - 22.0, "the 22 point status bar is under the strip");
    let preview = rect_of(&v["preview"]["rect"]);
    assert!((preview.width() - 956.0).abs() <= 1.0 && (preview.height() - 538.0).abs() <= 1.0, "the canvas is {preview:?}");
    let docks = v["docks"].as_array().cloned().unwrap_or_default();
    let widths: Vec<f32> = docks.iter().map(|d| rect_of(&d["rect"]).width()).collect();
    assert_eq!(widths, [300.0, 340.0, 638.0]);
    for d in &docks {
        let h = rect_of(&d["header"]);
        assert_eq!(h.height(), 24.0, "{d}");
        assert_eq!(h.top(), strip.top(), "{d}: the header is at the top of the strip");
    }
}

#[test]
fn every_header_action_is_visible_at_760_by_480() {
    let mut a = app();
    let ctx = egui::Context::default();
    run(&mut a, &ctx, "source.add", json!({"kind": "color", "name": "Back"})).unwrap();
    for _ in 0..3 {
        frame_at(&mut a, &ctx, egui::vec2(760.0, 480.0));
    }
    let v = a.ui_inspect(&ctx);
    let strip = rect_of(&v["dock_strip"]["rect"]);
    assert!((strip.height() - 144.0).abs() < 0.01, "30% of 480: {strip:?}");
    let want = [("scenes", vec!["add", "remove", "up", "down"]), ("sources", vec!["add", "remove", "up", "down", "properties"]), ("mixer", vec![])];
    let docks = v["docks"].as_array().cloned().unwrap_or_default();
    assert_eq!(docks.len(), 3);
    for (d, (name, actions)) in docks.iter().zip(want) {
        assert_eq!(d["dock"], json!(name));
        let dock = rect_of(&d["rect"]);
        let header = rect_of(&d["header"]);
        assert!(dock.width() >= crate::strip::MIN_WIDTH - 0.5, "{name} is {dock:?}");
        let got: Vec<&str> = d["actions"].as_array().map(|a| a.iter().filter_map(|x| x["name"].as_str()).collect()).unwrap_or_default();
        assert_eq!(got, actions, "{name}");
        let rects: Vec<egui::Rect> = d["actions"].as_array().map(|a| a.iter().map(|x| rect_of(&x["rect"])).collect()).unwrap_or_default();
        for (r, n) in rects.iter().zip(&got) {
            assert!(header.contains_rect(*r) && dock.contains_rect(*r), "{name}/{n} at {r:?} is outside {header:?}");
            assert!(r.width() >= 24.0 && r.height() >= 24.0, "{name}/{n} is a 24 point target: {r:?}");
        }
        for pair in rects.windows(2) {
            assert!(pair[0].right() <= pair[1].left(), "{name}: actions overlap: {pair:?}");
        }
    }
}

#[test]
fn dragging_the_strip_edge_and_a_divider_saves_the_sizes_and_reset_clears_them() {
    let mut a = app();
    let ctx = egui::Context::default();
    let size = egui::vec2(1280.0, 800.0);
    for _ in 0..2 {
        frame_at(&mut a, &ctx, size);
    }
    let v = a.ui_inspect(&ctx);
    let top = rect_of(&v["dock_strip"]["rect"]).top();
    for events in drag(egui::pos2(900.0, top + 0.5), egui::pos2(900.0, top + 0.5 - 60.0)) {
        frame_with(&mut a, &ctx, size, events);
    }
    frame_at(&mut a, &ctx, size);
    assert_eq!(a.store().prefs().docks.height, 260.0, "dragging the edge up 60 points makes the strip 260 tall");
    assert_eq!(rect_of(&a.ui_inspect(&ctx)["dock_strip"]["rect"]).height(), 260.0);

    let scenes = rect_of(&a.ui_inspect(&ctx)["docks"][0]["rect"]);
    let y = scenes.center().y;
    for events in drag(egui::pos2(scenes.right() + 0.5, y), egui::pos2(scenes.right() + 40.5, y)) {
        frame_with(&mut a, &ctx, size, events);
    }
    frame_at(&mut a, &ctx, size);
    let d = a.store().prefs().docks;
    assert_eq!((d.scenes_width, d.sources_width), (340.0, 300.0), "the divider moves width from Sources to Scenes");
    let widths: Vec<f32> = a.ui_inspect(&ctx)["docks"].as_array().unwrap_or(&Vec::new()).iter().map(|d| rect_of(&d["rect"]).width()).collect();
    assert_eq!(widths, [340.0, 300.0, 638.0]);

    run(&mut a, &ctx, "dock.reset", json!({})).unwrap();
    let d = a.store().prefs().docks;
    assert_eq!((d.height, d.scenes_width, d.sources_width), (0.0, 0.0, 0.0));
    frame_at(&mut a, &ctx, size);
    assert_eq!(rect_of(&a.ui_inspect(&ctx)["dock_strip"]["rect"]).height(), 200.0);
}

fn dock_order(a: &StudioApp, ctx: &egui::Context) -> Vec<String> {
    let v = a.ui_inspect(ctx);
    v["docks"].as_array().unwrap_or(&Vec::new()).iter().filter_map(|d| d["dock"].as_str().map(str::to_string)).collect()
}

#[test]
fn dock_move_rearranges_the_strip_and_reset_puts_it_back() {
    let mut a = app();
    let ctx = egui::Context::default();
    let size = egui::vec2(1280.0, 800.0);
    frame_at(&mut a, &ctx, size);
    let r = run(&mut a, &ctx, "dock.move", json!({"dock": "mixer", "to": "left"})).unwrap();
    // The order lists every dock, the hidden Application Audio dock included.
    assert_eq!(r, json!({"moved": true, "order": ["scenes", "mixer", "sources", "app_audio"]}));
    frame_at(&mut a, &ctx, size);
    assert_eq!(dock_order(&a, &ctx), ["scenes", "mixer", "sources"]);
    let widths: Vec<f32> = a.ui_inspect(&ctx)["docks"].as_array().unwrap_or(&Vec::new()).iter().map(|d| rect_of(&d["rect"]).width()).collect();
    assert_eq!(widths, [300.0, 400.0, 578.0], "Sources is last and takes the rest");
    let before = a.store().prefs().docks;
    let r = run(&mut a, &ctx, "dock.move", json!({"dock": "scenes", "to": "left"})).unwrap();
    assert_eq!(r["moved"], json!(false), "the first dock cannot move left");
    assert_eq!(a.store().prefs().docks, before, "nothing changed");
    // A hidden dock is passed over.
    run(&mut a, &ctx, "dock.mixer", json!({})).unwrap();
    run(&mut a, &ctx, "dock.move", json!({"dock": "scenes", "to": "right"})).unwrap();
    run(&mut a, &ctx, "dock.mixer", json!({})).unwrap();
    frame_at(&mut a, &ctx, size);
    assert_eq!(dock_order(&a, &ctx), ["mixer", "sources", "scenes"]);
    for bad in [json!({"dock": "stats", "to": "left"}), json!({"dock": "mixer", "to": "up"}), json!({"dock": "mixer"}), json!({"to": "left"})] {
        assert!(run(&mut a, &ctx, "dock.move", bad.clone()).is_err(), "{bad}");
    }
    run(&mut a, &ctx, "dock.reset", json!({})).unwrap();
    frame_at(&mut a, &ctx, size);
    assert_eq!(dock_order(&a, &ctx), ["scenes", "sources", "mixer"]);
}

/// A right-click at `at`, one step per frame.
fn right_click(at: egui::Pos2) -> [Vec<Event>; 3] {
    let b = |pressed| Event::PointerButton { pos: at, button: egui::PointerButton::Secondary, pressed, modifiers: Modifiers::NONE };
    [vec![Event::PointerMoved(at)], vec![b(true)], vec![b(false)]]
}

#[test]
fn a_right_click_on_a_dock_title_opens_the_dock_menu() {
    let mut a = app();
    let ctx = egui::Context::default();
    let size = egui::vec2(1280.0, 800.0);
    for _ in 0..2 {
        frame_at(&mut a, &ctx, size);
    }
    let v = a.ui_inspect(&ctx);
    let title = rect_of(&v["docks"][1]["title"]);
    let body = rect_of(&v["docks"][1]["rect"]);
    assert!(title.width() > 0.0 && body.contains(title.center()), "{title:?} in {body:?}");
    for events in right_click(egui::pos2(body.center().x, body.bottom() - 10.0)) {
        frame_with(&mut a, &ctx, size, events);
    }
    assert!(!egui::Popup::is_any_open(&ctx), "a right-click below the header opens no dock menu");
    for events in right_click(title.center()) {
        frame_with(&mut a, &ctx, size, events);
    }
    frame_at(&mut a, &ctx, size);
    assert!(egui::Popup::is_any_open(&ctx), "the dock menu is open");
}

fn names(v: &Value) -> Vec<String> {
    v.as_array().unwrap_or(&Vec::new()).iter().filter_map(|d| d["dock"].as_str().map(str::to_string)).collect()
}

#[test]
fn a_saved_dock_layout_puts_every_dock_back_as_it_was() {
    let mut a = app();
    let ctx = egui::Context::default();
    let size = egui::vec2(1280.0, 800.0);
    run(&mut a, &ctx, "dock.mixer", json!({})).unwrap();
    run(&mut a, &ctx, "dock.move", json!({"dock": "scenes", "to": "right"})).unwrap();
    run(&mut a, &ctx, "dock.float", json!({"dock": "sources", "floating": true})).unwrap();
    a.store.update(|p| {
        p.docks.place_window(1, [400.0, 250.0], [360.0, 240.0]);
    });
    for _ in 0..2 {
        frame_at(&mut a, &ctx, size);
    }
    let r = run(&mut a, &ctx, "dock.layout_save", json!({"name": "Talk"})).unwrap();
    assert_eq!(r, json!({"saved": "Talk", "replaced": false}));
    let saved = a.store().prefs().docks;

    run(&mut a, &ctx, "dock.reset", json!({})).unwrap();
    for _ in 0..2 {
        frame_at(&mut a, &ctx, size);
    }
    assert_eq!(names(&a.ui_inspect(&ctx)["docks"]), ["scenes", "sources", "mixer"]);

    let r = run(&mut a, &ctx, "dock.layout_apply", json!({"name": "talk"})).unwrap();
    assert_eq!(r, json!({"applied": "Talk"}));
    for _ in 0..2 {
        frame_at(&mut a, &ctx, size);
    }
    assert_eq!(a.store().prefs().docks, saved);
    let v = a.ui_inspect(&ctx);
    assert_eq!(names(&v["docks"]), ["scenes"]);
    assert_eq!(names(&v["floating_docks"]), ["sources"]);
    let window = rect_of(&v["floating_docks"][0]["rect"]);
    assert_eq!((window.min, window.size()), (egui::pos2(400.0, 250.0), egui::vec2(360.0, 240.0)));
    assert_eq!(v["dock_layouts"], json!(["Talk"]));
    let state = |a: &StudioApp, id: &str| a.item_state(id).enabled;
    assert!(state(&a, "dock.layout_apply") && state(&a, "dock.layout_remove"));

    // Errors: an unknown layout, a missing or wrong name, and a name the prompt would refuse.
    for (id, bad) in [
        ("dock.layout_apply", json!({"name": "Game"})),
        ("dock.layout_apply", json!({})),
        ("dock.layout_remove", json!({"name": "Game"})),
        ("dock.layout_save", json!({"name": 3})),
        ("dock.layout_save", json!({"name": ""})),
        ("dock.layout_save", json!({"name": "a/b"})),
    ] {
        assert!(run(&mut a, &ctx, id, bad.clone()).is_err(), "{id} {bad}");
    }
    assert_eq!(a.store().prefs().dock_layout_names(), ["Talk"]);

    // Without a name the window asks for one.
    let r = run(&mut a, &ctx, "dock.layout_save", json!({})).unwrap();
    assert_eq!(r, json!({"asking": true}));
    assert_eq!(a.ui_inspect(&ctx)["dialogs"]["prompt"]["title"], json!("Save Dock Layout"));
    a.answer_dialog(&ctx, true, Some("Game")).unwrap();
    assert_eq!(a.store().prefs().dock_layout_names(), ["Talk", "Game"]);

    // The menu reaches each layout, also two whose names differ only by an ellipsis.
    for name in ["x\u{2026}y", "x...y"] {
        run(&mut a, &ctx, "dock.layout_save", json!({ "name": name })).unwrap();
    }
    for name in ["x\u{2026}y", "x...y", "Game"] {
        let cmd = a.menu_command(&format!("Docks > Layouts > {name}")).unwrap();
        assert_eq!((cmd.id.as_str(), cmd.params), ("dock.layout_apply", json!({ "name": name })));
    }
    for name in ["x\u{2026}y", "x...y"] {
        run(&mut a, &ctx, "dock.layout_remove", json!({ "name": name })).unwrap();
    }

    let r = run(&mut a, &ctx, "dock.layout_remove", json!({"name": "TALK"})).unwrap();
    assert_eq!(r, json!({"removed": "TALK"}));
    run(&mut a, &ctx, "dock.layout_remove", json!({"name": "Game"})).unwrap();
    assert_eq!(a.ui_inspect(&ctx)["dock_layouts"], json!([]));
    assert!(!state(&a, "dock.layout_apply") && !state(&a, "dock.layout_remove"));
}

#[test]
fn dock_float_moves_a_dock_into_a_window_and_back() {
    let mut a = app();
    let ctx = egui::Context::default();
    let size = egui::vec2(1280.0, 800.0);
    frame_at(&mut a, &ctx, size);
    let r = run(&mut a, &ctx, "dock.float", json!({"dock": "scenes", "floating": true})).unwrap();
    assert_eq!(r, json!({"floating": true, "changed": true}));
    for _ in 0..2 {
        frame_at(&mut a, &ctx, size);
    }
    let v = a.ui_inspect(&ctx);
    assert_eq!(names(&v["docks"]), ["sources", "mixer"]);
    assert_eq!(names(&v["floating_docks"]), ["scenes"]);
    let widths: Vec<f32> = v["docks"].as_array().unwrap_or(&Vec::new()).iter().map(|d| rect_of(&d["rect"]).width()).collect();
    assert_eq!(widths, [340.0, 939.0], "the strip lays out the docks left in it");
    let window = rect_of(&v["floating_docks"][0]["rect"]);
    assert!(window.width() >= 200.0 && window.height() >= 120.0, "{window:?}");
    assert!(egui::Rect::from_min_size(egui::Pos2::ZERO, size).contains_rect(window), "{window:?}");
    let saved = a.store().prefs().docks.window_pos[0];
    assert_eq!(saved, [window.min.x, window.min.y], "the window's place is saved");

    let r = run(&mut a, &ctx, "dock.float", json!({"dock": "scenes", "floating": true})).unwrap();
    assert_eq!(r["changed"], json!(false));
    let r = run(&mut a, &ctx, "dock.move", json!({"dock": "scenes", "to": "right"})).unwrap();
    assert_eq!(r["moved"], json!(false), "a dock in a window does not move in the strip");
    for bad in [json!({"dock": "scenes"}), json!({"dock": "scenes", "floating": "yes"}), json!({"dock": "stats", "floating": true})] {
        assert!(run(&mut a, &ctx, "dock.float", bad.clone()).is_err(), "{bad}");
    }

    // A hidden floating dock draws no window, and shows in it again.
    run(&mut a, &ctx, "dock.scenes", json!({})).unwrap();
    frame_at(&mut a, &ctx, size);
    assert!(names(&a.ui_inspect(&ctx)["floating_docks"]).is_empty());
    run(&mut a, &ctx, "dock.scenes", json!({})).unwrap();
    frame_at(&mut a, &ctx, size);
    assert_eq!(names(&a.ui_inspect(&ctx)["floating_docks"]), ["scenes"]);

    run(&mut a, &ctx, "dock.float", json!({"dock": "scenes", "floating": false})).unwrap();
    frame_at(&mut a, &ctx, size);
    let v = a.ui_inspect(&ctx);
    assert_eq!(names(&v["docks"]), ["scenes", "sources", "mixer"]);
    assert!(names(&v["floating_docks"]).is_empty());
}

#[test]
fn every_dock_can_float_and_reset_puts_them_back() {
    let mut a = app();
    let ctx = egui::Context::default();
    let size = egui::vec2(1280.0, 800.0);
    for dock in ["scenes", "sources", "mixer"] {
        run(&mut a, &ctx, "dock.float", json!({"dock": dock, "floating": true})).unwrap();
    }
    for _ in 0..2 {
        frame_at(&mut a, &ctx, size);
    }
    let v = a.ui_inspect(&ctx);
    assert!(v["dock_strip"].is_null(), "no strip without docks in it");
    assert_eq!(names(&v["floating_docks"]), ["scenes", "sources", "mixer"]);
    run(&mut a, &ctx, "dock.reset", json!({})).unwrap();
    frame_at(&mut a, &ctx, size);
    let v = a.ui_inspect(&ctx);
    assert_eq!(names(&v["docks"]), ["scenes", "sources", "mixer"]);
    assert!(names(&v["floating_docks"]).is_empty());
    assert_eq!(a.store().prefs().docks, crate::store::Docks::default());
}

#[test]
fn dragging_a_floating_dock_by_its_header_moves_its_window_and_saves_the_place() {
    let mut a = app();
    let ctx = egui::Context::default();
    let size = egui::vec2(1280.0, 800.0);
    run(&mut a, &ctx, "dock.float", json!({"dock": "mixer", "floating": true})).unwrap();
    for _ in 0..2 {
        frame_at(&mut a, &ctx, size);
    }
    let v = a.ui_inspect(&ctx);
    let before = rect_of(&v["floating_docks"][0]["rect"]);
    let header = rect_of(&v["floating_docks"][0]["title"]);
    let from = header.center();
    for events in drag(from, from + egui::vec2(200.0, 150.0)) {
        frame_with(&mut a, &ctx, size, events);
    }
    frame_at(&mut a, &ctx, size);
    let after = rect_of(&a.ui_inspect(&ctx)["floating_docks"][0]["rect"]);
    assert_eq!(after.min, before.min + egui::vec2(200.0, 150.0), "{before:?} -> {after:?}");
    assert_eq!(a.store().prefs().docks.window_pos[2], [after.min.x, after.min.y]);

    // A drag that starts in the dock's body does not move the window.
    let from = egui::pos2(after.center().x, after.bottom() - 12.0);
    for events in drag(from, from + egui::vec2(-150.0, -100.0)) {
        frame_with(&mut a, &ctx, size, events);
    }
    frame_at(&mut a, &ctx, size);
    assert_eq!(rect_of(&a.ui_inspect(&ctx)["floating_docks"][0]["rect"]).min, after.min);

    // The title that moves the window still opens the dock's menu.
    let title = rect_of(&a.ui_inspect(&ctx)["floating_docks"][0]["title"]);
    assert!(!egui::Popup::is_any_open(&ctx));
    for events in right_click(title.center()) {
        frame_with(&mut a, &ctx, size, events);
    }
    frame_at(&mut a, &ctx, size);
    assert!(egui::Popup::is_any_open(&ctx), "the floating dock's menu is open");

    // A new window, as after a restart, opens where the dock was moved to.
    let mut b = app();
    let docks = a.store().prefs().docks;
    b.store.update(|p| p.docks = docks);
    let ctx = egui::Context::default();
    for _ in 0..2 {
        frame_at(&mut b, &ctx, size);
    }
    assert_eq!(rect_of(&b.ui_inspect(&ctx)["floating_docks"][0]["rect"]).min, after.min);
}

#[test]
fn a_smaller_main_window_does_not_change_a_floating_dock_s_saved_place() {
    let mut a = app();
    let ctx = egui::Context::default();
    a.store.update(|p| {
        p.docks.floating[2] = true;
        p.docks.place_window(2, [900.0, 500.0], [300.0, 200.0]);
    });
    let big = egui::vec2(1280.0, 800.0);
    for _ in 0..2 {
        frame_at(&mut a, &ctx, big);
    }
    assert_eq!(rect_of(&a.ui_inspect(&ctx)["floating_docks"][0]["rect"]).min, egui::pos2(900.0, 500.0));
    let small = egui::vec2(900.0, 600.0);
    for _ in 0..3 {
        frame_at(&mut a, &ctx, small);
    }
    let pushed = rect_of(&a.ui_inspect(&ctx)["floating_docks"][0]["rect"]);
    assert!(egui::Rect::from_min_size(egui::Pos2::ZERO, small).contains_rect(pushed), "egui keeps it on screen: {pushed:?}");
    assert_eq!(a.store().prefs().docks.window_pos(2), Some([900.0, 500.0]), "the saved place is kept");
    for _ in 0..3 {
        frame_at(&mut a, &ctx, big);
    }
    assert_eq!(rect_of(&a.ui_inspect(&ctx)["floating_docks"][0]["rect"]).min, egui::pos2(900.0, 500.0), "it goes back");

    // The same after a restart in the smaller main window, with a resize while it is small.
    let mut b = app();
    let docks = a.store().prefs().docks;
    b.store.update(|p| p.docks = docks);
    let ctx_b = egui::Context::default();
    for _ in 0..3 {
        frame_at(&mut b, &ctx_b, small);
    }
    let pushed = rect_of(&b.ui_inspect(&ctx_b)["floating_docks"][0]["rect"]);
    let corner = pushed.max - egui::vec2(2.0, 2.0);
    for events in drag(corner, corner - egui::vec2(20.0, 20.0)) {
        frame_with(&mut b, &ctx_b, small, events);
    }
    for _ in 0..2 {
        frame_at(&mut b, &ctx_b, small);
    }
    assert_eq!(b.store().prefs().docks.window_pos(2), Some([900.0, 500.0]), "a resize keeps the saved place");
    for _ in 0..3 {
        frame_at(&mut b, &ctx_b, big);
    }
    assert_eq!(rect_of(&b.ui_inspect(&ctx_b)["floating_docks"][0]["rect"]).min, egui::pos2(900.0, 500.0), "it goes back after a restart");

    // A window larger than the main window keeps its saved size too.
    run(&mut a, &ctx, "dock.reset", json!({})).unwrap();
    a.store.update(|p| {
        p.docks.floating[2] = true;
        p.docks.place_window(2, [100.0, 100.0], [800.0, 500.0]);
    });
    for _ in 0..3 {
        frame_at(&mut a, &ctx, egui::vec2(600.0, 400.0));
    }
    let squeezed = rect_of(&a.ui_inspect(&ctx)["floating_docks"][0]["rect"]);
    assert_eq!(a.store().prefs().docks.window_size[2], [800.0, 500.0], "drawn at {squeezed:?}");
    // A click inside the squeezed dock is not a resize.
    for events in drag(squeezed.center(), squeezed.center()) {
        frame_with(&mut a, &ctx, egui::vec2(600.0, 400.0), events);
    }
    frame_at(&mut a, &ctx, egui::vec2(600.0, 400.0));
    assert_eq!(a.store().prefs().docks.window_size[2], [800.0, 500.0]);
}

#[test]
fn dragging_a_floating_dock_s_corner_resizes_it_and_saves_the_size() {
    let mut a = app();
    let ctx = egui::Context::default();
    let size = egui::vec2(1280.0, 800.0);
    a.store.update(|p| {
        p.docks.floating[0] = true;
        p.docks.place_window(0, [200.0, 150.0], [300.0, 200.0]);
    });
    for _ in 0..2 {
        frame_at(&mut a, &ctx, size);
    }
    let before = rect_of(&a.ui_inspect(&ctx)["floating_docks"][0]["rect"]);
    let corner = before.max - egui::vec2(2.0, 2.0);
    for events in drag(corner, corner + egui::vec2(60.0, 40.0)) {
        frame_with(&mut a, &ctx, size, events);
    }
    frame_at(&mut a, &ctx, size);
    let after = rect_of(&a.ui_inspect(&ctx)["floating_docks"][0]["rect"]);
    assert_eq!(after.size(), before.size() + egui::vec2(60.0, 40.0), "{before:?} -> {after:?}");
    assert_eq!(a.store().prefs().docks.window_size[0], [after.width(), after.height()]);
    assert_eq!(a.store().prefs().docks.window_pos(0), Some([200.0, 150.0]));

    // A resize inward by more than the edge's grab width is saved too.
    let corner = after.max - egui::vec2(2.0, 2.0);
    for events in drag(corner, corner - egui::vec2(40.0, 30.0)) {
        frame_with(&mut a, &ctx, size, events);
    }
    for _ in 0..2 {
        frame_at(&mut a, &ctx, size);
    }
    let smaller = rect_of(&a.ui_inspect(&ctx)["floating_docks"][0]["rect"]);
    assert_eq!(smaller.size(), after.size() - egui::vec2(40.0, 30.0), "{after:?} -> {smaller:?}");
    assert_eq!(a.store().prefs().docks.window_size[0], [smaller.width(), smaller.height()]);
    let after = smaller;

    // A drag on the left edge does not move the window's corner.
    let edge = egui::pos2(after.left() + 1.0, after.center().y);
    for events in drag(edge, edge - egui::vec2(50.0, 0.0)) {
        frame_with(&mut a, &ctx, size, events);
    }
    for _ in 0..2 {
        frame_at(&mut a, &ctx, size);
    }
    assert_eq!(rect_of(&a.ui_inspect(&ctx)["floating_docks"][0]["rect"]).min, after.min);
    assert_eq!(a.store().prefs().docks.window_pos(0), Some([200.0, 150.0]));
}

#[test]
fn a_floating_dock_opens_at_its_saved_size_and_keeps_it() {
    let mut a = app();
    let ctx = egui::Context::default();
    let size = egui::vec2(1280.0, 800.0);
    a.store.update(|p| {
        p.docks.floating[1] = true;
        p.docks.place_window(1, [300.0, 200.0], [520.0, 330.0]);
    });
    for _ in 0..4 {
        frame_at(&mut a, &ctx, size);
    }
    let window = rect_of(&a.ui_inspect(&ctx)["floating_docks"][0]["rect"]);
    assert_eq!(window.min, egui::pos2(300.0, 200.0));
    assert_eq!(window.size(), egui::vec2(520.0, 330.0));
    assert_eq!(a.store().prefs().docks.window_size[1], [520.0, 330.0], "the saved size does not drift from frame to frame");

    // A floating window made before any size was saved saves the size it opened at.
    run(&mut a, &ctx, "dock.float", json!({"dock": "scenes", "floating": true})).unwrap();
    for _ in 0..2 {
        frame_at(&mut a, &ctx, size);
    }
    let saved = a.store().prefs().docks.window_size[0];
    assert_eq!(saved, [crate::strip::DEFAULT_WIDTHS[0], crate::strip::DEFAULT_HEIGHT]);
    for _ in 0..2 {
        frame_at(&mut a, &ctx, size);
    }
    assert_eq!(a.store().prefs().docks.window_size[0], saved);
}

#[test]
fn a_press_on_a_header_action_under_the_strip_edge_runs_the_action() {
    let mut a = app();
    let ctx = egui::Context::default();
    let size = egui::vec2(1280.0, 800.0);
    for _ in 0..2 {
        frame_at(&mut a, &ctx, size);
    }
    let before = a.studio().collection().scenes.len();
    let v = a.ui_inspect(&ctx);
    let strip = rect_of(&v["dock_strip"]["rect"]);
    let add = v["docks"][0]["actions"].as_array().and_then(|x| x.iter().find(|x| x["name"] == json!("add"))).map(|x| rect_of(&x["rect"]));
    let Some(add) = add else { panic!("no Scenes Add action: {}", v["docks"][0]) };
    assert_eq!(add.top(), strip.top(), "the Add button starts at the top of the strip");
    let at = egui::pos2(add.center().x, add.top() + 1.0);
    let b = |pressed| Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
    for events in [vec![Event::PointerMoved(at)], vec![b(true)], vec![b(false)], vec![]] {
        frame_with(&mut a, &ctx, size, events);
    }
    assert_eq!(a.studio().collection().scenes.len(), before + 1, "the press 1 point below the top of Add adds a scene");
    assert_eq!(a.store().prefs().docks.height, 0.0, "the strip keeps its default height");
    assert_eq!(rect_of(&a.ui_inspect(&ctx)["dock_strip"]["rect"]).height(), 200.0);

    // A press on Add that moves up does not resize the strip either.
    for events in drag(at, egui::pos2(at.x, at.y - 60.0)) {
        frame_with(&mut a, &ctx, size, events);
    }
    frame_at(&mut a, &ctx, size);
    assert_eq!(a.store().prefs().docks.height, 0.0, "a drag that starts on Add leaves the strip's height");
    assert_eq!(rect_of(&a.ui_inspect(&ctx)["dock_strip"]["rect"]).height(), 200.0);
}

#[test]
fn a_hidden_dock_gives_its_width_to_the_others() {
    let mut a = app();
    let ctx = egui::Context::default();
    run(&mut a, &ctx, "dock.sources", json!({})).unwrap();
    for _ in 0..2 {
        frame_at(&mut a, &ctx, egui::vec2(1280.0, 800.0));
    }
    let v = a.ui_inspect(&ctx);
    let widths: Vec<f32> = v["docks"].as_array().unwrap_or(&Vec::new()).iter().map(|d| rect_of(&d["rect"]).width()).collect();
    assert_eq!(widths, [300.0, 979.0]);
    for name in ["dock.scenes", "dock.mixer"] {
        run(&mut a, &ctx, name, json!({})).unwrap();
    }
    frame_at(&mut a, &ctx, egui::vec2(1280.0, 800.0));
    let v = a.ui_inspect(&ctx);
    assert_eq!(v["dock_strip"], Value::Null, "no strip without docks");
    let preview = rect_of(&v["preview"]["rect"]);
    assert!(preview.height() > 538.0, "the canvas takes the strip's room: {preview:?}");
}

#[test]
fn the_output_bar_runs_the_output_commands() {
    let mut a = app();
    let ctx = egui::Context::default();
    frame(&mut a, &ctx, vec![]);
    let mode = |a: &StudioApp| bar_parts(a, &ctx).into_iter().find(|(n, _)| n == "mode").map(|(_, r)| r).unwrap_or([0.0; 4]);
    let click = |at: egui::Pos2| {
        let b = |pressed| Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
        vec![Event::PointerMoved(at), b(true), b(false)]
    };
    // The Studio segment turns Studio Mode on, and the Edit segment turns it off again.
    let p = mode(&a);
    frame(&mut a, &ctx, click(egui::pos2(p[2] - 4.0, (p[1] + p[3]) / 2.0)));
    frame(&mut a, &ctx, vec![]);
    assert!(a.studio().engine().studio_mode(), "a click on Studio turns Studio Mode on");
    let p = mode(&a);
    frame(&mut a, &ctx, click(egui::pos2(p[0] + 4.0, (p[1] + p[3]) / 2.0)));
    frame(&mut a, &ctx, vec![]);
    assert!(!a.studio().engine().studio_mode(), "a click on Edit turns it off");
}

#[test]
fn selection_commands_need_a_selection() {
    let mut a = app();
    let ctx = egui::Context::default();
    assert!(run(&mut a, &ctx, "selection.fit", json!({})).is_err());
    let r = run(&mut a, &ctx, "source.add", json!({"kind": "color", "name": "Back"})).unwrap();
    let item = ItemId(r["item"].as_u64().unwrap());
    assert_eq!(a.hud()["ui"]["selected_item"], json!(item));
    let source = a.studio().collection().scenes[0].items[0].source;
    wait_shown(&a, source);
    run(&mut a, &ctx, "selection.rotate_cw", json!({})).unwrap_or_else(|e| panic!("{e}"));
    let rot = |a: &StudioApp| a.studio().collection().scenes[0].items[0].transform.rotation;
    assert_eq!(rot(&a), 90.0);
    run(&mut a, &ctx, "selection.rotate_ccw", json!({})).unwrap();
    assert_eq!(rot(&a), 0.0);
    run(&mut a, &ctx, "selection.remove", json!({})).unwrap();
    assert!(a.studio().collection().scenes[0].items.is_empty());
    assert_eq!(a.hud()["ui"]["selected_item"], Value::Null);
}

#[test]
fn adding_a_source_opens_its_properties() {
    let mut a = app();
    let ctx = egui::Context::default();
    a.act(&ctx, Act::AddSource { kind: "text".into() });
    let hud = a.hud();
    assert_eq!(hud["ui"]["properties_open"].as_array().map(Vec::len), Some(1));
    frame(&mut a, &ctx, vec![]);
    let name = &a.studio().collection().sources.last().unwrap().name;
    assert_eq!(name, "Text");
}

#[test]
fn unknown_commands_and_links_are_errors() {
    let mut a = app();
    let ctx = egui::Context::default();
    assert!(run(&mut a, &ctx, "nope.nothing", json!({})).is_err());
    assert!(run(&mut a, &ctx, "app.links", json!({"link": "elsewhere"})).is_err());
    assert!(run(&mut a, &ctx, "help.unknown", json!({})).unwrap_err().contains("unknown command"));
    assert!(a.hud()["ui"]["notices"].as_array().is_some_and(|n| !n.is_empty()));
}

#[test]
fn about_draws_credits_and_opens_the_licenses() {
    let mut a = app();
    let ctx = egui::Context::default();
    assert_eq!(a.hud()["ui"]["about_open"], json!(false));
    run(&mut a, &ctx, "app.about", json!({})).unwrap();
    for mode in credits::NameMode::ALL {
        a.credit_names = mode;
        frame(&mut a, &ctx, vec![]);
    }
    assert_eq!(a.hud()["ui"]["about_open"], json!(true));
    assert_eq!(a.hud()["ui"]["licenses_open"], json!(false));
    run(&mut a, &ctx, "app.licenses", json!({})).unwrap();
    assert_eq!(a.hud()["ui"]["licenses_open"], json!(true));
    for l in [credits::License::Apache, credits::License::Inter, credits::License::JetBrainsMono, credits::License::Mit] {
        a.licenses = Some(l);
        frame(&mut a, &ctx, vec![]);
    }
    assert_eq!(a.licenses, Some(credits::License::Mit), "a second frame keeps the window open");
    run(&mut a, &ctx, "app.licenses", json!({})).unwrap();
    assert_eq!(a.licenses, Some(credits::License::Mit), "opening it again keeps the license shown");
    let info = run(&mut a, &ctx, "app.info", json!({})).unwrap();
    assert_eq!(info["contributors"], json!(studiocraft_engine::credits::CONTRIBUTORS.len()));
}

#[test]
fn link_queries_run_in_the_engine() {
    let mut a = app();
    let ctx = egui::Context::default();
    let all = run(&mut a, &ctx, "app.links", json!({})).unwrap();
    assert_eq!(all, studiocraft_engine::links::all());
    let one = run(&mut a, &ctx, "app.links", json!({"link": "issues"})).unwrap();
    assert_eq!(one, json!({"url": studiocraft_engine::links::ISSUES}));
    assert_eq!(run(&mut a, &ctx, "app.info", json!({})).unwrap(), studiocraft_engine::links::info());
    let names = a.collections.clone();
    let bar = commands::menu_bar(&commands::MenuState { collections: &names, collection: "", profiles: &names, profile: "", layouts: &[] });
    let help = bar.menus.into_iter().find(|m| m.title == "Help").unwrap();
    let ids: Vec<String> = help
        .children
        .iter()
        .flat_map(|n| match n {
            craft_menu::Node::Submenu { children, .. } => children.clone(),
            _ => Vec::new(),
        })
        .filter_map(|n| match n {
            craft_menu::Node::Item(i) => Some(i.command.id),
            _ => None,
        })
        .collect();
    assert_eq!(ids, ["help.discord", "help.website", "help.github", "help.issues"]);
}

/// Every text a frame draws, with where it is drawn.
fn texts(out: &egui::FullOutput) -> Vec<(String, egui::Rect)> {
    fn walk(shape: &egui::Shape, found: &mut Vec<(String, egui::Rect)>) {
        match shape {
            egui::Shape::Text(t) => found.push((t.galley.text().to_string(), t.galley.rect.translate(t.pos.to_vec2()))),
            egui::Shape::Vec(v) => v.iter().for_each(|s| walk(s, found)),
            _ => {}
        }
    }
    let mut found = Vec::new();
    out.shapes.iter().for_each(|c| walk(&c.shape, &mut found));
    found
}

/// One frame with `events`; the texts it draws.
fn frame_texts(app: &mut StudioApp, ctx: &egui::Context, events: Vec<Event>) -> Vec<(String, egui::Rect)> {
    let raw = RawInput { events, screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 800.0))), ..RawInput::default() };
    let mut out = ctx.run_ui(raw, |ui| app.frame(ui));
    out.textures_delta.clear();
    texts(&out)
}

/// Click the text `text` drawn in `shown`, one step per frame; the texts of the frame after.
fn click_text(app: &mut StudioApp, ctx: &egui::Context, shown: &[(String, egui::Rect)], text: &str) -> Vec<(String, egui::Rect)> {
    let Some((_, rect)) = shown.iter().rev().find(|(t, _)| t == text) else { panic!("`{text}` is not shown: {shown:?}") };
    let at = rect.center();
    let b = |pressed| Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
    for events in [vec![Event::PointerMoved(at)], vec![b(true)], vec![b(false)]] {
        frame_texts(app, ctx, events);
    }
    frame_texts(app, ctx, vec![])
}

#[test]
fn the_output_tab_sets_the_recording_encoder_s_preset() {
    let mut a = app();
    a.set_language("en");
    let ctx = egui::Context::default();
    a.open_settings(crate::settings::Tab::Output);
    frame_texts(&mut a, &ctx, vec![]);
    let shown = frame_texts(&mut a, &ctx, vec![]);
    assert!(shown.iter().any(|(t, _)| t == "CPU usage preset (faster uses less CPU)"), "{shown:?}");
    assert!(a.settings.as_ref().is_some_and(|w| w.draft.output.encoder_settings.is_empty()), "showing the form saves nothing");

    let shown = click_text(&mut a, &ctx, &shown, "veryfast");
    let shown = click_text(&mut a, &ctx, &shown, "fast");
    click_text(&mut a, &ctx, &shown, "OK");
    assert!(a.settings.is_none(), "OK closes the window");
    assert_eq!(a.studio().profile().output.video_encoder_settings().str("preset"), Some("fast"));
    assert_eq!(a.studio().profile().stream.video_encoder_settings().str("preset"), None, "the stream keeps its own settings");
}

#[test]
fn the_fonts_are_installed_again_after_the_language_changes() {
    let mut a = app();
    a.set_language("en");
    let ctx = egui::Context::default();
    frame(&mut a, &ctx, vec![]);
    assert_eq!(a.fonts_for, None, "a frame installs no fonts before install_fonts");
    a.install_fonts(&ctx);
    assert_eq!(a.fonts_for, Some(craft_i18n::Lang::EN));
    a.set_language("de");
    frame(&mut a, &ctx, vec![]);
    assert_eq!(a.fonts_for.map(|l| i18n::I18N.code(l)), Some("de"));
    a.set_language("en");
    frame(&mut a, &ctx, vec![]);
    assert_eq!(a.fonts_for, Some(craft_i18n::Lang::EN));
}

#[test]
fn arabic_and_hebrew_run_the_window_right_to_left() {
    use craft_ui::direction::is_rtl;
    let mut a = app();
    a.set_language("en");
    let ctx = egui::Context::default();
    frame(&mut a, &ctx, vec![]);
    assert!(!is_rtl(&ctx));
    let menus = |a: &StudioApp| bar_parts(a, &ctx).into_iter().find(|(n, _)| n == "menus").map(|(_, r)| r);
    let Some(ltr) = menus(&a) else { panic!("no menus part") };
    for code in ["ar", "he"] {
        a.set_language(code);
        frame(&mut a, &ctx, vec![]);
        frame(&mut a, &ctx, vec![]);
        assert!(is_rtl(&ctx), "{code}");
        assert!(ctx.global_style().right_to_left, "{code}: egui's own chrome mirrors");
        let Some(rtl) = menus(&a) else { panic!("{code}: no menus part") };
        assert!(rtl[0] < rtl[2], "{code}: {rtl:?}");
        assert!(rtl[2] > ltr[2] + 200.0, "{code}: the menus move to the right end of the bar: {ltr:?} {rtl:?}");
        assert!((1280.0 - rtl[2] - ltr[0]).abs() < 0.5, "{code}: the bar's margin moves to the right: {ltr:?} {rtl:?}");
    }
    a.set_language("en");
    frame(&mut a, &ctx, vec![]);
    assert!(!is_rtl(&ctx));
    assert!(!ctx.global_style().right_to_left);
}

/// A frame on a display of `points` at `ppp` pixels per point.
fn frame_on_display(app: &mut StudioApp, ctx: &egui::Context, points: egui::Vec2, ppp: f32) {
    let info = egui::ViewportInfo { monitor_size: Some(points), native_pixels_per_point: Some(ppp), ..Default::default() };
    let raw = RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 800.0))),
        viewports: std::iter::once((egui::ViewportId::ROOT, info)).collect(),
        ..RawInput::default()
    };
    let mut out = ctx.run_ui(raw, |ui| app.frame(ui));
    out.textures_delta.clear();
}

#[test]
fn the_canvas_takes_the_size_of_the_display_the_window_is_on() {
    let mut a = app();
    a.displays = follow::Watch::new(Arc::new(Vec::new));
    let ctx = egui::Context::default();
    frame(&mut a, &ctx, vec![]);
    let e = run(&mut a, &ctx, "video.canvas_from_display", json!({})).unwrap_err();
    assert!(e.contains("not known"), "{e}");
    assert_eq!(a.studio().canvas(), (1920, 1080), "nothing changed");

    frame_on_display(&mut a, &ctx, egui::vec2(1512.0, 982.0), 2.0);
    a.open_settings(crate::settings::Tab::Video);
    let r = run(&mut a, &ctx, "video.canvas_from_display", json!({})).unwrap();
    assert_eq!(
        r,
        json!({"width": 3024, "height": 1964, "output": {"width": 3024, "height": 1964}, "changed": true, "follows": null}),
        "no listed display has its size"
    );
    assert_eq!(a.studio().canvas(), (3024, 1964));
    let v = a.studio().profile().video;
    assert_eq!((v.output_width, v.output_height), (3024, 1964), "an output at 100% of the canvas follows it");
    let saved = a.store().load_profile(DEFAULT_NAME).unwrap();
    assert_eq!((saved.video.base_width, saved.video.base_height), (3024, 1964), "the profile is saved");
    let draft = &a.settings.as_ref().unwrap().draft.video;
    assert_eq!((draft.base_width, draft.base_height), (3024, 1964), "the open Settings window follows");
    assert_eq!((draft.output_width, draft.output_height), (3024, 1964));

    // The same size again changes nothing, but an edited Settings draft follows it.
    a.settings.as_mut().unwrap().draft.video.base_width = 1280;
    let r = run(&mut a, &ctx, "video.canvas_from_display", json!({})).unwrap();
    assert_eq!(r["changed"], json!(false));
    assert_eq!(a.settings.as_ref().unwrap().draft.video.base_width, 3024);
    // An output size typed into the draft and not yet applied stays.
    a.settings.as_mut().unwrap().draft.video.output_width = 1000;
    run(&mut a, &ctx, "video.canvas_from_display", json!({})).unwrap();
    let draft = &a.settings.as_ref().unwrap().draft.video;
    assert_eq!((draft.output_width, draft.output_height), (1000, 1964), "the unsaved output size stays");
    let listed = run(&mut a, &ctx, "video.displays", json!({})).unwrap();
    assert_eq!(listed["window"], json!({"width": 3024, "height": 1964}));
    assert!(listed["displays"].is_array());

    // An odd side is rounded down to an even one.
    frame_on_display(&mut a, &ctx, egui::vec2(1281.0, 721.0), 1.0);
    let r = run(&mut a, &ctx, "video.canvas_from_display", json!({})).unwrap();
    assert_eq!((r["width"].clone(), r["height"].clone()), (json!(1280), json!(720)));

    // Too large for a canvas.
    frame_on_display(&mut a, &ctx, egui::vec2(5120.0, 2880.0), 2.0);
    let e = run(&mut a, &ctx, "video.canvas_from_display", json!({})).unwrap_err();
    assert!(e.contains("8192"), "{e}");
    assert_eq!(a.studio().canvas(), (1280, 720));
}

#[test]
fn a_filled_source_and_a_scaled_output_follow_a_new_canvas() {
    let mut a = app();
    let ctx = egui::Context::default();
    run(&mut a, &ctx, "source.add", json!({"kind": "color", "name": "Back", "settings": {"width": 400, "height": 300}})).unwrap();
    let source = a.studio().collection().scenes[0].items[0].source;
    wait_shown(&a, source);
    let native = a.studio().source_size(source).unwrap();
    assert_eq!(native, (400, 300));
    let at = |a: &StudioApp| a.studio().collection().scenes[0].items[0].transform;
    run(&mut a, &ctx, "selection.fill", json!({})).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(at(&a), Transform::fill(native, (1920, 1080)));
    assert_eq!(a.studio().collection().scenes[0].items[0].fit, Some(studiocraft_model::FitMode::Fill), "the item keeps its mode");
    let mut p = a.studio().profile().clone();
    (p.video.output_width, p.video.output_height) = (960, 540);
    a.studio.set_profile(p).unwrap();

    frame_on_display(&mut a, &ctx, egui::vec2(1080.0, 1920.0), 1.0);
    let r = run(&mut a, &ctx, "video.canvas_from_display", json!({})).unwrap();
    assert_eq!(r["output"], json!({"width": 540, "height": 960}), "an output at 50% stays at 50%");
    assert_eq!(at(&a), Transform::fill(native, (1080, 1920)), "a filled source fills the new canvas");
    run(&mut a, &ctx, "edit.undo", json!({})).unwrap();
    assert_eq!(at(&a), Transform::fill(native, (1920, 1080)), "one undo step takes the layout back");

    // A typed output size that is no share of the canvas stays.
    let mut p = a.studio().profile().clone();
    (p.video.output_width, p.video.output_height) = (1000, 500);
    a.studio.set_profile(p).unwrap();
    frame_on_display(&mut a, &ctx, egui::vec2(1280.0, 720.0), 1.0);
    let r = run(&mut a, &ctx, "video.canvas_from_display", json!({})).unwrap();
    assert_eq!(r["output"], json!({"width": 1000, "height": 500}));
}

#[test]
fn a_canvas_size_that_cant_be_saved_is_an_error() {
    let dir = temp_dir("canvas-save-fails");
    std::fs::create_dir_all(&dir).unwrap();
    let (store, _) = Store::open(craft_settings::Location::Dir(dir.clone()));
    let mut a = StudioApp::boot(store, None, Vec::new());
    // A file where the profiles folder goes makes the save fail.
    let profiles = dir.join("profiles");
    let _ = std::fs::remove_dir_all(&profiles);
    std::fs::write(&profiles, b"").unwrap();
    let ctx = egui::Context::default();
    run(&mut a, &ctx, "source.add", json!({"kind": "image", "name": "Logo", "transform": {"position": {"x": 960.0, "y": 540.0}}})).unwrap();
    a.studio.mark_saved();
    let old = a.studio().canvas();
    frame_on_display(&mut a, &ctx, egui::vec2(1280.0, 720.0), 1.0);
    let r = run(&mut a, &ctx, "video.canvas_from_display", json!({}));
    // The old canvas and layout come back, and the scene collection has nothing new to save.
    let back = (a.studio().canvas(), a.studio().collection().scenes[0].items[0].transform.position, a.studio().engine().is_dirty());
    // Once the profiles folder can be written, the size is set and saved.
    std::fs::remove_file(&profiles).unwrap();
    let again = run(&mut a, &ctx, "video.canvas_from_display", json!({}));
    let saved = a.store().load_profile(DEFAULT_NAME).map(|p| (p.video.base_width, p.video.base_height));
    let _ = std::fs::remove_dir_all(&dir);
    let e = r.unwrap_err();
    assert!(e.contains("could not be saved"), "{e}");
    assert_eq!(back, (old, Vec2::new(960.0, 540.0), false));
    assert_eq!(again.unwrap()["changed"], json!(true));
    assert_eq!(saved, Ok((1280, 720)));
}

#[test]
fn a_display_size_the_canvas_had_follows_the_display_only_once_saved() {
    let dir = temp_dir("follow-same-size-save-fails");
    std::fs::create_dir_all(&dir).unwrap();
    let (store, _) = Store::open(craft_settings::Location::Dir(dir.clone()));
    let mut a = StudioApp::boot(store, None, Vec::new());
    let old = a.studio().canvas();
    listed(&mut a, vec![screen("2", "Side", old.0, old.1)]);
    let profiles = dir.join("profiles");
    let _ = std::fs::remove_dir_all(&profiles);
    std::fs::write(&profiles, b"").unwrap();
    let ctx = egui::Context::default();
    let r = run(&mut a, &ctx, "video.canvas_from_display", json!({"display": "2"}));
    let followed = a.followed_display().is_some();
    std::fs::remove_file(&profiles).unwrap();
    let again = run(&mut a, &ctx, "video.canvas_from_display", json!({"display": "2"}));
    let _ = std::fs::remove_dir_all(&dir);
    assert!(r.unwrap_err().contains("could not be saved"));
    assert!(!followed, "a failed save follows no display");
    assert_eq!(again.unwrap()["follows"], json!("2"));
}

#[test]
fn a_display_picked_in_settings_is_not_followed_when_the_profile_cant_be_saved() {
    let dir = temp_dir("settings-follow-save-fails");
    std::fs::create_dir_all(&dir).unwrap();
    let (store, _) = Store::open(craft_settings::Location::Dir(dir.clone()));
    let mut a = StudioApp::boot(store, None, Vec::new());
    let old = a.studio().canvas();
    let profiles = dir.join("profiles");
    let _ = std::fs::remove_dir_all(&profiles);
    std::fs::write(&profiles, b"").unwrap();
    a.open_settings(crate::settings::Tab::Video);
    let w = a.settings.as_ref().unwrap();
    let apply = crate::settings::Apply {
        profile: w.draft.clone(),
        overrides: a.keymap.overrides().clone(),
        language: w.language.clone(),
        check_updates: w.check_updates,
        system_title_bar: w.system_title_bar,
        graphics_adapter: w.graphics_adapter.clone(),
        canvas_display: Some(Some(screen("2", "Side", old.0, old.1))),
    };
    a.apply_settings(apply);
    let followed = a.followed_display().is_some();
    let _ = std::fs::remove_file(&profiles);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(a.studio().canvas(), old);
    assert!(!followed, "a failed save follows no display");
}

#[test]
fn a_canvas_size_from_settings_that_cant_be_saved_is_taken_back() {
    let dir = temp_dir("settings-canvas-save-fails");
    std::fs::create_dir_all(&dir).unwrap();
    let (store, _) = Store::open(craft_settings::Location::Dir(dir.clone()));
    let mut a = StudioApp::boot(store, None, Vec::new());
    let profiles = dir.join("profiles");
    let _ = std::fs::remove_dir_all(&profiles);
    std::fs::write(&profiles, b"").unwrap();
    let ctx = egui::Context::default();
    run(&mut a, &ctx, "source.add", json!({"kind": "image", "name": "Logo", "transform": {"position": {"x": 960.0, "y": 540.0}}})).unwrap();
    a.studio.mark_saved();
    a.open_settings(crate::settings::Tab::Video);
    let w = a.settings.as_ref().unwrap();
    let mut profile = w.draft.clone();
    (profile.video.base_width, profile.video.base_height) = (1280, 720);
    let apply = crate::settings::Apply {
        profile,
        overrides: a.keymap.overrides().clone(),
        language: w.language.clone(),
        check_updates: w.check_updates,
        system_title_bar: w.system_title_bar,
        graphics_adapter: w.graphics_adapter.clone(),
        canvas_display: None,
    };
    let took = a.apply_settings(apply);
    let _ = std::fs::remove_file(&profiles);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(!took, "the Settings window stays open with the edits");
    assert_eq!(a.studio().canvas(), (1920, 1080));
    assert_eq!(a.studio().collection().scenes[0].items[0].transform.position, Vec2::new(960.0, 540.0));
    assert!(!a.studio().engine().is_dirty(), "the scene collection has nothing new to save");
}

#[test]
fn the_canvas_from_a_display_refuses_bad_parameters() {
    let mut a = app();
    let ctx = egui::Context::default();
    frame(&mut a, &ctx, vec![]);
    let e = run(&mut a, &ctx, "video.canvas_from_display", json!({"display": 3})).unwrap_err();
    assert!(e.contains("display id"), "{e}");
    let e = run(&mut a, &ctx, "video.canvas_from_display", json!({"display": null})).unwrap_err();
    assert!(e.contains("display id"), "{e}");
    let e = run(&mut a, &ctx, "video.canvas_from_display", json!({"display": "no such display \u{1}/.."})).unwrap_err();
    assert!(e.contains("there is no display"), "{e}");
    assert_eq!(a.studio().canvas(), (1920, 1080));
}

fn screen(id: &str, name: &str, width: u32, height: u32) -> DisplayInfo {
    DisplayInfo { id: id.into(), name: name.into(), width, height, primary: false }
}

/// The app lists `list`, which the test can change.
fn listed(a: &mut StudioApp, list: Vec<DisplayInfo>) -> Arc<std::sync::Mutex<Vec<DisplayInfo>>> {
    let shared = Arc::new(std::sync::Mutex::new(list));
    let theirs = shared.clone();
    a.displays = follow::Watch::new(Arc::new(move || theirs.lock().unwrap().clone()));
    shared
}

/// Let the displays be looked at, at `now`, and wait until that look has ended.
fn look(a: &mut StudioApp, ctx: &egui::Context, now: Instant) {
    a.follow_displays(ctx, now);
    while a.displays.looking() {
        std::thread::yield_now();
        a.follow_displays(ctx, now);
    }
}

/// The display the canvas of the profile in use follows.
fn canvas_link(a: &StudioApp) -> (String, String, u32, u32) {
    a.followed_display().map_or_else(Default::default, |l| (l.profile.clone(), l.id.clone(), l.width, l.height))
}

#[test]
fn the_canvas_follows_the_display_it_came_from() {
    let mut a = app();
    let shown = listed(&mut a, vec![screen("1", "Main", 1920, 1080), screen("2", "Side", 2560, 1440)]);
    let ctx = egui::Context::default();
    let r = run(&mut a, &ctx, "video.canvas_from_display", json!({"display": "2"})).unwrap();
    assert_eq!(r["follows"], json!("2"));
    assert_eq!(canvas_link(&a), (DEFAULT_NAME.to_string(), "2".to_string(), 2560, 1440));
    let mut t = Instant::now();

    // The display's resolution changes.
    shown.lock().unwrap()[1] = screen("2", "Side", 3840, 2160);
    look(&mut a, &ctx, t);
    assert_eq!(a.studio().canvas(), (3840, 2160));
    let saved = a.store().load_profile(DEFAULT_NAME).unwrap();
    assert_eq!((saved.video.base_width, saved.video.base_height), (3840, 2160), "the profile is saved");
    assert_eq!(canvas_link(&a).2, 3840, "and still follows the display");

    // Another display changing does nothing.
    shown.lock().unwrap()[0] = screen("1", "Main", 1280, 720);
    t += follow::LOOK;
    look(&mut a, &ctx, t);
    assert_eq!(a.studio().canvas(), (3840, 2160));

    // A display that is not connected is followed again when it is, under a new id.
    shown.lock().unwrap().pop();
    t += follow::LOOK;
    look(&mut a, &ctx, t);
    assert_eq!(a.studio().canvas(), (3840, 2160));
    assert_eq!(canvas_link(&a).1, "2");
    shown.lock().unwrap().push(screen("9", "Side", 1920, 1200));
    t += follow::LOOK;
    look(&mut a, &ctx, t);
    assert_eq!(a.studio().canvas(), (1920, 1200));
    assert_eq!(canvas_link(&a), (DEFAULT_NAME.to_string(), "9".to_string(), 1920, 1200));

    // An odd side is rounded down to an even one, and a display too large for a canvas is not
    // followed until it fits again.
    shown.lock().unwrap()[1] = screen("9", "Side", 1281, 721);
    t += follow::LOOK;
    look(&mut a, &ctx, t);
    assert_eq!(a.studio().canvas(), (1280, 720));
    shown.lock().unwrap()[1] = screen("9", "Side", 10_000, 5000);
    t += follow::LOOK;
    look(&mut a, &ctx, t);
    assert_eq!(a.studio().canvas(), (1280, 720));
    shown.lock().unwrap()[1] = screen("9", "Side", 2560, 1600);
    t += follow::LOOK;
    look(&mut a, &ctx, t);
    assert_eq!(a.studio().canvas(), (2560, 1600));

    // The displays are not looked at before it is time.
    shown.lock().unwrap()[1] = screen("9", "Side", 1920, 1080);
    a.follow_displays(&ctx, t + follow::LOOK - Duration::from_millis(1));
    assert!(!a.displays.looking());
}

#[test]
fn an_open_settings_window_shows_the_followed_size_unless_another_was_typed() {
    let mut a = app();
    let shown = listed(&mut a, vec![screen("2", "Side", 2560, 1440)]);
    let ctx = egui::Context::default();
    run(&mut a, &ctx, "video.canvas_from_display", json!({"display": "2"})).unwrap();
    a.open_settings(crate::settings::Tab::Video);
    let draft = |a: &StudioApp| {
        let v = &a.settings.as_ref().unwrap().draft.video;
        (v.base_width, v.base_height)
    };
    shown.lock().unwrap()[0] = screen("2", "Side", 3840, 2160);
    let t = Instant::now();
    look(&mut a, &ctx, t);
    assert_eq!(a.studio().canvas(), (3840, 2160));
    assert_eq!(draft(&a), (3840, 2160));
    // A size typed and not yet applied stays.
    let v = &mut a.settings.as_mut().unwrap().draft.video;
    (v.base_width, v.base_height) = (1280, 720);
    shown.lock().unwrap()[0] = screen("2", "Side", 1920, 1080);
    look(&mut a, &ctx, t + follow::LOOK);
    assert_eq!(a.studio().canvas(), (1920, 1080));
    assert_eq!(draft(&a), (1280, 720));
}

#[test]
fn a_followed_size_that_cant_be_saved_is_saved_at_a_later_look() {
    let dir = temp_dir("follow-save-fails");
    std::fs::create_dir_all(&dir).unwrap();
    let (store, _) = Store::open(craft_settings::Location::Dir(dir.clone()));
    let mut a = StudioApp::boot(store, None, Vec::new());
    let shown = listed(&mut a, vec![screen("2", "Side", 2560, 1440)]);
    let ctx = egui::Context::default();
    run(&mut a, &ctx, "video.canvas_from_display", json!({"display": "2"})).unwrap();
    // A file where the profiles folder goes makes the save fail.
    let profiles = dir.join("profiles");
    let _ = std::fs::remove_dir_all(&profiles);
    std::fs::write(&profiles, b"").unwrap();
    shown.lock().unwrap()[0] = screen("2", "Side", 3840, 2160);
    let mut t = Instant::now();
    let before = a.notices.len();
    look(&mut a, &ctx, t);
    // The new size is taken back.
    let canvas = a.studio().canvas();
    let told = a.notices.len() - before;
    // The failure is shown once, not at every look.
    t += follow::LOOK;
    look(&mut a, &ctx, t);
    let told_again = a.notices.len() - before;
    // Once the profiles folder can be written, the next look takes and saves the size.
    std::fs::remove_file(&profiles).unwrap();
    t += follow::LOOK;
    look(&mut a, &ctx, t);
    let saved = a.store().load_profile(DEFAULT_NAME).map(|p| (p.video.base_width, p.video.base_height));
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(canvas, (2560, 1440));
    assert_eq!(told, 1);
    assert_eq!(told_again, 1);
    assert_eq!(a.studio().canvas(), (3840, 2160));
    assert_eq!(saved, Ok((3840, 2160)));
    assert_eq!(canvas_link(&a).2, 3840);
}

#[test]
fn a_followed_size_that_cant_be_saved_is_shown_again_after_a_save_worked() {
    let dir = temp_dir("follow-save-fails-twice");
    std::fs::create_dir_all(&dir).unwrap();
    let (store, _) = Store::open(craft_settings::Location::Dir(dir.clone()));
    let mut a = StudioApp::boot(store, None, Vec::new());
    let shown = listed(&mut a, vec![screen("2", "Side", 2560, 1440)]);
    let ctx = egui::Context::default();
    run(&mut a, &ctx, "video.canvas_from_display", json!({"display": "2"})).unwrap();
    let profiles = dir.join("profiles");
    let _ = std::fs::remove_dir_all(&profiles);
    std::fs::write(&profiles, b"").unwrap();
    shown.lock().unwrap()[0] = screen("2", "Side", 3840, 2160);
    let mut t = Instant::now();
    let before = a.notices.len();
    look(&mut a, &ctx, t);
    // The profile is saved another way, and then the size still can't be saved.
    std::fs::remove_file(&profiles).unwrap();
    let saved_all = a.save_all();
    std::fs::remove_dir_all(&profiles).unwrap();
    std::fs::write(&profiles, b"").unwrap();
    t += follow::LOOK;
    look(&mut a, &ctx, t);
    let canvas = a.studio().canvas();
    let told = a.notices.len() - before;
    let _ = std::fs::remove_file(&profiles);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(saved_all);
    assert_eq!(canvas, (2560, 1440));
    assert_eq!(told, 2);
}

#[test]
fn each_followed_size_that_cant_be_saved_is_shown() {
    let dir = temp_dir("follow-save-fails-new-size");
    std::fs::create_dir_all(&dir).unwrap();
    let (store, _) = Store::open(craft_settings::Location::Dir(dir.clone()));
    let mut a = StudioApp::boot(store, None, Vec::new());
    let shown = listed(&mut a, vec![screen("2", "Side", 2560, 1440)]);
    let ctx = egui::Context::default();
    run(&mut a, &ctx, "video.canvas_from_display", json!({"display": "2"})).unwrap();
    let profiles = dir.join("profiles");
    let _ = std::fs::remove_dir_all(&profiles);
    std::fs::write(&profiles, b"").unwrap();
    shown.lock().unwrap()[0] = screen("2", "Side", 3840, 2160);
    let mut t = Instant::now();
    let before = a.notices.len();
    look(&mut a, &ctx, t);
    // The display changes again while the profile still can't be saved.
    shown.lock().unwrap()[0] = screen("2", "Side", 1920, 1080);
    t += follow::LOOK;
    look(&mut a, &ctx, t);
    let canvas = a.studio().canvas();
    let told = a.notices.len() - before;
    let _ = std::fs::remove_file(&profiles);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(canvas, (2560, 1440));
    assert_eq!(told, 2);
}

#[test]
fn a_canvas_set_another_way_follows_no_display() {
    let mut a = app();
    let shown = listed(&mut a, vec![screen("2", "Side", 2560, 1440)]);
    let ctx = egui::Context::default();
    run(&mut a, &ctx, "video.canvas_from_display", json!({"display": "2"})).unwrap();
    let mut p = a.studio().profile().clone();
    (p.video.base_width, p.video.base_height) = (1280, 720);
    a.studio.set_profile(p).unwrap();
    let t = Instant::now();
    look(&mut a, &ctx, t);
    assert_eq!(canvas_link(&a), (String::new(), String::new(), 0, 0));
    shown.lock().unwrap()[0] = screen("2", "Side", 3840, 2160);
    look(&mut a, &ctx, t + follow::LOOK);
    assert_eq!(a.studio().canvas(), (1280, 720));
}

#[test]
fn the_display_the_window_is_on_is_followed_when_its_size_tells_it_apart() {
    let mut a = app();
    let shown = listed(&mut a, vec![screen("1", "Main", 1920, 1080), screen("2", "Side", 2560, 1440)]);
    let ctx = egui::Context::default();
    frame_on_display(&mut a, &ctx, egui::vec2(1280.0, 720.0), 2.0);
    let r = run(&mut a, &ctx, "video.canvas_from_display", json!({})).unwrap();
    assert_eq!(r["follows"], json!("2"));
    // Two displays of that size can't be told apart.
    shown.lock().unwrap()[0] = screen("1", "Main", 2560, 1440);
    let r = run(&mut a, &ctx, "video.canvas_from_display", json!({})).unwrap();
    assert_eq!(r["follows"], json!(null));
    assert_eq!(canvas_link(&a).0, "");
}

#[test]
fn only_the_profile_whose_canvas_came_from_a_display_follows_it() {
    let mut a = app();
    let shown = listed(&mut a, vec![screen("1", "Main", 1280, 720), screen("2", "Side", 2560, 1440)]);
    let ctx = egui::Context::default();
    run(&mut a, &ctx, "video.canvas_from_display", json!({"display": "2"})).unwrap();
    a.named(Named::RenameProfile, "Show").unwrap();
    assert_eq!(canvas_link(&a).0, "Show", "a renamed profile still follows");
    a.named(Named::DuplicateProfile, "Show 2").unwrap();
    assert_eq!(canvas_link(&a), ("Show 2".to_string(), "2".to_string(), 2560, 1440), "and so does a copy");
    a.switch_profile("Show").unwrap();
    a.named(Named::NewProfile, "Other").unwrap();
    assert_eq!(canvas_link(&a).0, "", "the new profile's canvas came from no display");
    shown.lock().unwrap()[1] = screen("2", "Side", 3840, 2160);
    let mut t = Instant::now();
    look(&mut a, &ctx, t);
    assert_eq!(a.studio().canvas(), (1920, 1080));
    // Each profile keeps the display its canvas follows.
    run(&mut a, &ctx, "video.canvas_from_display", json!({"display": "1"})).unwrap();
    a.switch_profile("Show").unwrap();
    t += follow::LOOK;
    look(&mut a, &ctx, t);
    assert_eq!(a.studio().canvas(), (3840, 2160));
    shown.lock().unwrap()[0] = screen("1", "Main", 1920, 1200);
    a.switch_profile("Other").unwrap();
    t += follow::LOOK;
    look(&mut a, &ctx, t);
    assert_eq!(a.studio().canvas(), (1920, 1200));
    // A removed profile's display is forgotten.
    a.remove_profile().unwrap();
    assert!(a.store.prefs().canvas_display("Other").is_none());
    assert_eq!(canvas_link(&a).0, a.studio().profile().name, "the next profile still follows");
}

#[test]
fn the_displays_canvases_follow_are_kept_within_bounds() {
    let mut p = store::Prefs::default();
    for n in 0..store::MAX_CANVAS_DISPLAYS + 3 {
        p.set_canvas_display(store::CanvasDisplay { profile: format!("P{n}"), ..Default::default() });
    }
    assert_eq!(p.canvas_displays.len(), store::MAX_CANVAS_DISPLAYS);
    assert!(p.canvas_display("P0").is_none(), "the earliest is forgotten first");
    p.set_canvas_display(store::CanvasDisplay { profile: "P10".into(), id: "4".into(), ..Default::default() });
    assert_eq!(p.canvas_displays.len(), store::MAX_CANVAS_DISPLAYS, "a profile has one");
    assert_eq!(p.canvas_display("P10").map(|l| l.id.as_str()), Some("4"));
    let back: store::Prefs = serde_json::from_value(serde_json::to_value(&p).unwrap()).unwrap();
    assert_eq!(back.canvas_displays, p.canvas_displays);
    let many: Vec<Value> = (0..store::MAX_CANVAS_DISPLAYS + 5).map(|n| json!({"profile": format!("Q{n}")})).collect();
    let read: store::Prefs = serde_json::from_value(json!({"canvas_displays": many})).unwrap();
    assert_eq!(read.canvas_displays.len(), store::MAX_CANVAS_DISPLAYS, "a longer list is cut");
}

#[test]
fn a_display_picked_in_settings_is_followed_once_applied() {
    let mut a = app();
    listed(&mut a, vec![screen("2", "Side", 2560, 1440)]);
    let apply = |a: &mut StudioApp, size: (u32, u32), display: Option<Option<DisplayInfo>>| {
        let w = a.settings.as_ref().unwrap();
        let mut profile = w.draft.clone();
        (profile.video.base_width, profile.video.base_height) = size;
        a.apply_settings(crate::settings::Apply {
            profile,
            overrides: a.keymap.overrides().clone(),
            language: w.language.clone(),
            check_updates: w.check_updates,
            system_title_bar: w.system_title_bar,
            graphics_adapter: w.graphics_adapter.clone(),
            canvas_display: display,
        })
    };
    a.open_settings(crate::settings::Tab::Video);
    // The size was edited after the pick.
    assert!(apply(&mut a, (2000, 1440), Some(Some(screen("2", "Side", 2560, 1440)))));
    assert_eq!(canvas_link(&a).0, "");
    a.settings.as_mut().unwrap().canvas_display = Some(Some(screen("2", "Side", 2560, 1440)));
    assert!(apply(&mut a, (2560, 1440), Some(Some(screen("2", "Side", 2560, 1440)))));
    assert_eq!(canvas_link(&a), (DEFAULT_NAME.to_string(), "2".to_string(), 2560, 1440));
    assert_eq!(a.settings.as_ref().unwrap().canvas_display, None, "the pick is used once");
    // Applying other settings with nothing picked keeps it.
    assert!(apply(&mut a, (2560, 1440), None));
    assert_eq!(canvas_link(&a).1, "2");
    // A pick that names no display (two displays of the window's size) ends it.
    assert!(apply(&mut a, (2560, 1440), Some(None)));
    assert_eq!(canvas_link(&a).0, "");
}

#[test]
fn a_slow_display_listing_does_not_hold_up_the_window() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let mut a = app();
    let shown = Arc::new(std::sync::Mutex::new(vec![screen("2", "Side", 2560, 1440)]));
    let slow = Arc::new(AtomicBool::new(false));
    let (go, wait) = std::sync::mpsc::channel::<()>();
    let wait = std::sync::Mutex::new(wait);
    let (theirs, slow_there) = (shown.clone(), slow.clone());
    a.displays = follow::Watch::new(Arc::new(move || {
        if slow_there.load(Ordering::Acquire) {
            let _ = wait.lock().unwrap().recv();
        }
        theirs.lock().unwrap().clone()
    }));
    let ctx = egui::Context::default();
    run(&mut a, &ctx, "video.canvas_from_display", json!({"display": "2"})).unwrap();
    slow.store(true, Ordering::Release);
    shown.lock().unwrap()[0] = screen("2", "Side", 3840, 2160);
    a.follow_displays(&ctx, Instant::now());
    assert!(a.displays.looking());
    let before = a.frames();
    for _ in 0..20 {
        frame(&mut a, &ctx, vec![]);
    }
    assert_eq!(a.frames(), before + 20, "the window keeps drawing");
    assert!(a.displays.looking(), "while the look waits");
    assert_eq!(a.studio().canvas(), (2560, 1440));
    go.send(()).unwrap();
    let t = Instant::now();
    look(&mut a, &ctx, t);
    assert_eq!(a.studio().canvas(), (3840, 2160));
}

#[test]
fn display_sizes_are_checked() {
    let d = |id: &str| DisplayInfo { id: id.into(), name: "Screen".into(), width: 2560, height: 1440, primary: false };
    let list = [d("1"), d("7")];
    assert_eq!(find_display(&list, "7").map(|d| d.id.as_str()), Ok("7"));
    assert_eq!(find_display(&list, "9").unwrap_err().english(), "there is no display `9` (the displays are 1, 7)");
    assert!(find_display(&[], "1").unwrap_err().english().contains("lists no displays"));
    assert_eq!(canvas_for((16, 8192)), Ok((16, 8192)));
    assert!(canvas_for((15, 1080)).is_err());
    assert!(canvas_for((1920, 8193)).is_err());
    assert!(canvas_for((0, 0)).is_err());
    let info = |monitor_size: Option<egui::Vec2>, native_pixels_per_point: Option<f32>| egui::ViewportInfo {
        monitor_size,
        native_pixels_per_point,
        ..Default::default()
    };
    assert_eq!(window_display(&info(Some(egui::vec2(1280.0, 800.0)), Some(1.5)), 1.0), Some((1920, 1200)));
    // With the interface zoomed to 125%, a 1920 x 1200 display is 1536 x 960 points.
    assert_eq!(window_display(&info(Some(egui::vec2(1536.0, 960.0)), Some(1.0)), 1.25), Some((1920, 1200)));
    assert_eq!(window_display(&info(None, Some(1.0)), 1.0), None);
    assert_eq!(window_display(&info(Some(egui::vec2(1280.0, 800.0)), None), 1.0), None);
    assert_eq!(window_display(&info(Some(egui::vec2(f32::NAN, 800.0)), Some(1.0)), 1.0), None);
    assert_eq!(window_display(&info(Some(egui::vec2(0.0, 800.0)), Some(1.0)), 1.0), None);
    assert_eq!(window_display(&info(Some(egui::vec2(1e30, 800.0)), Some(1.0)), 1.0), None);
}

#[test]
fn window_command_errors_are_shown_translated_and_returned_in_english() {
    let mut a = app();
    a.set_language("de");
    let ctx = egui::Context::default();
    let e = run(&mut a, &ctx, "dock.layout_apply", json!({"name": "Talk"})).unwrap_err();
    assert_eq!(e, "there is no dock layout named `Talk`");
    let Some((note, _)) = a.notices.back() else { panic!("no notice") };
    assert!(note.error);
    assert_eq!(note.msg.english(), e);
    assert_eq!(note.msg.shown(), "es gibt kein Dock-Layout namens `Talk`");
    let notices = a.hud()["ui"]["notices"].clone();
    assert_eq!(notices.as_array().and_then(|n| n.last()).map(|n| n["text"].clone()), Some(json!(e)));

    // A name the prompt refuses stays in the prompt, translated, and the control channel gets it
    // in English.
    run(&mut a, &ctx, "dock.layout_save", json!({})).unwrap();
    let e = a.answer_dialog(&ctx, true, Some("a/b")).unwrap_err();
    assert_eq!(e, "a name can't contain '/'");
    assert_eq!(a.ui_inspect(&ctx)["dialogs"]["prompt"]["error"], json!(e));
    assert_eq!(a.prompt.as_ref().and_then(|p| p.error.as_ref()).map(Msg::shown).as_deref(), Some("ein Name darf '/' nicht enthalten"));
    let e = canvas_for((8, 4)).unwrap_err();
    assert_eq!(e.english(), "a 8 × 4 display can't be the canvas: each side must be from 16 to 8192 pixels");
    assert_eq!(e.shown(), "ein Bildschirm mit 8 × 4 kann nicht die Leinwand sein: jede Seite muss 16 bis 8192 Pixel haben");
    a.set_language("en");
}

#[test]
fn pause_or_resume_pauses_the_program_and_the_status_bar_shows_the_countdown() {
    let mut a = app();
    let ctx = egui::Context::default();
    assert_eq!(a.item_state("pause.toggle").checked, Some(false));
    assert!(!a.item_state("pause.extend").enabled, "there is nothing to add time to");
    assert!(a.keymap.catalog().iter().any(|b| b.command == CommandRef::new("pause.toggle")), "Pause or Resume Program takes a shortcut");
    run(&mut a, &ctx, "pause.toggle", json!({})).unwrap();
    assert_eq!(a.item_state("pause.toggle").checked, Some(true));
    assert!(a.item_state("pause.extend").enabled);
    let left = a.studio().paused().flatten().unwrap_or(0);
    run(&mut a, &ctx, "pause.extend", json!({"seconds": 60})).unwrap();
    assert!(a.studio().paused().flatten().unwrap_or(0) > left, "a minute was added");
    frame(&mut a, &ctx, vec![]);
    run(&mut a, &ctx, "pause.toggle", json!({})).unwrap();
    assert_eq!(a.item_state("pause.toggle").checked, Some(false));
    frame(&mut a, &ctx, vec![]);
}

#[test]
fn the_profile_is_not_switched_or_removed_while_the_program_is_paused() {
    let mut a = app();
    let ctx = egui::Context::default();
    a.named(Named::NewProfile, "Second").unwrap();
    run(&mut a, &ctx, "pause.toggle", json!({})).unwrap();
    let e = run(&mut a, &ctx, "profile.switch", json!({"name": DEFAULT_NAME})).unwrap_err();
    assert_eq!(e, "Resume before switching the profile");
    assert!(a.remove_profile().is_err(), "removing the profile switches to another");
    for what in [Named::NewProfile, Named::DuplicateProfile] {
        let e = a.named(what, "Third").unwrap_err();
        assert_eq!(e.english(), "Resume before switching the profile", "{what:?}");
    }
    a.named(Named::RenameProfile, "Renamed").unwrap();
    assert_eq!(a.studio().profile().name, "Renamed", "a rename keeps the open profile");
    assert!(!a.store().profiles().contains(&"Third".to_string()), "no profile was added");
    a.named(Named::RenameProfile, "Second").unwrap();
    assert_eq!(a.studio().profile().name, "Second");
    assert!(a.store().profiles().contains(&"Second".to_string()), "the profile was not removed");
    assert!(a.studio().paused().is_some(), "the pause continues");
    run(&mut a, &ctx, "pause.toggle", json!({})).unwrap();
    run(&mut a, &ctx, "profile.switch", json!({"name": DEFAULT_NAME})).unwrap();
    assert_eq!(a.studio().profile().name, DEFAULT_NAME);
}

#[test]
fn the_pause_status_item_shows_the_time_left_and_the_time_paused() {
    assert_eq!(pause_item(None, None), None, "nothing while not paused");
    assert_eq!(pause_item(Some(Some(247_000)), Some(61_500)), Some(("PAUSED 4:07".into(), "Paused for 1:01".into())));
    assert_eq!(pause_item(Some(None), Some(3_600_000)), Some(("PAUSED".into(), "Paused for 1:00:00".into())));
}

#[test]
fn the_stream_tag_says_reconnecting_when_none_is_live() {
    use studiocraft_plugin::DestinationCounts as C;
    assert_eq!(stream_tag(C { live: 0, retrying: 2 }), ("RECONNECTING", None, true));
    assert_eq!(stream_tag(C { live: 1, retrying: 0 }), ("LIVE", None, false));
    // Some live: the tag stays LIVE and the count of those waiting follows it.
    assert_eq!(stream_tag(C { live: 2, retrying: 1 }), ("LIVE", Some("1 reconnecting".into()), false));
    // Connecting for the first time, with none waiting, is LIVE as before.
    assert_eq!(stream_tag(C::default()), ("LIVE", None, false));
}

/// The File menu's items in English, with the command each runs.
fn file_items(a: &StudioApp) -> Vec<(String, CommandRef)> {
    let bar = a.laid_menu();
    let Some(file) = bar.menus.iter().find(|m| m.title == "File") else { panic!("no File menu") };
    file.children
        .iter()
        .filter_map(|n| match n {
            craft_menu::Node::Item(it) => Some((it.label.clone(), it.command.clone())),
            _ => None,
        })
        .collect()
}

fn click_at(at: egui::Pos2) -> Vec<Event> {
    let b = |pressed| Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
    vec![Event::PointerMoved(at), b(true), b(false)]
}

#[test]
fn the_file_menu_pauses_and_resumes_the_program() {
    let mut a = app();
    let ctx = egui::Context::default();
    let items = file_items(&a);
    let pause = ("Pause Program".to_string(), CommandRef::new("pause.start"));
    let resume = ("Resume Program".to_string(), CommandRef::new("pause.stop"));
    let at = |item: &(String, CommandRef)| items.iter().position(|i| i == item);
    let (Some(p), Some(r)) = (at(&pause), at(&resume)) else { panic!("File lacks Pause Program or Resume Program: {items:?}") };
    assert_eq!(r, p + 1, "Resume Program follows Pause Program");
    let extend = CommandRef::with_params("pause.extend", json!({"seconds": 60}));
    assert_eq!(items.get(r + 1), Some(&("Add a Minute to the Pause".to_string(), extend)));
    assert!(a.item_state("pause.start").enabled && !a.item_state("pause.stop").enabled);
    run(&mut a, &ctx, "pause.start", json!({})).unwrap();
    assert!(a.studio().paused().is_some());
    assert!(!a.item_state("pause.start").enabled && a.item_state("pause.stop").enabled);
    run(&mut a, &ctx, "pause.stop", json!({})).unwrap();
    assert!(a.studio().paused().is_none());
    assert!(a.item_state("pause.start").enabled && !a.item_state("pause.stop").enabled);
}

#[test]
fn the_pause_button_pauses_and_the_banner_adds_a_minute_and_resumes() {
    let mut a = app();
    let ctx = egui::Context::default();
    for _ in 0..2 {
        frame(&mut a, &ctx, vec![]);
    }
    assert!(a.pause_banner.buttons.is_empty(), "no banner while not paused");
    let rect = |a: &StudioApp, part: &str| bar_parts(a, &ctx).into_iter().find(|(n, _)| n == part).map(|(_, r)| r);
    let Some([l, t, r, b]) = rect(&a, "pause") else { panic!("no Pause button") };
    frame(&mut a, &ctx, click_at(egui::pos2((l + r) / 2.0, (t + b) / 2.0)));
    assert!(a.studio().paused().is_some(), "a click on the face pauses with the profile's settings");
    frame(&mut a, &ctx, vec![]);
    let buttons = a.pause_banner.buttons.clone();
    assert_eq!(buttons.len(), 2, "+1 min and Resume");
    assert!(rect(&a, "pause_menu").is_some(), "the Pause arrow stays while paused");
    assert!(buttons[0].top() >= 32.0, "the banner is under the menu row: {:?}", buttons[0]);
    let left = a.studio().paused().flatten().unwrap_or(0);
    frame(&mut a, &ctx, click_at(buttons[0].center()));
    assert!(a.studio().paused().flatten().unwrap_or(0) > left + 50_000, "+1 min added a minute");
    frame(&mut a, &ctx, vec![]);
    let resume = a.pause_banner.buttons[1].center();
    frame(&mut a, &ctx, click_at(resume));
    assert!(a.studio().paused().is_none(), "Resume ended the pause");
    frame(&mut a, &ctx, vec![]);
    assert!(a.pause_banner.buttons.is_empty(), "the banner is gone");
}

#[test]
fn the_pause_menu_starts_a_countdown_and_opens_the_pause_screen_settings() {
    let mut a = app();
    let ctx = egui::Context::default();
    for (_, cmd) in crate::output_bar::pause_menu().into_iter().filter(|(_, c)| c.id == "pause.start") {
        a.perform(&ctx, &cmd).unwrap();
        let want = cmd.params["countdown_s"].as_u64().unwrap_or(u64::MAX) * 1000;
        match a.studio().paused() {
            Some(Some(left)) => assert!(left <= want && left + 2_000 >= want, "{cmd:?}: {left}"),
            Some(None) => assert_eq!(want, 0, "{cmd:?}"),
            None => panic!("{cmd:?} did not pause"),
        }
    }
    run(&mut a, &ctx, "pause.stop", json!({})).unwrap();
    run(&mut a, &ctx, "pause.settings", json!({})).unwrap();
    let Some(w) = &a.settings else { panic!("Settings did not open") };
    assert_eq!(w.tab(), Tab::Stream);
    assert!(w.pause_focus_pending());
    frame_at(&mut a, &ctx, egui::vec2(760.0, 480.0));
    assert!(a.settings.as_ref().is_some_and(|w| !w.pause_focus_pending()), "the Pause screen section took the focus");
    assert!(ctx.memory(|m| m.focused()).is_some(), "a field has the keyboard focus");
}
