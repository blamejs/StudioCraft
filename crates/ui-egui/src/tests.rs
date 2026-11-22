use egui::{Event, Key, Modifiers, RawInput};

use super::*;

fn app() -> StudioApp {
    StudioApp::boot(Store::memory(), None, Vec::new())
}

fn frame(app: &mut StudioApp, ctx: &egui::Context, events: Vec<Event>) {
    let raw = RawInput { events, screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 800.0))), ..RawInput::default() };
    app.before_input(ctx);
    let mut out = ctx.run_ui(raw, |ui| app.frame(ui));
    out.textures_delta.clear();
    // Run what the pass fired (shortcuts, menu rows), as the next frame's input hook does.
    app.before_input(ctx);
}

fn frame_at(app: &mut StudioApp, ctx: &egui::Context, size: egui::Vec2) {
    let raw = RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)), ..RawInput::default() };
    app.before_input(ctx);
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
    let ctx = crate::test_context();
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
    let ctx = crate::test_context();
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
    let ctx = crate::test_context();
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
fn the_desktop_shell_keeps_the_menu_bar_in_the_window_on_macos() {
    use craft_host::target::{MenuPlacement, Os};

    let mac = ShellInfo::current().os(Os::Mac).caps(Caps::native(Os::Mac));
    assert_eq!(mac.menu(), MenuPlacement::ScreenTop);
    let shell = desktop_shell(mac, DoubleClick::Minimize);
    assert_eq!(shell.menu(), MenuPlacement::InWindow);
    assert_eq!(shell.double_click, DoubleClick::Minimize);
    assert_eq!(shell.caps, Caps::native(Os::Mac).difference(Caps::NATIVE_MENU_BAR));
}

/// A context with the UI runtime installed for a window whose frame the app draws, with the
/// Windows window buttons Minimize, Maximize and Close at the end of the bar.
fn integrated_context() -> egui::Context {
    use craft_ui::shell::{CaptionFamily, Captions, Frame};
    use craft_window::policy::{Desktop, DesktopKind};

    let layout = craft_window::Layout {
        left: Vec::new(),
        right: vec![craft_window::Button::Minimize, craft_window::Button::Maximize, craft_window::Button::Close],
    };
    let frame = Frame::Integrated(Captions { family: CaptionFamily::Windows, layout });
    let desktop = Desktop { kind: DesktopKind::Other, ssd: None, gtk_theme: None, icon_theme: None };
    let spec = InstallSpec::new(ShellInfo::default(), Arc::new(NoWindows)).frame(Decision { frame, overridden: None, desktop });
    let ctx = egui::Context::default();
    if let Err(e) = crate::install_spec(&ctx, spec) {
        panic!("the UI runtime did not install: {e:?}");
    }
    ctx
}

/// Runs one 1280 x 800 frame with `events` at `seconds` and returns the window commands it sent.
fn window_commands(app: &mut StudioApp, ctx: &egui::Context, events: Vec<Event>, seconds: f64) -> Vec<egui::ViewportCommand> {
    let raw = RawInput {
        events,
        time: Some(seconds),
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 800.0))),
        ..RawInput::default()
    };
    app.before_input(ctx);
    let mut out = ctx.run_ui(raw, |ui| app.frame(ui));
    out.textures_delta.clear();
    app.before_input(ctx);
    out.viewport_output.remove(&egui::ViewportId::ROOT).map(|v| v.commands).unwrap_or_default()
}

/// A press (`down`) or release of the primary button at `at`.
fn button(at: egui::Pos2, down: bool) -> Event {
    Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed: down, modifiers: Modifiers::NONE }
}

#[test]
fn the_menu_strip_is_the_title_bar_when_the_app_draws_the_frame() {
    let mut a = app();
    let ctx = integrated_context();
    for n in 0..3 {
        window_commands(&mut a, &ctx, vec![], f64::from(n) * 0.05);
    }
    // The menu titles sit at the start of the bar, clear of the window buttons at its end.
    let parts = bar_parts(&a, &ctx);
    let menus = parts.iter().find(|(name, _)| name == "menus").map(|(_, r)| *r).unwrap_or_else(|| panic!("no menus in {parts:?}"));
    assert!(menus[1] >= 0.0 && menus[0] < 100.0 && menus[2] < 1280.0 - 3.0 * 46.0, "{menus:?}");
    // Tab and F with nothing focused leave the bar in place.
    for (n, key) in [Key::Tab, Key::F].into_iter().enumerate() {
        let mut sent = window_commands(&mut a, &ctx, press(key, Modifiers::NONE), 0.1 + n as f64 * 0.01);
        a.output_bar.rect = None;
        sent.extend(window_commands(&mut a, &ctx, vec![], 0.105 + n as f64 * 0.01));
        assert!(a.output_bar.rect.is_some(), "{key:?} hid the title bar");
        assert!(!sent.iter().any(|c| matches!(c, egui::ViewportCommand::Fullscreen(_))), "{key:?} changed full screen: {sent:?}");
    }
    // The menus still run their commands from the strip.
    run(&mut a, &ctx, "app.settings", json!({})).unwrap();
    window_commands(&mut a, &ctx, vec![], 0.2);
    assert_eq!(a.hud()["ui"]["settings_open"], json!(true));
    // Close is the button at the window's end edge, 46 points wide; it closes the window through
    // the close confirmation.
    let close = egui::pos2(1280.0 - 23.0, (menus[1] + menus[3]) / 2.0);
    let mut sent = Vec::new();
    for (n, events) in [vec![Event::PointerMoved(close)], vec![button(close, true)], vec![button(close, false)], vec![]].into_iter().enumerate() {
        sent.extend(window_commands(&mut a, &ctx, events, 1.0 + n as f64 * 0.05));
    }
    assert!(sent.contains(&egui::ViewportCommand::Close), "{sent:?}");
}

#[test]
fn the_strips_empty_area_moves_the_window_and_a_double_click_maximizes_it() {
    let mut a = app();
    let ctx = integrated_context();
    for n in 0..3 {
        window_commands(&mut a, &ctx, vec![], f64::from(n) * 0.05);
    }
    // The empty area lies between the menu titles and the first output control.
    let parts = bar_parts(&a, &ctx);
    let menus = parts.iter().find(|(name, _)| name == "menus").map(|(_, r)| *r).unwrap_or_else(|| panic!("no menus in {parts:?}"));
    let outputs = parts.iter().filter(|(name, _)| name != "menus").map(|(_, r)| r[0]).fold(1280.0 - 3.0 * 46.0, f32::min);
    assert!(outputs - menus[2] > 20.0, "the bar has an empty area: {parts:?}");
    let empty = egui::pos2((menus[2] + outputs) / 2.0, (menus[1] + menus[3]) / 2.0);
    let to = empty + egui::vec2(40.0, 0.0);

    let drag = [vec![Event::PointerMoved(empty)], vec![button(empty, true)], vec![Event::PointerMoved(to)], vec![button(to, false)], vec![]];
    let mut sent = Vec::new();
    for (n, events) in drag.into_iter().enumerate() {
        sent.extend(window_commands(&mut a, &ctx, events, 1.0 + n as f64 * 0.05));
    }
    assert!(sent.contains(&egui::ViewportCommand::StartDrag), "a drag of the empty area moves the window: {sent:?}");

    let double = [
        vec![Event::PointerMoved(empty)],
        vec![button(empty, true)],
        vec![button(empty, false)],
        vec![button(empty, true)],
        vec![button(empty, false)],
        vec![],
    ];
    let mut sent = Vec::new();
    for (n, events) in double.into_iter().enumerate() {
        sent.extend(window_commands(&mut a, &ctx, events, 3.0 + n as f64 * 0.05));
    }
    assert!(sent.contains(&egui::ViewportCommand::Maximized(true)), "a double-click maximizes the window: {sent:?}");
    assert!(!sent.contains(&egui::ViewportCommand::StartDrag), "a double-click does not move the window: {sent:?}");
}

/// Whether the exit question's prompt is open on the window's dialog host.
fn exit_prompt_shown(a: &StudioApp) -> bool {
    let (Some(host), Some(id)) = (&a.window_dialogs, a.exit_prompt) else { return false };
    host.with(|h| h.get_by_id::<StandardPrompt>(id).is_some())
}

#[test]
fn the_exit_question_is_a_prompt_on_the_dialog_host_that_escape_and_the_control_channel_answer() {
    let mut a = app();
    let ctx = crate::test_context();
    frame(&mut a, &ctx, vec![]);
    // (case, answer: None presses Escape in the window, Some(ok) answers through `ui.dialog`) -> closes the window
    for (case, answer, exits) in [("Escape cancels", None, false), ("Cancel keeps running", Some(false), false), ("Stop and Exit", Some(true), true)]
    {
        a.confirm_exit = true;
        frame(&mut a, &ctx, vec![]);
        assert!(exit_prompt_shown(&a), "{case}: the question shows on the dialog host");
        assert!(a.answer_dialog(&ctx, true, Some("x")).is_err_and(|e| e.contains("no `text`")), "{case}");
        assert!(a.confirming_exit() && exit_prompt_shown(&a), "{case}: a refused answer leaves the question open");
        let shown = a.exit_prompt;
        match answer {
            None => frame(&mut a, &ctx, press(Key::Escape, Modifiers::NONE)),
            Some(ok) => {
                assert_eq!(a.answer_dialog(&ctx, ok, None), Ok(json!({"dialog": "confirm_exit", "closed": true, "exiting": ok})), "{case}");
            }
        }
        assert!(!a.confirming_exit() && a.exit_prompt.is_none(), "{case}");
        assert_eq!(a.exit_confirmed, exits, "{case}");
        let gone = a.window_dialogs.as_ref().zip(shown).is_some_and(|(h, id)| h.with(|h| h.get_by_id::<StandardPrompt>(id).is_none()));
        assert!(gone, "{case}: the prompt left the dialog host");
        assert!(a.answer_dialog(&ctx, true, None).is_err(), "{case}: nothing is left to answer");
    }
}

#[test]
fn a_name_prompt_is_answered_before_the_first_frame_and_shows_its_refusal_once_open() {
    let mut a = app();
    let ctx = crate::test_context();
    run(&mut a, &ctx, "collection.new", json!({})).unwrap();
    let taken = DEFAULT_NAME.to_lowercase();
    assert!(a.answer_dialog(&ctx, true, Some(&taken)).is_err_and(|e| e.contains("already")));
    assert!(a.ui_inspect(&ctx)["dialogs"]["prompt"]["error"].as_str().is_some_and(|e| e.contains("already")));
    frame(&mut a, &ctx, vec![]);
    let id = a.naming.as_ref().and_then(|n| n.dialog).expect("the prompt opens with the first frame");
    let host = a.window_dialogs.clone().unwrap();
    assert!(matches!(host.with(|h| h.held_state(id)), Some(craft_ui::dialog::HeldOk::Refused(_))), "the refusal shows under the field");
    let fields = host.with(|h| h.get_by_id::<HostedParamDialog>(id).map(|d| d.fields().text(NAME_FIELD).map(str::to_string)));
    assert_eq!(fields, Some(Some(taken)));
    assert_eq!(a.answer_dialog(&ctx, true, Some("Second")), Ok(json!({"dialog": "prompt", "closed": true, "name": "Second"})));
    assert_eq!(a.studio().collection().name, "Second");
}

#[test]
fn a_refused_name_keeps_the_name_prompt_open_with_its_error() {
    let mut a = app();
    let ctx = crate::test_context();
    frame(&mut a, &ctx, vec![]);
    run(&mut a, &ctx, "collection.new", json!({})).unwrap();
    frame(&mut a, &ctx, vec![]);
    let id = a.naming.as_ref().and_then(|n| n.dialog).expect("the name prompt shows on the dialog host");
    let host = a.window_dialogs.clone().unwrap();
    // OK with a name that is taken (names are compared without case), as the button does.
    host.with(|h| h.set_json_field(&ctx, id, NAME_FIELD, json!(DEFAULT_NAME.to_lowercase()))).unwrap();
    host.with(|h| h.confirm_by_id(&ctx, id));
    // Until the window has answered that OK, the control channel's answer is refused.
    assert!(a.answer_dialog(&ctx, true, Some("Other")).is_err_and(|e| e.contains("still being answered")));
    for _ in 0..2 {
        frame(&mut a, &ctx, vec![]);
    }
    assert!(host.with(|h| h.get_by_id::<HostedParamDialog>(id).is_some()), "the prompt stays open");
    assert!(matches!(host.with(|h| h.held_state(id)), Some(craft_ui::dialog::HeldOk::Refused(_))), "its error shows under the field");
    let prompt = &a.ui_inspect(&ctx)["dialogs"]["prompt"];
    assert!(prompt["error"].as_str().is_some_and(|e| e.contains("already")), "{prompt}");
    assert_eq!(a.collections, [DEFAULT_NAME]);
    // A name it takes closes it and makes the collection.
    host.with(|h| h.set_json_field(&ctx, id, NAME_FIELD, json!("Second"))).unwrap();
    frame(&mut a, &ctx, vec![]);
    assert_eq!(a.ui_inspect(&ctx)["dialogs"]["prompt"]["error"], Value::Null, "a changed name ends the error");
    host.with(|h| h.confirm_by_id(&ctx, id));
    for _ in 0..2 {
        frame(&mut a, &ctx, vec![]);
    }
    assert!(a.naming.is_none() && host.with(|h| h.get_by_id::<HostedParamDialog>(id).is_none()));
    assert_eq!(a.studio().collection().name, "Second");
    // Escape closes the prompt without a name.
    run(&mut a, &ctx, "profile.new", json!({})).unwrap();
    frame(&mut a, &ctx, vec![]);
    frame(&mut a, &ctx, press(Key::Escape, Modifiers::NONE));
    frame(&mut a, &ctx, vec![]);
    assert!(a.naming.is_none(), "Escape cancels the name prompt");
    assert_eq!(a.profiles, [DEFAULT_NAME]);
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
    let ctx = crate::test_context();
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
    let ctx = crate::test_context();
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
    let ctx = crate::test_context();
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

/// Runs one frame with `events` and returns what the runtime fired for them, each with the
/// parameters the catalog entry stands for. Nothing it fired is run.
fn fired_by(app: &mut StudioApp, ctx: &egui::Context, events: Vec<Event>) -> Vec<(CommandRef, craft_ui::runtime::commands::FireSource)> {
    let raw = RawInput { events, screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 800.0))), ..RawInput::default() };
    app.before_input(ctx);
    let mut out = ctx.run_ui(raw, |ui| app.frame(ui));
    out.textures_delta.clear();
    let mut fired = Vec::new();
    embed::before_input(ctx, |mut output| {
        fired = std::mem::take(&mut output.fired).into_iter().map(|f| (commands::fired_command(&f.command), f.source)).collect();
        app.frame_input(ctx, output)
    });
    fired
}

/// The name of the scene on the program.
fn program_name(app: &StudioApp) -> Option<String> {
    let c = app.studio().collection();
    c.scene(c.program_scene).map(|s| s.name.clone())
}

/// The number of items in the scene being edited.
fn edit_items(app: &StudioApp) -> usize {
    app.studio().collection().scene(app.studio().engine().edit_scene()).map_or(0, |s| s.items.len())
}

#[test]
fn every_default_shortcut_fires_its_command() {
    use craft_ui::runtime::commands::FireSource;
    let mut a = app();
    let ctx = crate::test_context();
    frame(&mut a, &ctx, vec![]);
    let scene = |i: u32| CommandRef::with_params("scene.select_index", json!({"index": i}));
    let rows = [
        (Key::Z, Modifiers::COMMAND, CommandRef::new("edit.undo")),
        (Key::Y, Modifiers::COMMAND, CommandRef::new("edit.redo")),
        (Key::Z, Modifiers::COMMAND | Modifiers::SHIFT, CommandRef::new("edit.redo")),
        (Key::Delete, Modifiers::NONE, CommandRef::new("selection.remove")),
        (Key::F, Modifiers::COMMAND, CommandRef::new("selection.fit")),
        (Key::R, Modifiers::COMMAND, CommandRef::new("selection.reset")),
        (Key::F11, Modifiers::NONE, CommandRef::new("view.fullscreen")),
        (Key::Num1, Modifiers::COMMAND, scene(1)),
        (Key::Num5, Modifiers::COMMAND, scene(5)),
        (Key::Num9, Modifiers::COMMAND, scene(9)),
    ];
    for (key, modifiers, command) in rows {
        assert_eq!(fired_by(&mut a, &ctx, press(key, modifiers)), [(command.clone(), FireSource::Key)], "{key:?} with {modifiers:?}");
    }
}

#[test]
fn shortcuts_run_their_commands_unless_a_dialog_has_the_keys() {
    type Setup = fn(&mut StudioApp, &egui::Context);
    // Two scenes with `Scene` on the program, and the addition of `Second` sealed in the history.
    let two: Setup = |a, ctx| {
        run(a, ctx, "scene.add", json!({"name": "Second"})).unwrap();
        run(a, ctx, "scene.select", json!({"scene": "Scene"})).unwrap();
    };
    let second: Setup = |a, ctx| {
        run(a, ctx, "scene.add", json!({"name": "Second"})).unwrap();
        run(a, ctx, "scene.select", json!({"scene": "Second"})).unwrap();
    };
    let added: Setup = |a, ctx| {
        run(a, ctx, "scene.add", json!({"name": "Second"})).unwrap();
        a.studio.seal_history();
    };
    let prompt: Setup = |a, ctx| {
        run(a, ctx, "scene.add", json!({"name": "Second"})).unwrap();
        a.studio.seal_history();
        a.ask_name(ctx, Named::NewProfile, "");
    };
    // (case, setup, key, modifiers, scenes after, program scene after when the case checks it)
    let rows: [(&str, Setup, Key, Modifiers, usize, Option<&str>); 5] = [
        ("Cmd+2 shows the second scene", two, Key::Num2, Modifiers::COMMAND, 2, Some("Second")),
        ("Cmd+1 shows the first scene", second, Key::Num1, Modifiers::COMMAND, 2, Some("Scene")),
        ("Cmd+3 with two scenes changes nothing", two, Key::Num3, Modifiers::COMMAND, 2, Some("Scene")),
        ("Cmd+Z undoes the last change", added, Key::Z, Modifiers::COMMAND, 1, None),
        ("Cmd+Z waits while the name prompt is open", prompt, Key::Z, Modifiers::COMMAND, 2, None),
    ];
    for (case, setup, key, modifiers, scenes, shown) in rows {
        let mut a = app();
        let ctx = crate::test_context();
        setup(&mut a, &ctx);
        frame(&mut a, &ctx, vec![]);
        frame(&mut a, &ctx, press(key, modifiers));
        assert_eq!(a.studio().collection().scenes.len(), scenes, "{case}");
        if let Some(shown) = shown {
            assert_eq!(program_name(&a).as_deref(), Some(shown), "{case}");
        }
    }
}

#[test]
fn redo_takes_cmd_y_and_cmd_shift_z_and_delete_removes_the_selected_source() {
    let mut a = app();
    let ctx = crate::test_context();
    run(&mut a, &ctx, "scene.add", json!({"name": "Extra"})).unwrap();
    a.studio.seal_history();
    frame(&mut a, &ctx, vec![]);
    for (name, redo) in [("Cmd+Y", press(Key::Y, Modifiers::COMMAND)), ("Cmd+Shift+Z", press(Key::Z, Modifiers::COMMAND | Modifiers::SHIFT))] {
        frame(&mut a, &ctx, press(Key::Z, Modifiers::COMMAND));
        assert_eq!(a.studio().collection().scenes.len(), 1, "undone before {name}");
        frame(&mut a, &ctx, redo);
        assert_eq!(a.studio().collection().scenes.len(), 2, "{name} redoes");
    }
    run(&mut a, &ctx, "source.add", json!({"kind": "color", "name": "Backdrop"})).unwrap();
    assert!(a.selection().is_some());
    frame(&mut a, &ctx, vec![]);
    frame(&mut a, &ctx, press(Key::Backspace, Modifiers::NONE));
    assert_eq!(edit_items(&a), 1, "Backspace leaves the source");
    assert!(a.notices.is_empty(), "Backspace posts nothing: {:?}", a.notices.iter().map(|(n, _)| n.msg.english()).collect::<Vec<_>>());
    frame(&mut a, &ctx, press(Key::Delete, Modifiers::NONE));
    assert_eq!(edit_items(&a), 0, "Delete removes the selected source");
}

/// A fixture that shows `bar` as the window's menu bar does, on Windows, with the menus'
/// mnemonics and English labels.
fn menu_fixture(bar: craft_menu::MenuBar) -> craft_ui::testing::Fixture {
    let bar = Tracked::new(bar);
    let mut f = craft_ui::testing::Fixture::new(move |ui| {
        craft_menu::MenuBarUi::new(&bar).mnemonics(commands::MNEMONICS).english(commands::english_label).show(ui);
    });
    // Wide enough for every title, so none overflows into the bar's "…" menu.
    f.os(craft_host::target::Os::Windows).room(egui::vec2(1600.0, 600.0));
    f.settle();
    f
}

/// The commands the fixture's runtime fired since the last call, with their sources.
fn fixture_fired(f: &craft_ui::testing::Fixture) -> Vec<(CommandRef, craft_ui::runtime::commands::FireSource)> {
    craft_ui::runtime::frame::take_output(f.ctx()).fired.into_iter().map(|fired| (fired.command, fired.source)).collect()
}

#[test]
fn every_menu_fires_its_rows_as_menu_commands() {
    use craft_ui::runtime::commands::FireSource;
    let a = app();
    // (menu, row, command)
    let rows = [
        ("File", "Show Recordings", "file.show_recordings"),
        ("Edit", "Lock Preview", "view.lock_preview"),
        ("View", "Stats", "view.stats"),
        ("Docks", "Reset Docks", "dock.reset"),
        ("Profile", "New…", "profile.new"),
        ("Scene Collection", "Import…", "collection.import"),
        ("Tools", "Available Encoders", "tools.encoders"),
        ("Help", "Check for Updates…", "app.check_updates"),
    ];
    for (menu, label, id) in rows {
        let mut f = menu_fixture(a.laid_menu_for(craft_menu::Target::Windows));
        let title = f.by_label(menu).unwrap_or_else(|| panic!("no {menu} title"));
        f.click(&title).settle();
        let row = f.by_label(label).unwrap_or_else(|| panic!("no {label} row in {menu}"));
        let _ = fixture_fired(&f);
        f.click(&row).settle();
        assert_eq!(fixture_fired(&f), [(CommandRef::new(id), FireSource::Menu)], "{menu} › {label}");
    }
}

#[test]
fn alt_letters_open_menus_and_run_rows_with_english_letters_where_a_label_has_none() {
    use craft_ui::runtime::commands::FireSource;
    let a = app();
    // Alt+F opens File, and X runs Exit by the letter the menus prefer for it.
    let mut f = menu_fixture(a.laid_menu_for(craft_menu::Target::Windows));
    f.key(Key::F, Modifiers::ALT).settle();
    let _ = fixture_fired(&f);
    f.key(Key::X, Modifiers::NONE).settle();
    assert_eq!(fixture_fired(&f), [(CommandRef::new("app.exit"), FireSource::Menu)]);
    // A Japanese title has no letter a keyboard types: it shows File's letter after it.
    i18n::set_lang(i18n::I18N.from_code("ja").unwrap());
    let bar = a.laid_menu_for(craft_menu::Target::Windows);
    let file = format!("{}(F)", tlc!("menu", "File"));
    let mut f = menu_fixture(bar);
    assert!(f.by_label(&file).is_some(), "no `{file}` title");
    f.key(Key::F, Modifiers::ALT).settle();
    let _ = fixture_fired(&f);
    // The first row, Show Recordings, takes S from its English label.
    f.key(Key::S, Modifiers::NONE).settle();
    let fired = fixture_fired(&f);
    i18n::set_lang(craft_i18n::Lang::EN);
    assert_eq!(fired, [(CommandRef::new("file.show_recordings"), FireSource::Menu)]);
}

#[test]
fn keys_the_window_or_a_text_field_owns_post_no_error() {
    let mut a = app();
    let ctx = crate::test_context();
    frame(&mut a, &ctx, vec![]);
    for (key, modifiers) in [
        (Key::F6, Modifiers::NONE),
        (Key::F6, Modifiers::SHIFT),
        (Key::F10, Modifiers::NONE),
        (Key::Escape, Modifiers::NONE),
        (Key::Tab, Modifiers::NONE),
        (Key::Backspace, Modifiers::NONE),
        (Key::C, Modifiers::COMMAND),
        (Key::X, Modifiers::COMMAND),
        (Key::V, Modifiers::COMMAND),
        (Key::A, Modifiers::COMMAND),
    ] {
        frame(&mut a, &ctx, press(key, modifiers));
        frame(&mut a, &ctx, vec![]);
        let notices: Vec<String> = a.notices.iter().map(|(n, _)| n.msg.english()).collect();
        assert!(notices.is_empty(), "{key:?} with {modifiers:?}: {notices:?}");
    }
}

#[test]
fn window_key_commands_refuse_parameters_and_list_a_key_only_while_it_is_free() {
    let mut a = app();
    let ctx = crate::test_context();
    frame(&mut a, &ctx, vec![]);
    for (params, ok) in [(Value::Null, true), (json!({}), true), (json!({"direction": "back"}), false), (json!(3), false)] {
        let got = run(&mut a, &ctx, "window.next_pane", params.clone());
        assert_eq!(got.is_ok(), ok, "{params}: {got:?}");
        if !ok {
            assert_eq!(got, Err("`window.next_pane` takes no parameters".to_string()));
        }
    }
    let shortcut = |a: &StudioApp, id: &str| a.command_list(Some(id)).into_iter().find(|c| c["id"] == id).map(|c| c["shortcut"].clone());
    assert_eq!(shortcut(&a, "window.next_pane"), Some(json!("F6")));
    assert_eq!(shortcut(&a, "window.focus_menu_bar"), Some(json!("F10")));
    // With F6 bound to Stats, F6 runs Stats, so Next Pane lists no key.
    let f6 = craft_keymap::Binding::parse("F6").unwrap();
    a.keymap.assign(&CommandRef::new("view.stats"), f6, craft_keymap::Policy::Steal).unwrap();
    assert_eq!(shortcut(&a, "view.stats"), Some(json!("F6")));
    assert_eq!(shortcut(&a, "window.next_pane"), Some(Value::Null));
    assert_eq!(shortcut(&a, "window.focus_menu_bar"), Some(json!("F10")));
}

#[test]
fn collections_are_made_switched_renamed_and_removed() {
    let mut a = app();
    let ctx = crate::test_context();
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
    let ctx = crate::test_context();
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
    let ctx = crate::test_context();
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
    let ctx = crate::test_context();
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
    let ctx = crate::test_context();
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
    let ctx = crate::test_context();
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
    let ctx = crate::test_context();
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
    let ctx = crate::test_context();
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
    let ctx = crate::test_context();
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
        let ctx = crate::test_context();
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
                assert_eq!(b - t, 24.0, "{w}: {name} is the standard 24 point control height");
                assert!((t - 4.0).abs() <= 0.5, "{w}: {name} is centered on the row, 4 points below the top, at {t}");
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
        // The title bar keeps its content as far from the end edge as from the start edge, and Go
        // Live keeps the output bar's 8 point margin inside that.
        let stream = parts.iter().find(|(n, _)| n == "stream").map(|(_, r)| r[2]).unwrap_or(0.0);
        let inset = parts.iter().find(|(n, _)| n == "menus").map(|(_, r)| r[0]).unwrap_or(f32::NAN);
        assert!(inset > 0.0, "{w}: the menus start inside the bar, at {inset}");
        assert!((w - stream - output_bar::MARGIN - inset).abs() <= 1.0, "{w}: Go Live ends {inset} + 8 points from the right edge, at {stream}");
    }
}

#[test]
fn the_docks_menu_shows_and_hides_the_output_bar_groups() {
    let mut a = app();
    let ctx = crate::test_context();
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

#[test]
fn the_application_audio_dock_reads_its_lanes_again_when_the_collection_changes() {
    let mut a = app();
    let ctx = crate::test_context();
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
    let ctx = crate::test_context();
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
    app.before_input(ctx);
    let mut out = ctx.run_ui(raw, |ui| app.frame(ui));
    out.textures_delta.clear();
    app.before_input(ctx);
}

#[test]
fn the_dock_strip_is_200_tall_at_1280_and_the_canvas_takes_the_rest() {
    let mut a = app();
    let ctx = crate::test_context();
    for _ in 0..3 {
        frame_at(&mut a, &ctx, egui::vec2(1280.0, 800.0));
    }
    let v = a.ui_inspect(&ctx);
    let strip = rect_of(&v["dock_strip"]["rect"]);
    assert_eq!(strip.height(), 200.0, "{strip:?}");
    assert_eq!(strip.bottom(), 800.0 - 24.0, "the 24 point status bar is under the strip");
    let preview = rect_of(&v["preview"]["rect"]);
    assert!((preview.width() - 953.0).abs() <= 1.0 && (preview.height() - 536.0).abs() <= 1.0, "the canvas is {preview:?}");
    let docks = v["docks"].as_array().cloned().unwrap_or_default();
    let widths: Vec<f32> = docks.iter().map(|d| rect_of(&d["rect"]).width()).collect();
    assert_eq!(widths, [300.0, 340.0, 638.0]);
    for d in &docks {
        let h = rect_of(&d["header"]);
        assert_eq!(h.height(), 28.0, "{d}: the Compact panel header");
        assert_eq!(h.top(), strip.top(), "{d}: the header is at the top of the strip");
    }
}

/// One frame at `size` with `events`, and the accessibility tree it produced.
fn frame_tree(app: &mut StudioApp, ctx: &egui::Context, size: egui::Vec2, events: Vec<Event>) -> craft_ui::testing::access::AccessTree {
    ctx.enable_accesskit();
    let raw = RawInput { events, screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)), ..RawInput::default() };
    app.before_input(ctx);
    let mut out = ctx.run_ui(raw, |ui| app.frame(ui));
    out.textures_delta.clear();
    app.before_input(ctx);
    craft_ui::testing::access::tree_of(&mut out)
}

/// The bounds of the node named `name` in `tree`, and whether it is disabled.
fn named(tree: &craft_ui::testing::access::AccessTree, name: &str) -> Option<(egui::Rect, bool)> {
    let key = tree.keys().into_iter().find(|k| tree.name_of(*k) == Some(name))?;
    Some((tree.bounds_of(key)?, tree.is_disabled_key(key)))
}

#[test]
fn every_header_action_is_visible_at_760_by_480() {
    let mut a = app();
    let ctx = crate::test_context();
    run(&mut a, &ctx, "source.add", json!({"kind": "color", "name": "Back"})).unwrap();
    let size = egui::vec2(760.0, 480.0);
    for _ in 0..2 {
        frame_at(&mut a, &ctx, size);
    }
    let tree = frame_tree(&mut a, &ctx, size, vec![]);
    let v = a.ui_inspect(&ctx);
    // The Scenes and Sources header buttons, found by their names in the accessibility tree, are
    // where `ui.inspect` says they are.
    for (dock, buttons) in [(0, &commands::SCENE_HEADER[..]), (1, &commands::SOURCE_HEADER[..])] {
        let actions = v["docks"][dock]["actions"].as_array().cloned().unwrap_or_default();
        for (name, _, _, tip) in buttons {
            let Some((bounds, _)) = named(&tree, tip) else { panic!("no button named `{tip}`") };
            let reported = actions.iter().find(|x| x["name"] == json!(name)).map(|x| rect_of(&x["rect"]));
            assert_eq!(reported, Some(bounds), "{tip}");
        }
    }
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
    let ctx = crate::test_context();
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

    // A drag that starts 2.5 points below the line, still in its hit band, moves the edge by the
    // distance dragged.
    let top = rect_of(&a.ui_inspect(&ctx)["dock_strip"]["rect"]).top();
    for events in drag(egui::pos2(900.0, top + 3.0), egui::pos2(900.0, top + 3.0 - 20.0)) {
        frame_with(&mut a, &ctx, size, events);
    }
    frame_at(&mut a, &ctx, size);
    assert_eq!(a.store().prefs().docks.height, 280.0, "a drag beside the line does not make the edge jump");

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

    // The same for a divider, with a drag that starts 2.5 points right of its line.
    let scenes = rect_of(&a.ui_inspect(&ctx)["docks"][0]["rect"]);
    for events in drag(egui::pos2(scenes.right() + 3.0, y), egui::pos2(scenes.right() + 23.0, y)) {
        frame_with(&mut a, &ctx, size, events);
    }
    frame_at(&mut a, &ctx, size);
    let d = a.store().prefs().docks;
    assert_eq!((d.scenes_width, d.sources_width), (360.0, 280.0), "a drag beside the divider does not make it jump");

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
    let ctx = crate::test_context();
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
    let ctx = crate::test_context();
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
    let ctx = crate::test_context();
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
    let ctx = crate::test_context();
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
    let ctx = crate::test_context();
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
    let ctx = crate::test_context();
    let size = egui::vec2(1280.0, 800.0);
    run(&mut a, &ctx, "dock.float", json!({"dock": "mixer", "floating": true})).unwrap();
    for _ in 0..2 {
        frame_at(&mut a, &ctx, size);
    }
    let v = a.ui_inspect(&ctx);
    let before = rect_of(&v["floating_docks"][0]["rect"]);
    let bar = rect_of(&v["floating_docks"][0]["bar"]);
    assert!(bar.height() > 0.0 && before.contains_rect(bar), "{bar:?} in {before:?}");
    let from = bar.center();
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

    // The dock's own title still opens the dock's menu.
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
    let ctx = crate::test_context();
    for _ in 0..2 {
        frame_at(&mut b, &ctx, size);
    }
    assert_eq!(rect_of(&b.ui_inspect(&ctx)["floating_docks"][0]["rect"]).min, after.min);
}

#[test]
fn a_smaller_main_window_does_not_change_a_floating_dock_s_saved_place() {
    let mut a = app();
    let ctx = crate::test_context();
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
    let ctx_b = crate::test_context();
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
    // A resize from the left edge saves the new left edge and keeps the saved top.
    let pushed = rect_of(&b.ui_inspect(&ctx_b)["floating_docks"][0]["rect"]);
    let edge = egui::pos2(pushed.left() + 1.0, pushed.center().y);
    for events in drag(edge, edge - egui::vec2(20.0, 0.0)) {
        frame_with(&mut b, &ctx_b, small, events);
    }
    for _ in 0..2 {
        frame_at(&mut b, &ctx_b, small);
    }
    let wider = rect_of(&b.ui_inspect(&ctx_b)["floating_docks"][0]["rect"]);
    assert_eq!(wider.left(), pushed.left() - 20.0, "{pushed:?} -> {wider:?}");
    assert_eq!(b.store().prefs().docks.window_pos(2), Some([wider.left(), 500.0]), "a left-edge resize keeps the saved top");
    b.store.update(|p| {
        let outer = p.docks.window_size[2];
        p.docks.place_window(2, [900.0, 500.0], outer);
    });
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
    let ctx = crate::test_context();
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

    // A drag on the left edge moves that edge, keeps the right one, and saves the new place.
    let edge = egui::pos2(after.left() + 1.0, after.center().y);
    for events in drag(edge, edge - egui::vec2(50.0, 0.0)) {
        frame_with(&mut a, &ctx, size, events);
    }
    for _ in 0..2 {
        frame_at(&mut a, &ctx, size);
    }
    let wider = rect_of(&a.ui_inspect(&ctx)["floating_docks"][0]["rect"]);
    assert_eq!((wider.left(), wider.right(), wider.min.y), (after.left() - 50.0, after.right(), after.min.y), "{after:?} -> {wider:?}");
    assert_eq!(a.store().prefs().docks.window_pos(0), Some([wider.min.x, wider.min.y]));
    assert_eq!(a.store().prefs().docks.window_size[0], [wider.width(), wider.height()]);
}

#[test]
fn a_floating_dock_opens_at_its_saved_size_and_keeps_it() {
    let mut a = app();
    let ctx = crate::test_context();
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
    let ctx = crate::test_context();
    let size = egui::vec2(1280.0, 800.0);
    for _ in 0..2 {
        frame_at(&mut a, &ctx, size);
    }
    let before = a.studio().collection().scenes.len();
    let tree = frame_tree(&mut a, &ctx, size, vec![]);
    let strip = rect_of(&a.ui_inspect(&ctx)["dock_strip"]["rect"]);
    let Some((add, false)) = named(&tree, "Add a scene") else { panic!("no enabled Scenes Add button") };
    // The button is as tall as the header at the top of the strip, so its top is in the strip
    // edge's hit band.
    assert_eq!(add.top(), strip.top(), "the Add button starts at the top of the strip");
    let band = crate::theme::with_root(&ctx, |cx| cx.tokens.size.hit.target) / 2.0;
    assert!(add.top() + 1.0 <= strip.top() + crate::strip::DIVIDER / 2.0 + band, "the press below is in the edge's hit band");
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
    let ctx = crate::test_context();
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
    let ctx = crate::test_context();
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
    let ctx = crate::test_context();
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
    let ctx = crate::test_context();
    a.act(&ctx, Act::AddSource { kind: "text".into() });
    let hud = a.hud();
    assert_eq!(hud["ui"]["properties_open"].as_array().map(Vec::len), Some(1));
    frame(&mut a, &ctx, vec![]);
    let name = &a.studio().collection().sources.last().unwrap().name;
    assert_eq!(name, "Text");
}

/// A click at `at`, one step per frame, at `size`.
fn click_at(app: &mut StudioApp, ctx: &egui::Context, size: egui::Vec2, at: egui::Pos2) {
    let b = |pressed| Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
    for events in [vec![Event::PointerMoved(at)], vec![b(true)], vec![b(false)]] {
        frame_with(app, ctx, size, events);
    }
}

/// Whether the node named `name` in `tree` is shown and enabled; `None` when there is no such node.
fn enabled(tree: &craft_ui::testing::access::AccessTree, name: &str) -> Option<bool> {
    named(tree, name).map(|(_, disabled)| !disabled)
}

#[test]
fn the_scenes_and_sources_header_buttons_are_enabled_when_their_command_can_run() {
    let mut a = app();
    let ctx = crate::test_context();
    let size = egui::vec2(1280.0, 800.0);
    frame_at(&mut a, &ctx, size);
    let tree = frame_tree(&mut a, &ctx, size, vec![]);
    assert_eq!(a.studio().collection().scenes.len(), 1);
    assert_eq!(enabled(&tree, "Add a scene"), Some(true));
    for name in ["Remove the selected scene", "Move the scene up", "Move the scene down"] {
        assert_eq!(enabled(&tree, name), Some(false), "{name} with one scene");
    }
    assert_eq!(enabled(&tree, "Add a source"), Some(true));
    for name in ["Remove the selected source from the scene", "Move the source up", "Move the source down", "Properties"] {
        assert_eq!(enabled(&tree, name), Some(false), "{name} with nothing selected");
    }
    run(&mut a, &ctx, "scene.add", json!({})).unwrap();
    run(&mut a, &ctx, "source.add", json!({"kind": "color", "name": "Back"})).unwrap();
    run(&mut a, &ctx, "source.add", json!({"kind": "color", "name": "Front"})).unwrap();
    let scene = a.studio().engine().edit_scene();
    let items = a.studio().collection().scene(scene).map(|s| s.items.iter().map(|i| i.id).collect::<Vec<_>>()).unwrap_or_default();
    assert_eq!(items.len(), 2);
    // The bottom item: it is drawn last in the dock, so it can move up and not down.
    a.act(&ctx, Act::Select(Some(items[0])));
    frame_at(&mut a, &ctx, size);
    let tree = frame_tree(&mut a, &ctx, size, vec![]);
    for name in ["Remove the selected scene", "Move the scene up", "Remove the selected source from the scene", "Move the source up", "Properties"] {
        assert_eq!(enabled(&tree, name), Some(true), "{name}");
    }
    for name in ["Move the scene down", "Move the source down"] {
        assert_eq!(enabled(&tree, name), Some(false), "{name}");
    }
}

#[test]
fn a_header_command_takes_no_parameters_and_fails_while_its_button_is_disabled() {
    let mut a = app();
    let ctx = crate::test_context();
    let err = run(&mut a, &ctx, "scenes.add", json!({"name": "Intro"})).unwrap_err();
    assert!(err.contains("takes no parameters"), "{err}");
    assert_eq!(a.studio().collection().scenes.len(), 1, "the refused command added nothing");
    let err = run(&mut a, &ctx, "scenes.remove", json!({})).unwrap_err();
    assert!(err.contains("cannot run now"), "{err}");
    assert_eq!(a.studio().collection().scenes.len(), 1);
    assert_eq!(run(&mut a, &ctx, "sources.add", json!({})).unwrap(), json!({"open": true}));
    assert_eq!(run(&mut a, &ctx, "sources.add", Value::Null).unwrap(), json!({"open": false}));
    assert!(run(&mut a, &ctx, "scenes.add", json!({})).is_ok());
    assert_eq!(a.studio().collection().scenes.len(), 2);
}

#[test]
fn a_click_on_a_header_button_runs_its_command_and_add_source_opens_and_closes_its_menu() {
    let mut a = app();
    let ctx = crate::test_context();
    let size = egui::vec2(1280.0, 800.0);
    frame_at(&mut a, &ctx, size);
    let tree = frame_tree(&mut a, &ctx, size, vec![]);
    let Some((add_scene, false)) = named(&tree, "Add a scene") else { panic!("no enabled Add a scene button") };
    click_at(&mut a, &ctx, size, add_scene.center());
    assert_eq!(a.studio().collection().scenes.len(), 2, "the click added a scene");
    let tree = frame_tree(&mut a, &ctx, size, vec![]);
    let Some((add_source, false)) = named(&tree, "Add a source") else { panic!("no enabled Add a source button") };
    click_at(&mut a, &ctx, size, add_source.center());
    assert!(a.lists.add_open, "the click opened the Add menu");
    let tree = frame_tree(&mut a, &ctx, size, vec![]);
    let kinds: Vec<String> = a.studio().registry().sources().iter().map(|p| tl!(p.name()).to_string()).collect();
    assert!(!kinds.is_empty() && kinds.iter().all(|k| named(&tree, k).is_some()), "the menu lists the source types: {kinds:?}");
    click_at(&mut a, &ctx, size, add_source.center());
    assert!(!a.lists.add_open, "a second click closed it");
}

#[test]
fn the_audio_docks_headers_are_title_rows_named_by_their_titles() {
    let mut a = app();
    let ctx = crate::test_context();
    let size = egui::vec2(1280.0, 800.0);
    run(&mut a, &ctx, "dock.app_audio", json!({})).unwrap();
    for _ in 0..2 {
        frame_at(&mut a, &ctx, size);
    }
    let tree = frame_tree(&mut a, &ctx, size, vec![]);
    let v = a.ui_inspect(&ctx);
    for (k, title) in [(2, "Audio Mixer"), (3, "Application Audio")] {
        let d = &v["docks"][k];
        assert_eq!(d["actions"], json!([]), "{title} has no header actions");
        let Some((heading, _)) = named(&tree, title) else { panic!("no heading named `{title}`") };
        // With no actions, the title row is the whole room inside the header's padding.
        assert_eq!(rect_of(&d["title"]), heading, "{title}");
        let header = rect_of(&d["header"]);
        assert!(header.contains_rect(heading) && header.height() == 28.0, "{title}: {heading:?} in {header:?}");
    }
}

#[test]
fn a_long_channel_name_is_cut_before_the_audio_mixer_buttons() {
    let mut a = app();
    let ctx = crate::test_context();
    let size = egui::vec2(1280.0, 800.0);
    let global = a.studio().collection().global_audio.clone();
    let Some(&source) = global.first() else { panic!("a new collection has global audio sources") };
    let long = "Desktop audio from the second output device in the studio rack, left and right".repeat(3);
    run(&mut a, &ctx, "source.rename", json!({"source": source, "name": long})).unwrap();
    for _ in 0..3 {
        frame_at(&mut a, &ctx, size);
    }
    // A frame after the rows have measured their buttons, and one where no row has a measure (a
    // channel's first frame), shown without a sizing pass.
    for frame in ["a measured frame", "a first frame"] {
        if frame == "a first frame" {
            ctx.data_mut(|d| d.remove_by_type::<crate::docks::TrailWidth>());
        }
        let tree = frame_tree(&mut a, &ctx, size, vec![]);
        let header = rect_of(&a.ui_inspect(&ctx)["docks"][2]["header"]);
        let Some((name, _)) = named(&tree, &long) else { panic!("{frame}: no text named after the channel") };
        let mut buttons = 0;
        for key in tree.keys() {
            let button = tree.name_of(key);
            if !matches!(button, Some("Mute" | "Audio source actions")) {
                continue;
            }
            let Some(r) = tree.bounds_of(key) else { continue };
            buttons += 1;
            assert!(r.left() >= header.left() && r.right() <= header.right(), "{frame}: {button:?} at {r:?} is inside the dock {header:?}");
            if (r.top()..=r.bottom()).contains(&name.center().y) {
                assert!(name.right() <= r.left(), "{frame}: the cut name {name:?} ends before {button:?} at {r:?}");
            }
        }
        assert_eq!(buttons, 2 * global.len(), "{frame}: a settings and a mute button for each channel");
    }
}

/// The sources that are muted.
fn muted(a: &StudioApp) -> Vec<SourceId> {
    a.studio().collection().sources.iter().filter(|s| s.audio.muted).map(|s| s.id).collect()
}

#[test]
fn the_audio_mixer_mute_button_is_named_by_what_a_click_does() {
    let mut a = app();
    let ctx = crate::test_context();
    let size = egui::vec2(1280.0, 800.0);
    frame_at(&mut a, &ctx, size);
    let tree = frame_tree(&mut a, &ctx, size, vec![]);
    assert!(muted(&a).is_empty() && named(&tree, "Unmute").is_none());
    let Some((mute, false)) = named(&tree, "Mute") else { panic!("no enabled Mute button in the Audio Mixer") };
    click_at(&mut a, &ctx, size, mute.center());
    let now = muted(&a);
    assert_eq!(now.len(), 1, "the click muted one channel");
    assert!(a.studio().collection().global_audio.contains(&now[0]), "a global audio source");
    let tree = frame_tree(&mut a, &ctx, size, vec![]);
    let Some((unmute, false)) = named(&tree, "Unmute") else { panic!("the muted channel's button is not named Unmute") };
    assert_eq!(unmute, mute, "the button stays in its place");
    click_at(&mut a, &ctx, size, unmute.center());
    assert!(muted(&a).is_empty(), "a second click unmuted it");
}

#[test]
fn rename_in_an_audio_mixer_channel_menu_renames_a_global_audio_source_in_the_name_prompt() {
    let mut a = app();
    let ctx = crate::test_context();
    let size = egui::vec2(1280.0, 800.0);
    frame_at(&mut a, &ctx, size);
    let global = a.studio().collection().global_audio.clone();
    assert!(global.len() >= 2, "a new collection has two global audio sources: {global:?}");
    let tree = frame_tree(&mut a, &ctx, size, vec![]);
    let Some((gear, false)) = named(&tree, "Audio source actions") else { panic!("no settings button in the Audio Mixer") };
    click_at(&mut a, &ctx, size, gear.center());
    let tree = frame_tree(&mut a, &ctx, size, vec![]);
    for row in ["Properties…", "Filters…"] {
        assert!(named(&tree, row).is_some(), "the channel's menu has {row}");
    }
    let Some((rename, false)) = named(&tree, "Rename") else { panic!("the channel's menu has no Rename row") };
    click_at(&mut a, &ctx, size, rename.center());
    let what = a.naming.as_ref().map(|n| n.what);
    let Some(Named::RenameSource(source)) = what else { panic!("Rename opened no name prompt: {what:?}") };
    assert!(global.contains(&source), "the channel is a global audio source, which the Sources dock does not list");
    assert_eq!(a.ui_inspect(&ctx)["dialogs"]["prompt"]["title"], json!("Rename Source"));
    let name_of = |a: &StudioApp, id: SourceId| a.studio().collection().source(id).map(|s| s.name.clone());
    let other = global.iter().copied().find(|id| *id != source).and_then(|id| name_of(&a, id)).unwrap_or_default();
    assert!(a.answer_dialog(&ctx, true, Some(&other)).is_err(), "the name of another source is refused");
    assert!(a.naming.is_some(), "the prompt stays open after a refusal");
    assert_eq!(a.answer_dialog(&ctx, true, Some("Speakers")), Ok(json!({"dialog": "prompt", "closed": true, "name": "Speakers"})));
    assert_eq!(name_of(&a, source).as_deref(), Some("Speakers"));
    run(&mut a, &ctx, "edit.undo", json!({})).unwrap();
    assert_ne!(name_of(&a, source).as_deref(), Some("Speakers"), "Edit › Undo puts the old name back");
}

#[test]
fn the_sources_dock_lists_the_top_item_first_and_its_eye_and_lock_change_the_item() {
    let mut a = app();
    let ctx = crate::test_context();
    let size = egui::vec2(1280.0, 800.0);
    run(&mut a, &ctx, "source.add", json!({"kind": "color", "name": "Back"})).unwrap();
    run(&mut a, &ctx, "source.add", json!({"kind": "color", "name": "Front"})).unwrap();
    frame_at(&mut a, &ctx, size);
    let tree = frame_tree(&mut a, &ctx, size, vec![]);
    let (Some((front, _)), Some((back, _))) = (named(&tree, "Front"), named(&tree, "Back")) else { panic!("no Front and Back rows") };
    assert!(front.top() < back.top(), "the top item, added last, is listed first: {front:?} {back:?}");
    let scene = a.studio().engine().edit_scene();
    let item = |a: &StudioApp, name: &str| {
        let c = a.studio().collection();
        let source = c.sources.iter().find(|s| s.name == name).map(|s| s.id);
        c.scene(scene).and_then(|s| s.items.iter().find(|i| Some(i.source) == source)).map(|i| (i.visible, i.locked))
    };
    assert_eq!(item(&a, "Back"), Some((true, false)));
    let Some((eye, _)) = named(&tree, "Visibility Back") else { panic!("no eye on the Back row") };
    click_at(&mut a, &ctx, size, eye.center());
    assert_eq!(item(&a, "Back"), Some((false, false)), "the eye hid the item");
    let tree = frame_tree(&mut a, &ctx, size, vec![]);
    let Some((lock, _)) = named(&tree, "Lock Back") else { panic!("no lock on the Back row") };
    click_at(&mut a, &ctx, size, lock.center());
    assert_eq!(item(&a, "Back"), Some((false, true)), "the lock locked the item");
    assert_eq!(item(&a, "Front"), Some((true, false)), "the other item is unchanged");
}

#[test]
fn a_double_click_on_a_scene_renames_it_in_place() {
    let mut a = app();
    let ctx = crate::test_context();
    let size = egui::vec2(1280.0, 800.0);
    frame_at(&mut a, &ctx, size);
    let tree = frame_tree(&mut a, &ctx, size, vec![]);
    let name = a.studio().collection().scenes[0].name.clone();
    let Some((row, _)) = named(&tree, &name) else { panic!("no row named `{name}`") };
    let at = row.center();
    let b = |pressed| Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
    for events in [vec![Event::PointerMoved(at)], vec![b(true)], vec![b(false)], vec![b(true)], vec![b(false)], vec![]] {
        frame_with(&mut a, &ctx, size, events);
    }
    let select_all = press(Key::A, Modifiers::COMMAND);
    frame_with(&mut a, &ctx, size, select_all);
    frame_with(&mut a, &ctx, size, vec![Event::Text("Intro".into())]);
    frame_with(&mut a, &ctx, size, press(Key::Enter, Modifiers::NONE));
    frame_at(&mut a, &ctx, size);
    assert_eq!(a.studio().collection().scenes[0].name, "Intro");
}

#[test]
fn enter_on_a_scene_in_studio_mode_sends_that_scene_to_the_program() {
    let mut a = app();
    let ctx = crate::test_context();
    let size = egui::vec2(1280.0, 800.0);
    run(&mut a, &ctx, "scene.add", json!({})).unwrap();
    run(&mut a, &ctx, "studio.toggle", json!({})).unwrap();
    let scenes: Vec<_> = a.studio().collection().scenes.iter().map(|s| (s.id, s.name.clone())).collect();
    run(&mut a, &ctx, "scene.select", json!({"scene": scenes[0].0})).unwrap();
    frame_at(&mut a, &ctx, size);
    let tree = frame_tree(&mut a, &ctx, size, vec![]);
    let Some((first, _)) = named(&tree, &scenes[0].1) else { panic!("no row named `{}`", scenes[0].1) };
    click_at(&mut a, &ctx, size, first.center());
    assert_eq!(a.studio().engine().edit_scene(), scenes[0].0);
    // The keyboard moves to the second scene without picking it, and Enter sends it.
    frame_with(&mut a, &ctx, size, press(Key::ArrowDown, Modifiers::COMMAND));
    frame_with(&mut a, &ctx, size, press(Key::Enter, Modifiers::NONE));
    frame_at(&mut a, &ctx, size);
    assert_eq!(a.studio().collection().program_scene, scenes[1].0);
}

#[test]
fn the_program_scene_carries_a_program_mark_in_studio_mode() {
    let mut a = app();
    let ctx = crate::test_context();
    let size = egui::vec2(1280.0, 800.0);
    frame_at(&mut a, &ctx, size);
    let tree = frame_tree(&mut a, &ctx, size, vec![]);
    assert!(named(&tree, "Program").is_none(), "no mark outside studio mode");
    run(&mut a, &ctx, "studio.toggle", json!({})).unwrap();
    frame_at(&mut a, &ctx, size);
    let tree = frame_tree(&mut a, &ctx, size, vec![]);
    let scene = a.studio().collection().scenes[0].name.clone();
    let (Some((mark, _)), Some((row, _))) = (named(&tree, "Program"), named(&tree, &scene)) else { panic!("no mark on the program scene") };
    assert!(row.intersects(mark), "the mark is on the program scene's row: {mark:?} {row:?}");
}

#[test]
fn unknown_commands_and_links_are_errors() {
    let mut a = app();
    let ctx = crate::test_context();
    assert!(run(&mut a, &ctx, "nope.nothing", json!({})).is_err());
    assert!(run(&mut a, &ctx, "app.links", json!({"link": "elsewhere"})).is_err());
    assert!(run(&mut a, &ctx, "help.unknown", json!({})).unwrap_err().contains("unknown command"));
    assert!(a.hud()["ui"]["notices"].as_array().is_some_and(|n| !n.is_empty()));
}

#[test]
fn about_draws_credits_and_opens_the_licenses() {
    let mut a = app();
    let ctx = crate::test_context();
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
    let ctx = crate::test_context();
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
    app.before_input(ctx);
    let mut out = ctx.run_ui(raw, |ui| app.frame(ui));
    out.textures_delta.clear();
    app.before_input(ctx);
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
    let ctx = crate::test_context();
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
    let ctx = crate::test_context();
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
    let is_rtl = |ctx: &egui::Context| ctx.global_style().right_to_left;
    let mut a = app();
    a.set_language("en");
    let ctx = crate::test_context();
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
    app.before_input(ctx);
    let mut out = ctx.run_ui(raw, |ui| app.frame(ui));
    out.textures_delta.clear();
}

#[test]
fn the_canvas_takes_the_size_of_the_display_the_window_is_on() {
    let mut a = app();
    let ctx = crate::test_context();
    frame(&mut a, &ctx, vec![]);
    let e = run(&mut a, &ctx, "video.canvas_from_display", json!({})).unwrap_err();
    assert!(e.contains("not known"), "{e}");
    assert_eq!(a.studio().canvas(), (1920, 1080), "nothing changed");

    frame_on_display(&mut a, &ctx, egui::vec2(1512.0, 982.0), 2.0);
    a.open_settings(crate::settings::Tab::Video);
    let r = run(&mut a, &ctx, "video.canvas_from_display", json!({})).unwrap();
    assert_eq!(r, json!({"width": 3024, "height": 1964, "changed": true}));
    assert_eq!(a.studio().canvas(), (3024, 1964));
    assert_eq!(a.studio().profile().video.output_width, 1920, "the output size stays");
    let saved = a.store().load_profile(DEFAULT_NAME).unwrap();
    assert_eq!((saved.video.base_width, saved.video.base_height), (3024, 1964), "the profile is saved");
    let draft = &a.settings.as_ref().unwrap().draft.video;
    assert_eq!((draft.base_width, draft.base_height), (3024, 1964), "the open Settings window follows");

    // The same size again changes nothing, but an edited Settings draft follows it.
    a.settings.as_mut().unwrap().draft.video.base_width = 1280;
    let r = run(&mut a, &ctx, "video.canvas_from_display", json!({})).unwrap();
    assert_eq!(r["changed"], json!(false));
    assert_eq!(a.settings.as_ref().unwrap().draft.video.base_width, 3024);
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
fn a_canvas_size_that_cant_be_saved_is_an_error() {
    let dir = temp_dir("canvas-save-fails");
    std::fs::create_dir_all(&dir).unwrap();
    let (store, _) = Store::open(craft_settings::Location::Dir(dir.clone()));
    let mut a = StudioApp::boot(store, None, Vec::new());
    // A file where the profiles folder goes makes the save fail.
    let profiles = dir.join("profiles");
    let _ = std::fs::remove_dir_all(&profiles);
    std::fs::write(&profiles, b"").unwrap();
    let ctx = crate::test_context();
    frame_on_display(&mut a, &ctx, egui::vec2(1280.0, 720.0), 1.0);
    let r = run(&mut a, &ctx, "video.canvas_from_display", json!({}));
    // Once the profiles folder can be written, the same size again is saved.
    std::fs::remove_file(&profiles).unwrap();
    let again = run(&mut a, &ctx, "video.canvas_from_display", json!({}));
    let saved = a.store().load_profile(DEFAULT_NAME).map(|p| (p.video.base_width, p.video.base_height));
    let _ = std::fs::remove_dir_all(&dir);
    let e = r.unwrap_err();
    assert!(e.contains("could not be saved"), "{e}");
    assert_eq!(again.unwrap()["changed"], json!(false));
    assert_eq!(saved, Ok((1280, 720)));
}

#[test]
fn the_canvas_from_a_display_refuses_bad_parameters() {
    let mut a = app();
    let ctx = crate::test_context();
    frame(&mut a, &ctx, vec![]);
    let e = run(&mut a, &ctx, "video.canvas_from_display", json!({"display": 3})).unwrap_err();
    assert!(e.contains("display id"), "{e}");
    let e = run(&mut a, &ctx, "video.canvas_from_display", json!({"display": null})).unwrap_err();
    assert!(e.contains("display id"), "{e}");
    let e = run(&mut a, &ctx, "video.canvas_from_display", json!({"display": "no such display \u{1}/.."})).unwrap_err();
    assert!(e.contains("there is no display"), "{e}");
    assert_eq!(a.studio().canvas(), (1920, 1080));
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
    let ctx = crate::test_context();
    let e = run(&mut a, &ctx, "dock.layout_apply", json!({"name": "Talk"})).unwrap_err();
    assert_eq!(e, "there is no dock layout named `Talk`");
    let last_notice = |a: &StudioApp| a.hud()["ui"]["notices"].as_array().and_then(|n| n.last()).cloned();
    assert_eq!(last_notice(&a), Some(json!({"text": e, "error": true})));
    // The next frame posts the notice's card in German, in the error tone, and the control
    // channel still reads it in English.
    frame(&mut a, &ctx, vec![]);
    assert_eq!(notice_cards(&ctx).last(), Some(&("es gibt kein Dock-Layout namens `Talk`".to_owned(), Tone::Bad)));
    assert_eq!(last_notice(&a), Some(json!({"text": e, "error": true})));
    // A language change shows the card on screen in the new language, once.
    a.set_language("en");
    frame(&mut a, &ctx, vec![]);
    let talk = |cards: Vec<(String, Tone)>| cards.into_iter().filter(|(text, _)| text.contains("Talk")).collect::<Vec<_>>();
    assert_eq!(talk(notice_cards(&ctx)), vec![(e.clone(), Tone::Bad)]);
    a.set_language("de");

    // A name the prompt refuses stays in the prompt, translated, and the control channel gets it
    // in English.
    run(&mut a, &ctx, "dock.layout_save", json!({})).unwrap();
    let e = a.answer_dialog(&ctx, true, Some("a/b")).unwrap_err();
    assert_eq!(e, "a name can't contain '/'");
    assert_eq!(a.ui_inspect(&ctx)["dialogs"]["prompt"]["error"], json!(e));
    let id = a.naming.as_ref().and_then(|n| n.dialog).unwrap();
    let held = a.window_dialogs.as_ref().and_then(|h| h.with(|h| h.held_state(id)));
    let Some(craft_ui::dialog::HeldOk::Refused(shown)) = held else { panic!("the prompt shows no refusal: {held:?}") };
    assert_eq!(shown.as_str(), "ein Name darf '/' nicht enthalten");
    let e = canvas_for((8, 4)).unwrap_err();
    assert_eq!(e.english(), "a 8 × 4 display can't be the canvas: each side must be from 16 to 8192 pixels");
    assert_eq!(e.shown(), "ein Bildschirm mit 8 × 4 kann nicht die Leinwand sein: jede Seite muss 16 bis 8192 Pixel haben");
    a.set_language("en");
}

/// The text and tone of each notice card in the window's toast queue, oldest first.
fn notice_cards(ctx: &egui::Context) -> Vec<(String, Tone)> {
    toast::cards(ctx, &WindowKey::main())
        .into_iter()
        .map(|c| (c.toast.title.map(|t| t.as_str().to_owned()).unwrap_or_default(), c.toast.tone))
        .collect()
}

/// Runs one frame at `seconds` on the frames' clock.
fn frame_at_time(app: &mut StudioApp, ctx: &egui::Context, seconds: f64) {
    let screen_rect = Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 800.0)));
    let raw = RawInput { screen_rect, time: Some(seconds), ..RawInput::default() };
    app.before_input(ctx);
    let mut out = ctx.run_ui(raw, |ui| app.frame(ui));
    out.textures_delta.clear();
}

#[test]
fn an_error_notice_stays_24_seconds_and_any_other_8() {
    let mut a = app();
    let ctx = crate::test_context();
    a.notify(Msg::plain("Recording saved"), false);
    a.notify(Msg::plain("The recording could not start"), true);
    let texts = |a: &StudioApp, ctx: &egui::Context| {
        let hud: Vec<String> =
            a.hud()["ui"]["notices"].as_array().into_iter().flatten().filter_map(|n| n["text"].as_str().map(str::to_owned)).collect();
        let cards: Vec<String> = notice_cards(ctx).into_iter().map(|(text, _)| text).collect();
        (hud, cards)
    };
    let both = vec!["Recording saved".to_owned(), "The recording could not start".to_owned()];
    let error = vec!["The recording could not start".to_owned()];
    let none = Vec::new();
    // The control channel lists a notice until the frame after its card left (`listed` false).
    let steps = [(0.0, &both, true), (0.0, &both, true), (7.9, &both, true), (8.1, &error, false), (8.2, &error, true)];
    let steps = steps.into_iter().chain([(23.9, &error, true), (24.1, &none, false), (24.2, &none, true)]);
    for (seconds, shown, listed) in steps {
        frame_at_time(&mut a, &ctx, seconds);
        let (hud, cards) = texts(&a, &ctx);
        assert_eq!(&cards, shown, "the cards at {seconds} s");
        if listed {
            assert_eq!(&hud, shown, "the notices at {seconds} s");
        }
    }
    assert!(a.notices.is_empty());
    // Five notices posted at once: the newest three show, and the control channel lists those.
    for n in 1..=5 {
        a.notify(Msg::plain(format!("Notice {n}")), false);
    }
    a.post_notices(&ctx);
    let newest: Vec<String> = (3..=5).map(|n| format!("Notice {n}")).collect();
    assert_eq!(texts(&a, &ctx), (newest.clone(), newest));
}
