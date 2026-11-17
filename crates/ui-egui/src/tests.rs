use egui::{Event, Key, Modifiers, RawInput};

use super::*;

fn app() -> StudioApp {
    StudioApp::boot(Store::memory(), None, Vec::new())
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
    p.stream.server = "rtmp://example.invalid/live".into();
    a.studio.set_profile(p).unwrap();
    run(&mut a, &ctx, "profile.switch", json!({"name": DEFAULT_NAME})).unwrap();
    assert_eq!(a.studio().profile().stream.server, "");
    run(&mut a, &ctx, "profile.switch", json!({"name": "Stream"})).unwrap();
    assert_eq!(a.studio().profile().stream.server, "rtmp://example.invalid/live");
    assert!(a.named(Named::NewProfile, "bad/name").is_err());
}

#[test]
fn available_encoders_lists_the_built_in_one_without_ffmpeg() {
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
    assert!(list.iter().filter(|(name, _, _)| name != "H.264 (built in)").all(|(_, _, ok)| !ok), "{list:?}");
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
        for want in ["menus", "mode", "record", "stream"] {
            assert!(names.contains(&want), "{w}: {want} missing from {names:?}");
        }
        assert!(names.contains(&"transition") || names.contains(&"transition_overflow"), "{w}: {names:?}");
        for (name, [l, t, r, b]) in &parts {
            assert!(*l >= 0.0 && *r <= w + 0.5 && *t >= 0.0 && *b <= 32.5, "{w}: {name} at {l},{t},{r},{b} is cut off");
            if name == "record" || name == "stream" {
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
        let stream = parts.iter().find(|(n, _)| n == "stream").map(|(_, r)| r[2]).unwrap_or(0.0);
        assert!((w - stream - 8.0).abs() <= 1.0, "{w}: Go Live ends 8 points from the right edge, at {stream}");
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
    assert_eq!(names(&a), ["menus", "mode", "record", "stream"]);
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
    assert_eq!(r, json!({"moved": true, "order": ["scenes", "mixer", "sources"]}));
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
    let until = Instant::now() + Duration::from_secs(5);
    while a.studio().source_size(source).is_none() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(10));
    }
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
