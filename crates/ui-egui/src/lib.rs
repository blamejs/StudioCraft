//! StudioCraft's window.
//!
//! [`StudioApp`] draws the menus, the preview (and the program in studio mode), the docks, the
//! status bar and the dialogs, and turns every click and shortcut into a command: an engine
//! command run through [`Studio::run`], or one of the window's own (`commands::UI_COMMANDS`).
//! Nothing here changes the scene collection directly.

#[macro_use]
pub mod i18n;
pub mod app_icon;
pub mod commands;
mod control;
pub mod credits;
mod docks;
pub mod fonts;
mod forms;
pub mod hardware;
mod icons;
pub mod kit;
mod output_bar;
mod preview;
mod scopes;
mod settings;
pub mod store;
pub mod strip;
pub mod theme;
mod title_bar;
mod updates_ui;

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use craft_keymap::egui::Dispatcher;
use craft_keymap::{Active, CommandRef, Keymap};
use craft_menu::Gate;
use craft_ui::layout::{ResizeHandle, ResizeHandleStyle, SplitAxis};
use egui::{Color32, RichText, Ui};
use serde_json::{Value, json};
use studiocraft_media::mix::MeterBallistics;
use studiocraft_model::{ItemId, Profile, SceneCollection, SceneId, SettingsSchema, SourceId, Transform, Vec2};
use studiocraft_platform::display::DisplayInfo;
use studiocraft_render::Gpu;
use studiocraft_render::device::{Candidate, Preference};
use studiocraft_studio::files::read_collection_file;
use studiocraft_studio::{OutputKind, Studio, UiAction, builtin_registry, default_collection};

use crate::forms::{Browse, Filters, Properties};
use crate::i18n::Msg;
use crate::preview::{EditItem, Editor, Textures, View};
use crate::settings::{Pick, SettingsWindow, Tab};
use crate::store::{DEFAULT_NAME, Store, check_name};

pub const APP_NAME: &str = "StudioCraft";
/// The app's id in the suite: the name of its settings folder and the stem of its release files.
pub const APP_ID: &str = "studiocraft";
/// The macOS bundle id, the Flatpak id, the Linux desktop-file id and the window's app id.
pub const BUNDLE_ID: &str = "ai.storyteller.studiocraft";

/// StudioCraft as the updater knows it: where its releases are listed, the accent color of the
/// update prompt, and the icon the prompt shows and the menu entry beside an AppImage uses.
pub const UPDATE_APP: craft_update::app::App = craft_update::app::App {
    accent: Some([0xd9, 0x48, 0x3b]),
    icon_png: Some(app_icon::ICON_128_PNG),
    ..craft_update::app::App::new(APP_ID, APP_NAME, "Recording and live streaming", "blamejs/StudioCraft")
};

/// Something the docks, menus or preview asked for.
pub(crate) enum Act {
    Run(CommandRef),
    Select(Option<ItemId>),
    Properties(SourceId),
    Filters(SourceId),
    AddSource {
        kind: String,
    },
    /// End the current drag in the undo history.
    Seal,
}

impl Act {
    pub(crate) fn run(id: &str, params: Value) -> Act {
        Act::Run(CommandRef::with_params(id, params))
    }
}

/// What is being renamed in place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Rename {
    Scene(SceneId),
    Source(SourceId),
}

/// What a file dialog's answer is for.
enum Then {
    SourcePath { source: SourceId, key: String },
    Settings(Pick),
    ImportCollection,
    ExportCollection,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Named {
    NewProfile,
    DuplicateProfile,
    RenameProfile,
    NewCollection,
    DuplicateCollection,
    RenameCollection,
    SaveDockLayout,
}

impl Named {
    /// The title of the window asking for the name.
    fn title(self) -> &'static str {
        match self {
            Named::NewProfile => "New Profile",
            Named::DuplicateProfile => "Duplicate Profile",
            Named::RenameProfile => "Rename Profile",
            Named::NewCollection => "New Scene Collection",
            Named::DuplicateCollection => "Duplicate Scene Collection",
            Named::RenameCollection => "Rename Scene Collection",
            Named::SaveDockLayout => "Save Dock Layout",
        }
    }
}

/// A small window asking for a name.
struct Prompt {
    what: Named,
    text: String,
    error: Option<Msg>,
}

/// A status bar message: kept in English, translated when the status bar shows it.
struct Note {
    msg: Msg,
    error: bool,
}

/// How long a notice stays in the status bar.
const NOTICE_S: u64 = 8;
/// How long after the last change the scene collection is saved.
const AUTOSAVE: Duration = Duration::from_millis(800);
/// How far the dock strip's top-edge hit band reaches above and below its 1 point line.
const EDGE_HIT_MARGIN: f32 = 3.5;

/// The window's graphics device: the adapter preference it opened with, and what happens after
/// it is lost.
#[derive(Default)]
struct WindowGpu {
    /// The preference the window opened with: the saved one, or software after repeated losses.
    opened_with: Preference,
    /// What the user was told about the adapter the window opened on.
    notices: Vec<Msg>,
    /// After a loss the app may start itself again: its settings are kept on disk and no script
    /// drives the window.
    may_relaunch: bool,
    /// The loss of the current device was handled.
    loss_seen: bool,
    /// The window closes so that the app starts again on a new device.
    relaunch: bool,
}

pub struct StudioApp {
    studio: Studio,
    store: Store,
    keymap: Keymap,
    dispatcher: Dispatcher,
    dialogs: Option<craft_dialogs::Dialogs<Then>>,
    render: Option<egui_wgpu::RenderState>,
    textures: Textures,
    editor: Editor,
    selected: Option<ItemId>,
    renaming: Option<(Rename, String)>,
    meters: HashMap<SourceId, MeterBallistics>,
    /// The Application Audio dock's lanes and meters, apart from the Audio Mixer's meters so that
    /// a source shown in both docks is advanced once per frame in each.
    lanes: docks::LaneCache,
    last_frame: Option<Instant>,
    properties: Vec<Properties>,
    filters: Vec<Filters>,
    settings: Option<SettingsWindow>,
    transition_schema: Option<SettingsSchema>,
    prompt: Option<Prompt>,
    about: bool,
    /// How the About window's Credits list names people.
    credit_names: credits::NameMode,
    /// The Licenses window, with the license it shows.
    licenses: Option<credits::License>,
    encoders: Option<Vec<(String, String, bool)>>,
    /// Why the Available Encoders window lists no ffmpeg encoder: the ffmpeg program was not found.
    encoders_ffmpeg: Option<Msg>,
    /// The Scopes window's feed and thread while it is open.
    scopes: Option<scopes::Scopes>,
    confirm_exit: bool,
    exit_confirmed: bool,
    /// A close was canceled because the scene collection could not be saved; the next one goes ahead.
    close_despite_failed_save: bool,
    /// False while the collection made at start has never been written: it is saved at exit
    /// even with no changes.
    collection_on_disk: bool,
    notices: VecDeque<(Note, Instant)>,
    collections: Vec<String>,
    profiles: Vec<String>,
    changed_at: Option<(u64, Instant)>,
    fullscreen: bool,
    frames: u64,
    /// Where the editable view was drawn last: its rectangle and screen points per canvas pixel.
    last_view: Option<(egui::Rect, f32)>,
    /// Where each dock in the strip was drawn last: its index in `control::DOCK_NAMES`, its
    /// rectangle and what its header drew.
    dock_rects: Vec<(usize, egui::Rect, docks::Drawn)>,
    /// The same for each dock in a window of its own, with the window's rectangle.
    floating_rects: Vec<(usize, egui::Rect, docks::Drawn)>,
    /// Where each floating dock's window is this session, by dock index, while it differs from the
    /// place saved when the pointer was last released.
    dock_window_at: [Option<egui::Pos2>; store::DOCK_COUNT],
    /// Whether the user moved or resized each floating dock's window since its place was saved.
    dock_window_touched: [bool; store::DOCK_COUNT],
    /// Where each floating dock's window was drawn last frame, and the main window's size then.
    dock_window_drawn: [Option<egui::Rect>; store::DOCK_COUNT],
    dock_viewport: Option<egui::Vec2>,
    /// Counts the dock windows' resets this session. It is part of their ids, so a window opened
    /// after a reset starts at its default size instead of the size egui kept for the old one.
    dock_window_gen: u32,
    /// Where the dock strip was drawn last.
    dock_strip: Option<egui::Rect>,
    /// Where the top row and the parts of the output bar were drawn last.
    output_bar: output_bar::Drawn,
    /// Help › Check for Updates, the check at start and the update prompt.
    updates: updates_ui::Runtime,
    /// The window buttons when the app draws the window frame; `None` when the desktop draws it.
    title_bar: Option<craft_window::Layout>,
    /// The interface language [`Self::install_fonts`] last built the fonts for; `None` until it
    /// runs. The fonts are built again when the language changes.
    fonts_for: Option<craft_i18n::Lang>,
    window_gpu: WindowGpu,
    /// Whether [`Self::logic`] ticked the studio since the last frame was drawn.
    ticked_in_logic: bool,
}

impl StudioApp {
    /// Open the collection and profile used last (or new ones), and start the studio on the
    /// window's graphics device.
    pub fn boot(mut store: Store, render: Option<egui_wgpu::RenderState>, problems: Vec<String>) -> StudioApp {
        let ffmpeg = Arc::new(RwLock::new(String::new()));
        let mut notices: Vec<Msg> = Vec::new();
        let registry = match builtin_registry(&ffmpeg) {
            Ok(r) => r,
            Err(e) => {
                notices.push(tmsg!("A plug-in could not be loaded: {error}", error = e));
                studiocraft_plugin::Registry::new()
            }
        };
        let prefs = store.prefs().clone();
        i18n::set_lang(i18n::resolve(prefs.language()));
        let profile_name = if check_name(&prefs.profile).is_ok() { prefs.profile.clone() } else { DEFAULT_NAME.to_string() };
        // A file that exists but cannot be read is kept: the new profile or collection that
        // replaces it gets a name of its own.
        // Only a new profile or collection is written at start. One read from a file stays as it
        // is on disk, repairs included, until it is changed or saved.
        let (profile, new_profile) = match store.load_profile(&profile_name) {
            Ok(p) => (p, false),
            Err(e) => {
                let existing = store.profiles();
                if !existing.is_empty() {
                    notices.push(Msg::plain(e));
                }
                (Profile { name: free_name(&profile_name, |n| existing.iter().any(|p| p.eq_ignore_ascii_case(n))), ..Profile::default() }, true)
            }
        };
        let collection_name = if check_name(&prefs.collection).is_ok() { prefs.collection.clone() } else { DEFAULT_NAME.to_string() };
        let (collection, new_collection) = match store.load_collection(&collection_name) {
            Ok(c) => (c, false),
            Err(e) => {
                let existing = store.collections();
                if !existing.is_empty() {
                    notices.push(Msg::plain(e));
                }
                (default_collection(&free_name(&collection_name, |n| existing.iter().any(|c| c.eq_ignore_ascii_case(n)))), true)
            }
        };
        let gpu = render.as_ref().map(|rs| Gpu::new(rs.device.clone(), rs.queue.clone()).with_adapter(Candidate::of(&rs.adapter, None)));
        let studio = Studio::new(gpu, collection, profile, Arc::new(registry), ffmpeg);
        store.update(|p| {
            p.profile = studio.profile().name.clone();
            p.collection = studio.collection().name.clone();
        });
        let keymap = commands::keymap(store.prefs().hotkeys.clone());
        let mut app = StudioApp {
            studio,
            store,
            keymap,
            dispatcher: Dispatcher::new(),
            dialogs: None,
            render,
            textures: Textures::default(),
            editor: Editor::default(),
            selected: None,
            renaming: None,
            meters: HashMap::new(),
            lanes: docks::LaneCache::default(),
            last_frame: None,
            properties: Vec::new(),
            filters: Vec::new(),
            settings: None,
            transition_schema: None,
            prompt: None,
            about: false,
            credit_names: credits::NameMode::default(),
            licenses: None,
            encoders: None,
            encoders_ffmpeg: None,
            scopes: None,
            confirm_exit: false,
            exit_confirmed: false,
            close_despite_failed_save: false,
            collection_on_disk: true,
            notices: VecDeque::new(),
            collections: Vec::new(),
            profiles: Vec::new(),
            changed_at: None,
            fullscreen: false,
            frames: 0,
            last_view: None,
            dock_rects: Vec::with_capacity(control::DOCK_NAMES.len()),
            floating_rects: Vec::new(),
            dock_window_at: [None; store::DOCK_COUNT],
            dock_window_touched: [false; store::DOCK_COUNT],
            dock_window_drawn: [None; store::DOCK_COUNT],
            dock_viewport: None,
            dock_window_gen: 0,
            dock_strip: None,
            output_bar: output_bar::Drawn::default(),
            updates: updates_ui::Runtime::default(),
            title_bar: None,
            fonts_for: None,
            window_gpu: WindowGpu::default(),
            ticked_in_logic: false,
        };
        if new_collection {
            app.collection_on_disk = app.save_collection();
        }
        if new_profile {
            app.save_profile();
        }
        app.refresh_lists();
        for msg in problems.into_iter().map(Msg::plain).chain(notices) {
            app.notify(msg, true);
        }
        app
    }

    pub fn studio(&self) -> &Studio {
        &self.studio
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Draw the window frame with these window buttons (the menu strip becomes the title bar), or
    /// leave it to the desktop (`None`, the default). Set once, before the first frame: the
    /// window's decorations are chosen when it opens.
    pub fn set_title_bar(&mut self, buttons: Option<craft_window::Layout>) {
        self.title_bar = buttons;
    }

    /// Whether the app draws the window frame.
    pub fn draws_title_bar(&self) -> bool {
        self.title_bar.is_some()
    }

    /// The window opened its graphics device for `opened_with` ([`hardware::startup`]), and the
    /// user is told `notices` about the choice. With `may_relaunch` a lost device closes the
    /// window so that the app starts again ([`StudioApp::relaunch_requested`]). It does not when
    /// the window drew on a software adapter, or opened for software and has now lost its device
    /// [`hardware::KEPT_LOSSES`] times in the recovery window (a machine with no software
    /// adapter), since starting again would end the same way; the status bar then asks the user to
    /// restart.
    pub fn set_window_gpu(&mut self, opened_with: Preference, notices: Vec<Msg>, may_relaunch: bool) {
        for n in &notices {
            self.notify(n.clone(), true);
        }
        self.window_gpu = WindowGpu { opened_with, notices, may_relaunch, loss_seen: false, relaunch: false };
    }

    /// Whether the window closed because its graphics device was lost and the app should start
    /// again. Everything was saved before it closed.
    pub fn relaunch_requested(&self) -> bool {
        self.window_gpu.relaunch
    }

    /// After the window's graphics device was lost: record the loss for [`hardware::startup`],
    /// and close the window to start again, or ask the user to restart (see
    /// [`StudioApp::set_window_gpu`]).
    fn watch_window_gpu(&mut self, ctx: &egui::Context) {
        if self.window_gpu.loss_seen || self.studio.gpu_lost().is_none() || self.studio.gpu().is_some_and(Gpu::can_reopen) {
            return;
        }
        self.window_gpu.loss_seen = true;
        let now = hardware::now_secs();
        self.store.update(|p| hardware::record_loss(&mut p.graphics_losses, now));
        // The adapter the window drew with, not the one asked for: a software request on a
        // machine without a software adapter draws on hardware.
        let on_software = self.studio.gpu().and_then(Gpu::adapter).is_some_and(Candidate::is_software);
        let no_way_out = self.window_gpu.opened_with == Preference::Software && hardware::repeated_losses(&self.store.prefs().graphics_losses, now);
        // The window closes without asking, so only once everything was saved; otherwise the
        // status bar says what could not be saved and the window stays open.
        if self.window_gpu.may_relaunch && !on_software && !no_way_out && self.save_all() {
            self.window_gpu.relaunch = true;
            self.exit_confirmed = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        } else {
            self.notify(tmsg!("Nothing can be drawn until StudioCraft starts again"), true);
        }
    }

    pub fn keymap(&self) -> &Keymap {
        &self.keymap
    }

    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Where the editable view (the program, or the preview in studio mode) was drawn last:
    /// its rectangle in the window and screen points per canvas pixel.
    pub fn preview_view(&self) -> Option<(egui::Rect, f32)> {
        self.last_view
    }

    fn notify(&mut self, msg: impl Into<Msg>, error: bool) {
        let msg = msg.into();
        if error {
            log::warn!("{}", msg.english());
        } else {
            log::info!("{}", msg.english());
        }
        self.notices.push_back((Note { msg, error }, Instant::now()));
        while self.notices.len() > 20 {
            self.notices.pop_front();
        }
    }

    fn refresh_lists(&mut self) {
        self.collections = self.store.collections();
        if !self.collections.contains(&self.studio.collection().name) {
            self.collections.push(self.studio.collection().name.clone());
        }
        self.profiles = self.store.profiles();
        if !self.profiles.contains(&self.studio.profile().name) {
            self.profiles.push(self.studio.profile().name.clone());
        }
    }

    /// Save the current scene collection; whether it was saved. A failure is shown as a notice.
    fn save_collection(&mut self) -> bool {
        match self.store.save_collection(self.studio.collection()) {
            Ok(()) => {
                self.studio.mark_saved();
                self.collection_on_disk = true;
                true
            }
            Err(e) => {
                self.notify(tmsg!("The scene collection could not be saved: {error}", error = e), true);
                false
            }
        }
    }

    /// Save the current scene collection before another one replaces it. When the save fails,
    /// the error is returned and the current collection stays open with its changes.
    fn save_before_leaving(&mut self) -> Result<(), Msg> {
        self.store.save_collection(self.studio.collection()).map_err(|e| tmsg!("The scene collection could not be saved: {error}", error = e))?;
        self.studio.mark_saved();
        Ok(())
    }

    /// Save the current profile; whether it was saved. A failure is shown as a notice.
    fn save_profile(&mut self) -> bool {
        match self.store.save_profile(self.studio.profile()) {
            Ok(()) => true,
            Err(e) => {
                self.notify(tmsg!("The profile could not be saved: {error}", error = e), true);
                false
            }
        }
    }

    /// Make the canvas `size` (each side rounded down to an even number) and save the profile.
    /// The output size stays as it is. An open Settings window shows the new canvas size. A
    /// profile that can't be saved is an error, and the new size stays in use until the app quits.
    fn set_canvas(&mut self, size: (u32, u32)) -> Result<Value, Msg> {
        let mut p = self.studio.profile().clone();
        let v = studiocraft_model::VideoSettings { base_width: size.0, base_height: size.1, ..p.video }.sanitized();
        let (width, height) = (v.base_width, v.base_height);
        p.video.base_width = width;
        p.video.base_height = height;
        let changed = self.studio.canvas() != (width, height);
        if changed {
            self.studio.set_profile(p).map_err(|e| e.to_string())?;
        }
        if let Some(w) = &mut self.settings {
            w.draft.video.base_width = width;
            w.draft.video.base_height = height;
        }
        // Saved even when the size was already in use, so a save that failed before is retried.
        self.store.save_profile(self.studio.profile()).map_err(|e| tmsg!("The profile could not be saved: {error}", error = e))?;
        Ok(json!({"width": width, "height": height, "changed": changed}))
    }

    /// Save everything now (the app is closing); whether the scene collection, the profile and
    /// the preferences were saved. A failure is shown as a notice.
    pub fn save_all(&mut self) -> bool {
        let collection = (self.collection_on_disk && !self.studio.engine().is_dirty()) || self.save_collection();
        let profile = self.save_profile();
        let preferences = match self.store.flush() {
            Ok(()) => true,
            Err(e) => {
                self.notify(tmsg!("The preferences could not be saved: {error}", error = e), true);
                false
            }
        };
        collection && profile && preferences
    }

    /// The selected item, if it is in the scene being edited.
    fn selection(&self) -> Option<(ItemId, SourceId, Transform)> {
        let c = self.studio.collection();
        let it = c.scene(self.studio.engine().edit_scene())?.item(self.selected?)?;
        Some((it.id, it.source, it.transform))
    }

    /// The state scripts and the self-test read: the studio's, and what the window shows.
    pub fn hud(&self) -> Value {
        let mut v = self.studio.state_json();
        let p = self.store.prefs();
        if let Some(o) = v.as_object_mut() {
            o.insert(
                "ui".into(),
                json!({
                    "selected_item": self.selected,
                    "settings_open": self.settings.is_some(),
                    "properties_open": self.properties.iter().map(|p| p.source).collect::<Vec<_>>(),
                    "filters_open": self.filters.iter().map(|f| f.source).collect::<Vec<_>>(),
                    "about_open": self.about,
                    "licenses_open": self.licenses.is_some(),
                    "updates_open": self.updates.window_open || self.updates.updater.prompt_open(),
                    "docks": p.docks,
                    "stats": p.stats,
                    "scopes": {
                        "open": p.scopes,
                        "kind": scopes::kind_of(&p.scope_kind).name(),
                        "computed": self.scopes.as_ref().map_or(0, |s| s.computed),
                        "dropped": self.scopes.as_ref().map_or(0, |s| s.dropped()),
                        "skipped": self.studio.scope_skipped(),
                    },
                    "status_bar": p.status_bar,
                    "lock_preview": p.lock_preview,
                    "profile": self.studio.profile().name,
                    "profiles": self.profiles,
                    "collections": self.collections,
                    "notices": self.notices.iter().map(|(n, _)| json!({"text": n.msg.english(), "error": n.error})).collect::<Vec<_>>(),
                    "language": p.language(),
                    "frames": self.frames,
                    "preview_drawn": self.studio.views().is_some_and(|v| v.program.latest().is_some()),
                }),
            );
        }
        v
    }

    /// Run a command by id: an engine command or one of the window's. An error is shown in the
    /// status bar in the interface language and returned in English.
    pub fn perform(&mut self, ctx: &egui::Context, cmd: &CommandRef) -> Result<Value, String> {
        let r = if commands::is_ui_command(&cmd.id) { self.ui_command(ctx, cmd) } else { self.engine_command(ctx, cmd) };
        r.map_err(|e| {
            let english = e.english();
            self.notify(e, true);
            english
        })
    }

    fn engine_command(&mut self, ctx: &egui::Context, cmd: &CommandRef) -> Result<Value, Msg> {
        let (result, ui) = self.studio.run(&cmd.id, &cmd.params).map_err(|e| e.to_string())?;
        match ui {
            Some(UiAction::OpenSettings) => self.open_settings(Tab::Stream),
            Some(UiAction::Exit) => self.request_exit(ctx),
            None => {}
        }
        if cmd.id == "source.add"
            && let Some(item) = result.get("item").and_then(Value::as_u64)
        {
            self.selected = Some(ItemId(item));
        }
        Ok(result)
    }

    fn open_settings(&mut self, tab: Tab) {
        if self.settings.is_none() {
            let p = self.store.prefs();
            let mut w = SettingsWindow::new(self.studio.profile(), &self.keymap, tab, p.language(), p.check_updates);
            w.system_title_bar = p.system_title_bar;
            w.graphics_adapter = p.graphics_adapter.clone();
            w.adapters = self.render.as_ref().map(|rs| rs.available_adapters.iter().map(|a| Candidate::of(a, None)).collect()).unwrap_or_default();
            w.drawing_with = self.studio.gpu().and_then(Gpu::adapter).map(|a| a.name.clone());
            w.gpu_notices = self.window_gpu.notices.clone();
            w.track_sources = self.studio.audio_track_sources();
            self.settings = Some(w);
        }
    }

    /// Put the settings OK or Apply gave in force; whether the studio took them (it refuses video
    /// changes while an output runs, and then nothing is applied).
    fn apply_settings(&mut self, a: settings::Apply) -> bool {
        match self.studio.set_profile(a.profile) {
            Ok(()) => {
                self.save_profile();
                self.keymap.set_overrides(a.overrides.clone());
                self.store.update(|p| {
                    p.hotkeys = a.overrides;
                    p.check_updates = a.check_updates;
                    p.system_title_bar = a.system_title_bar;
                    if p.graphics_adapter != a.graphics_adapter {
                        // Losses on the old choice say nothing about the new one.
                        p.graphics_adapter = a.graphics_adapter;
                        p.graphics_losses.clear();
                    }
                });
                self.set_language(&a.language);
                true
            }
            Err(e) => {
                self.notify(e.to_string(), true);
                false
            }
        }
    }

    /// Whether the recording, the stream or the replay buffer runs or is stopping.
    fn outputs_running(&self) -> bool {
        [OutputKind::Recording, OutputKind::Streaming, OutputKind::Replay].iter().any(|k| self.studio.output(*k).is_some())
    }

    fn request_exit(&mut self, ctx: &egui::Context) {
        let busy = self.outputs_running();
        if busy && !self.exit_confirmed {
            self.confirm_exit = true;
        } else {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn param_str<'a>(cmd: &'a CommandRef, key: &str) -> Result<&'a str, Msg> {
        cmd.params.get(key).and_then(Value::as_str).ok_or_else(|| tmsg!("`{command}` needs `{key}`", command = cmd.id, key = key))
    }

    /// The `dock` parameter as a dock index, or an error naming the docks.
    fn param_dock(cmd: &CommandRef) -> Result<u8, Msg> {
        let name = Self::param_str(cmd, "dock")?;
        control::DOCK_NAMES.iter().position(|n| *n == name).and_then(|i| u8::try_from(i).ok()).ok_or_else(|| {
            tmsg!("`{command}`: unknown dock `{name}` (one of {docks})", command = cmd.id, name = name, docks = control::DOCK_NAMES.join(", "))
        })
    }

    /// The optional `path` parameter: `None` when it is left out, an error when it is not a
    /// non-empty string.
    fn param_path(cmd: &CommandRef) -> Result<Option<PathBuf>, Msg> {
        match cmd.params.get("path") {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(p)) if !p.trim().is_empty() => Ok(Some(PathBuf::from(p))),
            Some(_) => Err(tmsg!("`{command}`: `path` must be a file path", command = cmd.id)),
        }
    }

    /// Queue a file dialog for a scene collection file, or fail when no dialog can be shown.
    fn ask_collection_path(&mut self, cmd: &CommandRef, dialog: craft_dialogs::FileDialog, then: Then) -> Result<(), Msg> {
        let dialogs = self.dialogs.as_mut().ok_or_else(|| tmsg!("`{command}` needs `path` when no file dialog can be shown", command = cmd.id))?;
        dialogs.ask(dialog.filter("Scene collection", &["json"]), then);
        Ok(())
    }

    /// Read the scene collection file at `path`, add it to the saved collections under a name no
    /// other collection uses, and switch to it.
    fn import_collection(&mut self, path: &Path) -> Result<Value, Msg> {
        let mut c = read_collection_file(path).map_err(|e| tmsg!("The scene collection could not be imported: {error}", error = e))?;
        let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let base = [c.name.as_str(), stem.as_str()].into_iter().find(|n| check_name(n).is_ok()).unwrap_or("Imported").to_string();
        let name = self.unique_collection_name(&base)?;
        c.name = name.clone();
        self.save_before_leaving()?;
        self.store.save_collection(&c).map_err(|e| tmsg!("The scene collection could not be imported: {error}", error = e))?;
        self.load_collection(c);
        self.refresh_lists();
        self.notify(tmsg!("Imported the scene collection `{name}`", name = name), false);
        Ok(json!({"name": name}))
    }

    /// Write the current scene collection to `path`. It is saved in the settings folder first; when
    /// that save fails, the collection stays marked as changed and autosave tries again.
    fn export_collection(&mut self, path: &Path) -> Result<Value, Msg> {
        self.save_collection();
        studiocraft_studio::files::write_collection_file(path, self.studio.collection())
            .map_err(|e| tmsg!("The scene collection could not be exported: {error}", error = e))?;
        self.notify(tmsg!("Exported the scene collection to {path}", path = craft_i18n::ltr(&path.display().to_string())), false);
        Ok(json!({"path": path.to_string_lossy()}))
    }

    /// `base`, or `base 2`, `base 3` and so on, whichever no collection uses (names are compared
    /// without case).
    fn unique_collection_name(&self, base: &str) -> Result<String, Msg> {
        let taken = |n: &str| self.collections.iter().any(|c| c.eq_ignore_ascii_case(n));
        if !taken(base) {
            return Ok(base.to_string());
        }
        let short: String = base.chars().take(90).collect();
        let short = short.trim_end();
        (2..=10_000u32)
            .map(|i| format!("{short} {i}"))
            .find(|n| !taken(n) && check_name(n).is_ok())
            .ok_or_else(|| tmsg!("there is no free name for the scene collection `{name}`", name = base))
    }

    /// Save the Language setting (`auto`, or a language code; a code this build does not offer
    /// is saved as `auto`) and show the interface in that language from the next frame.
    pub fn set_language(&mut self, setting: &str) {
        let setting = i18n::normalize(setting).to_string();
        i18n::set_lang(i18n::resolve(&setting));
        self.store.update(|p| p.language = setting);
    }

    fn toggle_pref(&mut self, f: impl FnOnce(&mut store::Prefs) -> &mut bool) -> Value {
        let mut now = false;
        self.store.update(|p| {
            let b = f(p);
            *b = !*b;
            now = *b;
        });
        json!({"on": now})
    }

    /// Start the scopes thread and the program copy while View › Scopes is on, and stop both when
    /// it is off. A thread that cannot start turns the setting off again.
    fn sync_scopes(&mut self) {
        let on = self.store.prefs().scopes;
        if on && self.scopes.is_none() {
            match scopes::Scopes::start() {
                Ok(s) => {
                    self.studio.set_scope_feed(Some(s.feed()));
                    self.scopes = Some(s);
                }
                Err(e) => {
                    self.store.update(|p| p.scopes = false);
                    self.notify(tmsg!("The scopes could not start: {error}", error = e), true);
                }
            }
        } else if !on && self.scopes.is_some() {
            self.studio.set_scope_feed(None);
            self.scopes = None;
        }
    }

    /// The Scopes window: the chosen scope of the program output, computed on the scopes thread.
    fn scopes_window(&mut self, ctx: &egui::Context, acts: &mut Vec<Act>) {
        self.sync_scopes();
        let kind = scopes::kind_of(&self.store.prefs().scope_kind);
        let has_gpu = self.render.is_some();
        let Some(s) = &mut self.scopes else { return };
        s.pump(kind);
        if s.fresh
            && let Some(p) = &mut s.latest
        {
            s.fresh = false;
            // The scopes thread made the image; it moves into the texture without a copy here.
            if let Some(image) = p.image.take() {
                match &mut s.texture {
                    Some(t) => t.set(image, egui::TextureOptions::LINEAR),
                    None => s.texture = Some(ctx.load_texture("scopes", image, egui::TextureOptions::LINEAR)),
                }
            }
        }
        let mut open = true;
        egui::Window::new(tid!("view.scopes", "Scopes")).id(egui::Id::unique("scopes")).open(&mut open).default_width(520.0).show(ctx, |ui| {
            craft_ui::ComboBox::from_id_salt("scope-kind").selected_text(tl!(kind.label())).show_ui(ui, |ui| {
                for k in scopes::MENU_ORDER {
                    if ui.selectable_label(k == kind, tl!(k.label())).clicked() && k != kind {
                        acts.push(Act::run("scopes.kind", json!({"kind": k.name()})));
                    }
                }
            });
            let latest = s.latest.as_ref().filter(|p| p.kind == kind);
            match latest {
                None if !has_gpu => {
                    ui.weak(tl!("The scopes need a graphics device to read the program."));
                }
                None => {
                    ui.weak(tl!("Waiting for the program picture…"));
                }
                Some(p) => match &p.histogram {
                    Some(h) => scopes::draw_histogram(ui, h),
                    None => {
                        if let Some(t) = &s.texture {
                            let size = scopes::fit((p.width, p.height), ui.available_width().max(64.0), 420.0);
                            let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
                            ui.painter().rect_filled(rect, 0.0, Color32::BLACK);
                            let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
                            ui.painter().image(t.id(), rect, uv, Color32::WHITE);
                        }
                    }
                },
            }
            if let Some(e) = &s.error {
                ui.colored_label(Color32::from_rgb(220, 90, 80), craft_i18n::fmt(tl!("The scope could not be computed: {error}"), &[("error", e)]));
            }
        });
        if !open {
            self.store.update(|p| p.scopes = false);
            self.sync_scopes();
        }
    }

    fn selection_transform(&mut self, f: impl FnOnce(Transform, (u32, u32), (u32, u32)) -> Transform) -> Result<Value, Msg> {
        let (item, source, t) = self.selection().ok_or_else(|| tmsg!("select a source first"))?;
        let native = self.studio.source_size(source).ok_or_else(|| tmsg!("the source has not shown a picture yet"))?;
        let canvas = self.studio.canvas();
        // The command refuses values out of range; a fit or a nudge stops at the limits instead.
        let t = f(t, native, canvas).sanitized();
        let r = self.studio.run("item.transform", &json!({"item": item, "transform": t})).map_err(|e| e.to_string())?;
        self.studio.seal_history();
        Ok(r.0)
    }

    fn ui_command(&mut self, ctx: &egui::Context, cmd: &CommandRef) -> Result<Value, Msg> {
        match cmd.id.as_str() {
            "file.show_recordings" => {
                let folder = studiocraft_studio::recording_folder(&self.studio.profile().output.folder);
                studiocraft_platform::shell::open_folder(&folder).map_err(|e| e.to_string())?;
            }
            "file.show_settings_folder" => {
                let dir = self.store.dir().ok_or_else(|| tmsg!("settings are kept in memory only (--no-prefs)"))?.to_path_buf();
                studiocraft_platform::shell::open_folder(&dir).map_err(|e| e.to_string())?;
            }
            "view.stats" => return Ok(self.toggle_pref(|p| &mut p.stats)),
            "view.scopes" => {
                let r = self.toggle_pref(|p| &mut p.scopes);
                self.sync_scopes();
                return Ok(r);
            }
            "scopes.kind" => {
                let name = Self::param_str(cmd, "kind")?;
                let kind = craft_scopes::ScopeKind::ALL.into_iter().find(|k| k.name() == name).ok_or_else(|| {
                    let known: Vec<&str> = craft_scopes::ScopeKind::ALL.iter().map(|k| k.name()).collect();
                    tmsg!("`scopes.kind`: unknown scope `{name}` (one of {scopes})", name = name, scopes = known.join(", "))
                })?;
                if scopes::kind_of(&self.store.prefs().scope_kind) != kind {
                    self.store.update(|p| p.scope_kind = kind.name().to_string());
                }
                return Ok(json!({"kind": kind.name()}));
            }
            "view.status_bar" => return Ok(self.toggle_pref(|p| &mut p.status_bar)),
            "view.lock_preview" => return Ok(self.toggle_pref(|p| &mut p.lock_preview)),
            "view.fullscreen" => {
                self.fullscreen = !self.fullscreen;
                ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
                return Ok(json!({"on": self.fullscreen}));
            }
            "dock.scenes" => return Ok(self.toggle_pref(|p| &mut p.docks.scenes)),
            "dock.sources" => return Ok(self.toggle_pref(|p| &mut p.docks.sources)),
            "dock.mixer" => return Ok(self.toggle_pref(|p| &mut p.docks.mixer)),
            "dock.app_audio" => return Ok(self.toggle_pref(|p| &mut p.docks.app_audio)),
            "dock.transitions" => return Ok(self.toggle_pref(|p| &mut p.docks.transitions)),
            "dock.controls" => return Ok(self.toggle_pref(|p| &mut p.docks.controls)),
            "dock.move" => {
                let i = Self::param_dock(cmd)?;
                let left = match Self::param_str(cmd, "to")? {
                    "left" => true,
                    "right" => false,
                    to => return Err(tmsg!("`dock.move`: `to` is `left` or `right`, not `{to}`", to = to)),
                };
                let mut docks = self.store.prefs().docks;
                let moved = docks.shift(i, left);
                if moved {
                    self.store.update(|p| p.docks = docks);
                }
                let order: Vec<&str> = docks.order().iter().filter_map(|i| control::DOCK_NAMES.get(usize::from(*i)).copied()).collect();
                return Ok(json!({"moved": moved, "order": order}));
            }
            "dock.float" => {
                let i = Self::param_dock(cmd)?;
                let floating =
                    cmd.params.get("floating").and_then(Value::as_bool).ok_or_else(|| tmsg!("`dock.float` needs `floating`: true or false"))?;
                let changed = self.store.prefs().docks.is_floating(i) != floating;
                if changed {
                    self.store.update(|p| {
                        if let Some(f) = p.docks.floating.get_mut(usize::from(i)) {
                            *f = floating;
                        }
                    });
                }
                return Ok(json!({"floating": floating, "changed": changed}));
            }
            "dock.reset" => self.set_docks(store::Docks::default()),
            "dock.layout_save" => {
                let Some(name) = cmd.params.get("name") else {
                    self.ask_name(Named::SaveDockLayout, "");
                    return Ok(json!({"asking": true}));
                };
                let name = name.as_str().ok_or_else(|| tmsg!("`dock.layout_save`: `name` is a string"))?;
                let replaced = self.save_dock_layout(name)?;
                return Ok(json!({"saved": name, "replaced": replaced}));
            }
            "dock.layout_apply" => {
                let name = Self::param_str(cmd, "name")?;
                let layout =
                    self.store.prefs().dock_layout(name).cloned().ok_or_else(|| tmsg!("there is no dock layout named `{name}`", name = name))?;
                self.set_docks(layout.docks);
                return Ok(json!({"applied": layout.name}));
            }
            "dock.layout_remove" => {
                let name = Self::param_str(cmd, "name")?.to_string();
                let mut removed = false;
                self.store.update(|p| removed = p.remove_dock_layout(&name));
                if !removed {
                    return Err(tmsg!("there is no dock layout named `{name}`", name = name));
                }
                return Ok(json!({"removed": name}));
            }
            "profile.new" => self.ask_name(Named::NewProfile, ""),
            "profile.duplicate" => {
                let n = format!("{} copy", self.studio.profile().name);
                self.ask_name(Named::DuplicateProfile, &n);
            }
            "profile.rename" => {
                let n = self.studio.profile().name.clone();
                self.ask_name(Named::RenameProfile, &n);
            }
            "profile.remove" => self.remove_profile()?,
            "profile.switch" => {
                let name = Self::param_str(cmd, "name")?.to_string();
                self.switch_profile(&name)?;
            }
            "collection.new" => self.ask_name(Named::NewCollection, ""),
            "collection.duplicate" => {
                let n = format!("{} copy", self.studio.collection().name);
                self.ask_name(Named::DuplicateCollection, &n);
            }
            "collection.rename" => {
                let n = self.studio.collection().name.clone();
                self.ask_name(Named::RenameCollection, &n);
            }
            "collection.remove" => self.remove_collection()?,
            "collection.switch" => {
                let name = Self::param_str(cmd, "name")?.to_string();
                self.switch_collection(&name)?;
            }
            "collection.import" => match Self::param_path(cmd)? {
                Some(path) => return self.import_collection(&path),
                None => self.ask_collection_path(cmd, craft_dialogs::FileDialog::open(), Then::ImportCollection)?,
            },
            "collection.export" => match Self::param_path(cmd)? {
                Some(path) => return self.export_collection(&path),
                None => {
                    let name = format!("{}.json", self.studio.collection().name);
                    self.ask_collection_path(cmd, craft_dialogs::FileDialog::save().file_name(name), Then::ExportCollection)?;
                }
            },
            "video.displays" => {
                let list: Vec<Value> = studiocraft_platform::display::displays()
                    .into_iter()
                    .map(|d| json!({"id": d.id, "name": d.name, "width": d.width, "height": d.height, "primary": d.primary}))
                    .collect();
                let zoom = ctx.zoom_factor();
                let window = ctx.input(|i| window_display(i.viewport(), zoom)).map(|(w, h)| json!({"width": w, "height": h}));
                return Ok(json!({"displays": list, "window": window}));
            }
            "video.canvas_from_display" => {
                let zoom = ctx.zoom_factor();
                let size = match cmd.params.get("display") {
                    None => ctx
                        .input(|i| window_display(i.viewport(), zoom))
                        .ok_or_else(|| tmsg!("`video.canvas_from_display`: the size of the display the window is on is not known; pass `display`"))?,
                    Some(Value::String(id)) => {
                        let list = studiocraft_platform::display::displays();
                        let d = find_display(&list, id)?;
                        (d.width, d.height)
                    }
                    Some(_) => return Err(tmsg!("`video.canvas_from_display`: `display` is a display id from `video.displays`")),
                };
                return self.set_canvas(canvas_for(size)?);
            }
            "tools.encoders" => {
                // Without the ffmpeg program only the built-in encoders are available, and the
                // window says why.
                let configured = self.studio.profile().output.ffmpeg_path.trim().to_string();
                let (offered, missing) = match studiocraft_platform::ffmpeg::locate(&configured) {
                    Ok(found) => (studiocraft_platform::ffmpeg::encoders(&found), None),
                    Err(_) if !configured.is_empty() => (Vec::new(), Some(tmsg!("ffmpeg at {path} was not found", path = configured))),
                    Err(e) => (Vec::new(), Some(Msg::plain(e.to_string()))),
                };
                self.encoders_ffmpeg = missing;
                // Recordings run the built-in encoders.
                let env = studiocraft_plugin::EncoderEnv { builtin: true, ..studiocraft_plugin::EncoderEnv::ffmpeg(&offered) };
                let list = self
                    .studio
                    .registry()
                    .encoders()
                    .iter()
                    .map(|e| {
                        let codec = e.ffmpeg().map_or(e.codec().id(), |f| f.ffmpeg_codec());
                        // Only a built-in encoder, or one this ffmpeg is known to run, is listed as
                        // available.
                        let ok = e.availability(&env) == studiocraft_plugin::Availability::Ready;
                        (e.name().to_string(), codec.to_string(), ok)
                    })
                    .collect();
                self.encoders = Some(list);
            }
            "selection.properties" => {
                let (_, source, _) = self.selection().ok_or_else(|| tmsg!("select a source first"))?;
                self.open_properties(source);
            }
            "selection.filters" => {
                let (_, source, _) = self.selection().ok_or_else(|| tmsg!("select a source first"))?;
                self.open_filters(source);
            }
            "selection.remove" => {
                let (item, _, _) = self.selection().ok_or_else(|| tmsg!("select a source first"))?;
                self.studio.run("item.remove", &json!({"item": item})).map_err(|e| e.to_string())?;
                self.selected = None;
            }
            "selection.fit" => return self.selection_transform(|t, n, c| Transform { crop: t.crop, ..Transform::fit(t.crop.apply(n.0, n.1), c) }),
            "selection.stretch" => {
                return self.selection_transform(|t, n, c| {
                    Transform { position: Vec2::ZERO, rotation: 0.0, ..t }.with_size(n, Vec2::new(c.0 as f32, c.1 as f32))
                });
            }
            "selection.center" => {
                return self.selection_transform(|t, n, c| {
                    let s = t.size(n);
                    Transform { position: Vec2::new((c.0 as f32 - s.x) * 0.5, (c.1 as f32 - s.y) * 0.5), ..t }
                });
            }
            "selection.reset" => return self.selection_transform(|_, _, _| Transform::default()),
            "selection.rotate_cw" => return self.selection_transform(|t, _, _| Transform { rotation: t.rotation + 90.0, ..t }.sanitized()),
            "selection.rotate_ccw" => return self.selection_transform(|t, _, _| Transform { rotation: t.rotation + 270.0, ..t }.sanitized()),
            "app.about" => self.about = true,
            "app.check_updates" => return updates_ui::check(self, ctx),
            "app.licenses" => {
                if self.licenses.is_none() {
                    self.licenses = Some(credits::License::default());
                }
            }
            id @ ("help.discord" | "help.website" | "help.github" | "help.issues") => {
                let url = commands::community_link(id).ok_or_else(|| tmsg!("there is no link for `{command}`", command = id))?;
                studiocraft_platform::shell::open_url(url).map_err(|e| e.to_string())?;
                return Ok(json!({"url": url}));
            }
            other => return Err(tmsg!("unknown command `{command}`", command = other)),
        }
        Ok(Value::Null)
    }

    /// Put every dock setting to `docks`. Floating dock windows open again at the places and sizes
    /// `docks` holds.
    fn set_docks(&mut self, docks: store::Docks) {
        self.store.update(|p| p.docks = docks);
        self.dock_window_at = [None; store::DOCK_COUNT];
        self.dock_window_touched = [false; store::DOCK_COUNT];
        self.dock_window_drawn = [None; store::DOCK_COUNT];
        self.dock_window_gen = self.dock_window_gen.wrapping_add(1);
    }

    /// Save the current docks as the layout `name`; whether a layout with that name was replaced.
    fn save_dock_layout(&mut self, name: &str) -> Result<bool, Msg> {
        let mut result = Ok(false);
        self.store.update(|p| result = p.save_dock_layout(name));
        result
    }

    fn ask_name(&mut self, what: Named, text: &str) {
        self.prompt = Some(Prompt { what, text: text.to_string(), error: None });
    }

    fn named(&mut self, what: Named, name: &str) -> Result<(), Msg> {
        check_name(name)?;
        let taken = |list: &[String]| list.iter().any(|n| n.eq_ignore_ascii_case(name));
        match what {
            Named::NewProfile | Named::DuplicateProfile | Named::RenameProfile => {
                if taken(&self.profiles) && !(what == Named::RenameProfile && name.eq_ignore_ascii_case(&self.studio.profile().name)) {
                    return Err(tmsg!("there is already a profile named `{name}`", name = name));
                }
                let old = self.studio.profile().name.clone();
                let mut p = if what == Named::NewProfile { Profile::default() } else { self.studio.profile().clone() };
                p.name = name.to_string();
                let previous = self.studio.profile().clone();
                self.store.save_profile(&previous).map_err(|e| tmsg!("The profile could not be saved: {error}", error = e))?;
                // The new profile is applied first, so no file is written for one that cannot be.
                self.studio.set_profile(p.clone()).map_err(|e| e.to_string())?;
                // On a disk that ignores case, `Show` and `show` are one file: a rename that
                // changes only the case renames that file, and the save then writes over it.
                let case_only = what == Named::RenameProfile && old != name && old.eq_ignore_ascii_case(name);
                let renamed = if case_only { self.store.rename_profile(&old, name) } else { Ok(()) };
                if let Err(e) = renamed.and_then(|()| self.store.save_profile(&p)) {
                    if case_only {
                        let _ = self.store.rename_profile(name, &old);
                    }
                    let _ = self.studio.set_profile(previous);
                    return Err(e.into());
                }
                if what == Named::RenameProfile && old != name && !case_only {
                    self.store.remove_profile(&old)?;
                }
                self.store.update(|pr| pr.profile = name.to_string());
            }
            Named::NewCollection | Named::DuplicateCollection | Named::RenameCollection => {
                if taken(&self.collections) && !(what == Named::RenameCollection && name.eq_ignore_ascii_case(&self.studio.collection().name)) {
                    return Err(tmsg!("there is already a scene collection named `{name}`", name = name));
                }
                let old = self.studio.collection().name.clone();
                let mut c = if what == Named::NewCollection { default_collection(name) } else { self.studio.collection().clone() };
                c.name = name.to_string();
                self.save_before_leaving()?;
                let case_only = what == Named::RenameCollection && old != name && old.eq_ignore_ascii_case(name);
                if case_only {
                    self.store.rename_collection(&old, name)?;
                }
                if let Err(e) = self.store.save_collection(&c) {
                    if case_only {
                        let _ = self.store.rename_collection(name, &old);
                    }
                    return Err(e.into());
                }
                self.load_collection(c);
                if what == Named::RenameCollection && old != name && !case_only {
                    self.store.remove_collection(&old)?;
                }
            }
            Named::SaveDockLayout => {
                self.save_dock_layout(name)?;
            }
        }
        self.refresh_lists();
        Ok(())
    }

    fn load_collection(&mut self, c: SceneCollection) {
        let name = c.name.clone();
        self.studio.load_collection(c);
        self.studio.mark_saved();
        self.collection_on_disk = true;
        self.selected = None;
        self.properties.clear();
        self.filters.clear();
        self.renaming = None;
        self.store.update(|p| p.collection = name);
    }

    fn switch_collection(&mut self, name: &str) -> Result<(), Msg> {
        if name == self.studio.collection().name {
            return Ok(());
        }
        let c = self.store.load_collection(name)?;
        self.save_before_leaving()?;
        self.load_collection(c);
        self.refresh_lists();
        Ok(())
    }

    fn switch_profile(&mut self, name: &str) -> Result<(), Msg> {
        if name == self.studio.profile().name {
            return Ok(());
        }
        let p = self.store.load_profile(name)?;
        // A profile whose settings cannot be saved stays open, so they are not lost.
        self.store.save_profile(self.studio.profile()).map_err(|e| tmsg!("The profile could not be saved: {error}", error = e))?;
        self.studio.set_profile(p).map_err(|e| e.to_string())?;
        self.store.update(|pr| pr.profile = name.to_string());
        self.settings = None;
        Ok(())
    }

    fn remove_collection(&mut self) -> Result<(), Msg> {
        let current = self.studio.collection().name.clone();
        let next = self.collections.iter().find(|n| **n != current).cloned().ok_or_else(|| tmsg!("the last scene collection can't be removed"))?;
        self.switch_collection(&next)?;
        self.store.remove_collection(&current)?;
        self.refresh_lists();
        Ok(())
    }

    fn remove_profile(&mut self) -> Result<(), Msg> {
        let current = self.studio.profile().name.clone();
        let next = self.profiles.iter().find(|n| **n != current).cloned().ok_or_else(|| tmsg!("the last profile can't be removed"))?;
        self.switch_profile(&next)?;
        self.store.remove_profile(&current)?;
        self.refresh_lists();
        Ok(())
    }

    fn open_properties(&mut self, source: SourceId) {
        if self.properties.iter().any(|p| p.source == source) {
            return;
        }
        if let Some(p) = Properties::open(&self.studio, source) {
            self.properties.push(p);
        }
    }

    fn open_filters(&mut self, source: SourceId) {
        if !self.filters.iter().any(|f| f.source == source) {
            self.filters.push(Filters::new(source));
        }
    }

    fn act(&mut self, ctx: &egui::Context, act: Act) {
        match act {
            Act::Run(cmd) => {
                let _ = self.perform(ctx, &cmd);
            }
            Act::Select(id) => self.selected = id,
            Act::Properties(s) => self.open_properties(s),
            Act::Filters(s) => self.open_filters(s),
            Act::AddSource { kind } => {
                let name = self.studio.registry().source(&kind).map(|p| p.name().to_string()).unwrap_or_else(|_| kind.clone());
                let name = self.studio.collection().unique_source_name(&name);
                if let Ok(r) = self.perform(ctx, &CommandRef::with_params("source.add", json!({"kind": kind, "name": name})))
                    && let Some(source) = r.get("source").and_then(Value::as_u64)
                {
                    self.open_properties(SourceId(source));
                }
            }
            Act::Seal => self.studio.seal_history(),
        }
    }

    fn browse(&mut self, b: Browse, then: Then) {
        let mut d = match b.mode {
            studiocraft_model::PathMode::OpenFile => craft_dialogs::FileDialog::open(),
            studiocraft_model::PathMode::SaveFile => craft_dialogs::FileDialog::save(),
            studiocraft_model::PathMode::Folder => craft_dialogs::FileDialog::folder(),
        };
        if !b.extensions.is_empty() {
            let ext: Vec<&str> = b.extensions.iter().map(String::as_str).collect();
            d = d.filter(b.filter_name, &ext);
        }
        if let Some(dialogs) = &mut self.dialogs {
            dialogs.ask(d, then);
        }
    }

    fn dialog_answers(&mut self, ctx: &egui::Context) {
        let Some(dialogs) = &mut self.dialogs else { return };
        for (then, answer) in dialogs.take_answers() {
            let path = match answer {
                craft_dialogs::Answer::Paths(p) => p.into_iter().next(),
                craft_dialogs::Answer::Unavailable(e) => {
                    let msg = tmsg!("No file dialog could be shown: {error}", error = e);
                    self.notices.push_back((Note { msg, error: true }, Instant::now()));
                    None
                }
                _ => None,
            };
            let Some(path) = path else { continue };
            match then {
                Then::SourcePath { source, key } => {
                    if let Some(p) = self.properties.iter_mut().find(|p| p.source == source) {
                        let params = p.set(&key, &path.to_string_lossy());
                        let _ = self.perform(ctx, &CommandRef::with_params("source.settings", params));
                        self.studio.seal_history();
                    }
                }
                Then::Settings(pick) => {
                    if let Some(w) = &mut self.settings {
                        w.picked(pick, &path);
                    }
                }
                Then::ImportCollection => {
                    let _ = self.perform(ctx, &CommandRef::with_params("collection.import", json!({"path": path.to_string_lossy()})));
                }
                Then::ExportCollection => {
                    let _ = self.perform(ctx, &CommandRef::with_params("collection.export", json!({"path": path.to_string_lossy()})));
                }
            }
        }
    }

    /// Housekeeping before drawing: studio ticks, notices, autosave, shortcuts.
    fn before(&mut self, ctx: &egui::Context) {
        if self.fonts_for.is_some_and(|l| l != i18n::lang()) {
            self.install_fonts(ctx);
        }
        let dir = i18n::direction();
        if craft_ui::direction::HasDirection::direction(ctx) != dir {
            craft_ui::direction::apply(ctx, dir);
        }
        // eframe calls `logic` before each drawn frame, and also while the window is hidden.
        if !std::mem::take(&mut self.ticked_in_logic) {
            self.tick_studio(ctx);
        }
        let now = Instant::now();
        let revision = self.studio.engine().revision();
        match self.changed_at {
            Some((r, _)) if r == revision => {}
            _ => self.changed_at = Some((revision, now)),
        }
        if self.studio.engine().is_dirty()
            && let Some((_, at)) = self.changed_at
            && now.duration_since(at) >= AUTOSAVE
        {
            self.save_collection();
        }
        self.store.tick();
        let at_start = self.store.prefs().check_updates;
        if let Some(e) = updates_ui::tick(&mut self.updates, ctx, at_start) {
            self.notify(tmsg!("Could not check for updates: {error}", error = e), true);
        }
        if !ctx.egui_is_using_pointer() && !ctx.egui_wants_keyboard_input() && !self.editor.dragging() {
            self.studio.seal_history();
        }
        if self.selection().is_none() {
            self.selected = None;
        }
        self.notices.retain(|(n, at)| {
            n.error && now.duration_since(*at) < Duration::from_secs(NOTICE_S * 3) || now.duration_since(*at) < Duration::from_secs(NOTICE_S)
        });
        let modal = self.prompt.is_some() || self.confirm_exit || self.settings.is_some();
        if !modal {
            let typing = ctx.egui_wants_keyboard_input();
            let active = Active { panel: None, contexts: &[], typing };
            let keymap = &self.keymap;
            let dispatcher = &mut self.dispatcher;
            let hits = ctx.input_mut(|i| dispatcher.dispatch(i, keymap, &active));
            for h in hits {
                let _ = self.perform(ctx, &h.command);
            }
        }
    }

    /// The top row: the menu titles and the output bar, and the window buttons when the app draws
    /// the window frame.
    fn menu(&mut self, ui: &mut Ui, acts: &mut Vec<Act>) {
        let laid = self.laid_menu();
        let gate =
            Gate { modal: self.prompt.is_some() || self.confirm_exit, recording_shortcut: false, text_editing: ui.ctx().egui_wants_keyboard_input() };
        let style = theme::bar_style(ui.style(), &theme::sizes(ui.ctx()));
        let clicked = match self.title_bar.clone() {
            Some(layout) => title_bar::strip(ui, &layout, |ui, room| self.top_row(ui, room, &laid, gate, style, acts)),
            None => {
                let room = ui.available_width();
                self.top_row(ui, room, &laid, gate, style, acts)
            }
        };
        let ctx = ui.ctx().clone();
        for c in clicked {
            let _ = self.perform(&ctx, &c);
        }
    }

    fn edit_items(&self) -> Vec<EditItem> {
        let c = self.studio.collection();
        c.scene(self.studio.engine().edit_scene())
            .map(|s| {
                s.items
                    .iter()
                    .map(|it| EditItem {
                        id: it.id,
                        transform: it.transform,
                        native: self.studio.source_size(it.source),
                        locked: it.locked,
                        visible: it.visible,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The editable view in `area`: the program, or the preview in studio mode.
    fn edit_view(&mut self, ui: &mut Ui, area: egui::Rect, acts: &mut Vec<Act>) {
        let canvas = self.studio.canvas();
        let view = View::fit(area, canvas);
        self.last_view = Some((view.rect, view.scale));
        let views = self.studio.views();
        let studio_mode = self.studio.engine().studio_mode();
        let tex = views.as_ref().and_then(|v| if studio_mode { self.textures.preview(v) } else { self.textures.program(v) });
        preview::paint_canvas(ui, view, tex);
        if views.is_none() {
            ui.painter().text(
                view.rect.center(),
                egui::Align2::CENTER_CENTER,
                tl!("No graphics device: the preview is off"),
                egui::FontId::proportional(16.0),
                Color32::GRAY,
            );
        }
        let items = self.edit_items();
        let locked = self.store.prefs().lock_preview;
        let (ev, resp) = self.editor.show(ui, view, canvas, &items, self.selected, locked);
        if let Some(sel) = ev.select {
            acts.push(Act::Select(sel));
        }
        if let Some((item, t)) = ev.transform {
            acts.push(Act::run("item.transform", json!({"item": item, "transform": t.sanitized()})));
        }
        if ev.released {
            acts.push(Act::Seal);
        }
        let selected = ev.select.unwrap_or(self.selected);
        let sel = selected.and_then(|id| self.studio.collection().scene(self.studio.engine().edit_scene())?.item(id)).map(|it| (it.id, it.source));
        let name = sel.and_then(|(_, s)| self.studio.collection().source(s)).map(|s| s.name.clone()).unwrap_or_default();
        let renaming = &mut self.renaming;
        let studio = &self.studio;
        resp.context_menu(|ui| {
            if let Some((item, source)) = sel {
                docks::item_menu(ui, item, source, &name, renaming, acts);
                ui.separator();
            }
            ui.menu_button(tl!("Add"), |ui| {
                for p in studio.registry().sources() {
                    if ui.button(tl!(p.name())).clicked() {
                        acts.push(Act::AddSource { kind: p.id().to_string() });
                        ui.close();
                    }
                }
            });
            if ui.checkbox(&mut locked.clone(), tid!("view.lock_preview", "Lock Preview")).clicked() {
                acts.push(Act::run("view.lock_preview", json!({})));
                ui.close();
            }
        });
    }

    fn program_view(&mut self, ui: &mut Ui, area: egui::Rect) {
        let view = View::fit(area, self.studio.canvas());
        let tex = self.studio.views().and_then(|v| self.textures.program(&v));
        preview::paint_canvas(ui, view, tex);
        if let Some(p) = self.studio.transition_progress() {
            let bar = egui::Rect::from_min_size(view.rect.left_bottom() - egui::vec2(0.0, 3.0), egui::vec2(view.rect.width() * p, 3.0));
            ui.painter().rect_filled(bar, 0.0, ui.visuals().selection.bg_fill);
        }
    }

    fn central(&mut self, ui: &mut Ui, acts: &mut Vec<Act>) {
        if let (Some(rs), Some(v)) = (&self.render, self.studio.views()) {
            self.textures.sync(rs, &v);
        }
        let area = ui.available_rect_before_wrap();
        if !self.studio.engine().studio_mode() {
            self.edit_view(ui, area.shrink(4.0), acts);
            return;
        }
        let mid = 110.0;
        let half = (area.width() - mid) / 2.0;
        let label_h = 22.0;
        let left = egui::Rect::from_min_size(area.min, egui::vec2(half, area.height()));
        let right = egui::Rect::from_min_size(area.min + egui::vec2(half + mid, 0.0), egui::vec2(half, area.height()));
        let middle = egui::Rect::from_min_size(area.min + egui::vec2(half, 0.0), egui::vec2(mid, area.height()));
        let edit_name = self.studio.collection().scene(self.studio.engine().edit_scene()).map(|s| s.name.clone()).unwrap_or_default();
        let program_name = self.studio.collection().scene(self.studio.collection().program_scene).map(|s| s.name.clone()).unwrap_or_default();
        let title = |ui: &Ui, r: egui::Rect, text: String| {
            ui.painter().text(
                r.center_top() + egui::vec2(0.0, 4.0),
                egui::Align2::CENTER_TOP,
                text,
                egui::FontId::proportional(14.0),
                ui.visuals().strong_text_color(),
            );
        };
        title(ui, left, craft_i18n::fmt(tl!("Preview: {name}"), &[("name", &edit_name)]));
        title(ui, right, craft_i18n::fmt(tl!("Program: {name}"), &[("name", &program_name)]));
        let body = |r: egui::Rect| egui::Rect::from_min_max(r.min + egui::vec2(4.0, label_h), r.max - egui::vec2(4.0, 4.0));
        self.edit_view(ui, body(left), acts);
        self.program_view(ui, body(right));
        let mut col =
            ui.new_child(egui::UiBuilder::new().max_rect(middle.shrink2(egui::vec2(6.0, 0.0))).layout(egui::Layout::top_down(egui::Align::Center)));
        col.add_space(middle.height() * 0.35);
        let w = col.available_width();
        if col
            .add(egui::Button::new(RichText::new(tl!("Transition")).strong()).min_size(egui::vec2(w, 30.0)))
            .on_hover_text(tl!("Send the preview to the program"))
            .clicked()
        {
            acts.push(Act::run("studio.transition", json!({})));
        }
        col.add_space(6.0);
        col.horizontal(|ui| {
            let half = (w - ui.spacing().item_spacing.x) / 2.0;
            if ui.add(egui::Button::new(tl!("Cut")).min_size(egui::vec2(half, 24.0))).on_hover_text(tl!("Switch at once")).clicked() {
                acts.push(Act::run("studio.transition", json!({"kind": "cut"})));
            }
            if ui.add(egui::Button::new(tl!("Fade")).min_size(egui::vec2(half, 24.0))).on_hover_text(tl!("Fade over 300 ms")).clicked() {
                acts.push(Act::run("studio.transition", json!({"kind": "fade", "ms": 300})));
            }
        });
    }

    fn docks(&mut self, ui: &mut Ui, acts: &mut Vec<Act>) {
        let d = self.store.prefs().docks;
        // The transition and output controls are in the output bar; Docks shows or hides them there.
        let shown = d.shown();
        // The header actions where the last frame drew them, for the shields over them below.
        let shields: Vec<egui::Rect> = self.dock_rects.iter().flat_map(|(_, _, drawn)| drawn.actions.iter().map(|(_, r)| *r)).collect();
        self.dock_rects.clear();
        self.dock_strip = None;
        let now = Instant::now();
        let dt = self.last_frame.map_or(0.0, |t| now.duration_since(t).as_secs_f32());
        self.last_frame = Some(now);
        let ctx = ui.ctx().clone();
        self.dock_windows(&ctx, acts, dt);
        if shown.is_empty() {
            return;
        }
        let palette = theme::palette(&ctx);
        let window = ctx.viewport_rect().height();
        let height = strip::height(window, d.height);
        let handle_style = ResizeHandleStyle { hovered: palette.line_strong, dragged: palette.accent, focused: palette.focus };
        let mut st = docks::DockState {
            selected: self.selected,
            renaming: &mut self.renaming,
            meters: &mut self.meters,
            lanes: &mut self.lanes,
            dt,
            place: Some((0, shown.len())),
        };
        let studio = &self.studio;
        let rects = &mut self.dock_rects;
        let saved = d.widths();
        // Widths after a divider moved this frame, by dock index.
        let mut new_widths: Option<[f32; store::DOCK_COUNT]> = None;
        let most = strip::max_height(window);
        let panel = egui::Panel::bottom("docks").exact_size(height).show_separator_line(false).frame(egui::Frame::NONE.fill(palette.surface)).show(
            ui,
            |ui| {
                let (area, _) = ui.allocate_exact_size(ui.available_rect_before_wrap().size(), egui::Sense::hover());
                // The strip's top edge: a 1 point line inside the strip, which is also its resize
                // handle. The handle comes before the docks, so a header action under its hit band
                // takes a press there. Each shield sits between the handle and one action and takes
                // a drag that starts on the action, so the edge resizes only outside the actions.
                // The edge is painted after the strip, over the headers.
                let edge = egui::Rect::from_min_size(area.left_top(), egui::vec2(area.width(), 1.0));
                let band = edge.expand2(egui::vec2(0.0, EDGE_HIT_MARGIN));
                let mut edge_ui = ui.new_child(egui::UiBuilder::new().id_salt("dock-strip-edge").max_rect(band));
                edge_ui.set_clip_rect(band);
                let unpainted = ResizeHandleStyle { hovered: Color32::TRANSPARENT, dragged: Color32::TRANSPARENT, focused: Color32::TRANSPARENT };
                let edge_r =
                    ResizeHandle::new(egui::Id::unique("dock-strip-edge"), SplitAxis::Vertical, edge, tl!("Resize the dock strip"), unpainted)
                        .hit_margin(EDGE_HIT_MARGIN)
                        .value(height, strip::MIN_HEIGHT.min(height)..=most.max(height))
                        .reversed(true)
                        .show(&mut edge_ui);
                for (k, r) in shields.iter().enumerate() {
                    ui.interact(*r, egui::Id::unique("dock-action-shield").with(k), egui::Sense::drag());
                }
                let widths = strip::widths(area.width(), &shown, saved);
                let mut x = area.left();
                let mut lines = Vec::with_capacity(shown.len());
                for (k, (i, w)) in shown.iter().zip(&widths).enumerate() {
                    let rect = egui::Rect::from_min_size(egui::pos2(x, area.top()), egui::vec2(*w, area.height()));
                    let mut col =
                        ui.new_child(egui::UiBuilder::new().id_salt(("dock", *i)).max_rect(rect).layout(egui::Layout::top_down(egui::Align::Min)));
                    col.set_clip_rect(rect.intersect(ui.clip_rect()));
                    st.place = Some((k, shown.len()));
                    let drawn = match i {
                        0 => docks::scenes(&mut col, studio, &mut st, acts),
                        1 => docks::sources(&mut col, studio, &mut st, acts),
                        3 => docks::app_audio(&mut col, studio, &mut st, acts),
                        _ => docks::mixer(&mut col, studio, &mut st, acts),
                    };
                    rects.push((usize::from(*i), rect, drawn));
                    x += w;
                    if k + 1 < shown.len() {
                        lines.push(egui::Rect::from_min_size(egui::pos2(x, area.top()), egui::vec2(strip::DIVIDER, area.height())));
                        x += strip::DIVIDER;
                    }
                }
                // The dividers come after the docks so that their 8 point hit bands take the pointer
                // over the docks' edges.
                for (k, line) in lines.iter().enumerate() {
                    ui.painter().rect_filled(*line, 0.0, palette.line);
                    let most = (widths[k] + widths[k + 1] - strip::MIN_WIDTH).max(strip::MIN_WIDTH);
                    let r = ResizeHandle::new(
                        egui::Id::unique("dock-divider").with(shown[k]),
                        SplitAxis::Horizontal,
                        *line,
                        tl!("Resize the docks"),
                        handle_style,
                    )
                    .hit_margin(3.5)
                    .value(widths[k], strip::MIN_WIDTH.min(widths[k])..=most.max(widths[k]))
                    .show(ui);
                    if r.delta != 0.0 {
                        let moved = strip::drag(&widths, k, r.delta);
                        new_widths = Some(strip::save(saved, &shown, &moved, k));
                    }
                }
                edge_r
            },
        );
        let rect = panel.response.rect;
        self.dock_strip = Some(rect);
        let r = panel.inner;
        let edge = egui::Rect::from_min_size(rect.left_top(), egui::vec2(rect.width(), 1.0));
        let fill = if r.response.dragged() {
            handle_style.dragged
        } else if r.response.has_focus() {
            handle_style.focused
        } else if r.response.hovered() {
            handle_style.hovered
        } else {
            palette.line
        };
        ui.painter().rect_filled(edge, 0.0, fill);
        let new_height = (r.delta != 0.0).then(|| strip::height(window, height - r.delta));
        if new_widths.is_some() || new_height.is_some_and(|h| h != height) {
            self.store.update(|p| {
                if let Some([a, b, c, e]) = new_widths {
                    p.docks.scenes_width = a;
                    p.docks.sources_width = b;
                    p.docks.mixer_width = c;
                    p.docks.app_audio_width = e;
                }
                if let Some(h) = new_height {
                    p.docks.height = h;
                }
            });
        }
    }

    /// The docks shown in windows of their own. A window is dragged by its dock's title, keeps
    /// inside the main window, and opens where it was last moved to at the size it was last given.
    /// The place and size are saved once the pointer is released.
    fn dock_windows(&mut self, ctx: &egui::Context, acts: &mut Vec<Act>, dt: f32) {
        self.floating_rects.clear();
        let d = self.store.prefs().docks;
        let mut st = docks::DockState {
            selected: self.selected,
            renaming: &mut self.renaming,
            meters: &mut self.meters,
            lanes: &mut self.lanes,
            dt,
            place: None,
        };
        let studio = &self.studio;
        let pointer_down = ctx.input(|i| i.pointer.any_down());
        let interacting = pointer_down || ctx.input(|i| i.pointer.any_released());
        let mut changed: Vec<(usize, [f32; 2], [f32; 2])> = Vec::new();
        let grab = {
            let s = ctx.global_style();
            s.interaction.resize_grab_radius_side.max(s.interaction.resize_grab_radius_corner)
        };
        let viewport = ctx.viewport_rect().size();
        let same_viewport = self.dock_viewport.replace(viewport) == Some(viewport);
        for i in d.floating_shown() {
            let k = usize::from(i).min(store::DOCK_COUNT - 1);
            // Window sizes are outer sizes, with the frame's margins and stroke in them.
            let size = d.window_size(i).map_or(egui::vec2(strip::default_width(k), strip::DEFAULT_HEIGHT), |[w, h]| egui::vec2(w, h));
            let mut window = egui::Window::new(tl!(control::DOCK_TITLES[k]))
                .id(egui::Id::unique("dock-window").with((k, self.dock_window_gen)))
                .title_bar(false)
                .movable(false)
                .resizable(true)
                .default_size(size)
                .min_size(egui::vec2(strip::MIN_WIDTH, strip::MIN_HEIGHT));
            let kept = self.dock_window_at[k].or_else(|| d.window_pos(i).map(|[x, y]| egui::pos2(x, y)));
            if let Some(at) = kept {
                window = window.current_pos(at);
            }
            let shown = window.show(ctx, |ui| {
                // The dock lays out its body in the room left under its header, so the window keeps
                // the size it was given instead of shrinking to the header.
                ui.set_min_size(ui.available_size());
                // The title moves the window (`Drawn::grab`); a drag anywhere else in the dock
                // does not.
                let drawn = match i {
                    0 => docks::scenes(ui, studio, &mut st, acts),
                    1 => docks::sources(ui, studio, &mut st, acts),
                    3 => docks::app_audio(ui, studio, &mut st, acts),
                    _ => docks::mixer(ui, studio, &mut st, acts),
                };
                let grab = drawn.grab;
                (drawn, grab)
            });
            let Some(shown) = shown else { continue };
            let rect = shown.response.rect;
            let Some((drawn, delta)) = shown.inner else { continue };
            // The user resized the window when its size changed during a press that started on it
            // or its resize edge, or on the frame that press ended, while the main window kept its
            // size.
            // The press is tested against the window as it was before the resize.
            let resized = self.dock_window_drawn[k].is_some_and(|was| {
                interacting
                    && same_viewport
                    && was.size() != rect.size()
                    && ctx.input(|i| i.pointer.press_origin()).is_some_and(|p| was.union(rect).expand(grab).contains(p))
            });
            self.dock_window_drawn[k] = Some(rect);
            // `dock_window_at` is where the user put the window. egui may draw it further in when
            // the main window is smaller; that does not change the place that is kept. A window that
            // cannot be moved resizes only from its right and bottom edges, so a resize keeps it.
            let at = if delta != egui::Vec2::ZERO { rect.min + delta } else { kept.unwrap_or(rect.min) };
            self.dock_window_at[k] = Some(at);
            if delta != egui::Vec2::ZERO || resized {
                self.dock_window_touched[k] = true;
                ctx.request_repaint();
            }
            let place = [at.x, at.y];
            let outer = [rect.width(), rect.height()];
            // The place and size are saved when the user moved or resized the window, and the first
            // time the window is shown.
            let save = self.dock_window_touched[k] || d.window_pos(i).is_none();
            if !pointer_down && save {
                self.dock_window_touched[k] = false;
                if place.iter().chain(&outer).all(|v| v.is_finite()) && { d }.place_window(k, place, outer) {
                    changed.push((k, place, outer));
                }
            }
            self.floating_rects.push((k, rect, drawn));
        }
        if !changed.is_empty() {
            self.store.update(|p| {
                for (k, place, outer) in &changed {
                    p.docks.place_window(*k, *place, *outer);
                }
            });
        }
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        if !self.store.prefs().status_bar {
            return;
        }
        let stats = self.studio.stats();
        let fps = self.studio.video_settings().fps();
        // Notices are kept in English and translated here, when shown.
        let notice = self.notices.back().map(|(n, _)| (n.msg.shown(), n.error));
        let rec = self.studio.output(OutputKind::Recording);
        let live = self.studio.output(OutputKind::Streaming);
        let replay = self.studio.output(OutputKind::Replay);
        let outputs_run = [&rec, &live].into_iter().flatten().any(|s| s.active);
        let source_limit = self.studio.audio_tracks_source_limit();
        let tracks = self.studio.audio_tracks_status().and_then(|s| tracks_item(s, outputs_run, source_limit));
        let height = theme::sizes(ui.ctx()).bar_status;
        let line = theme::palette(ui.ctx()).line;
        let frame = egui::Frame::side_top_panel(ui.style()).inner_margin(egui::Margin::symmetric(8, 0));
        let panel = egui::Panel::bottom("status").exact_size(height).show_separator_line(false).frame(frame).show(ui, |ui| {
            ui.with_layout(craft_ui::direction::row(ui), |ui| {
                if let Some((text, error)) = notice {
                    let color = if error { ui.visuals().warn_fg_color } else { ui.visuals().text_color() };
                    // A message longer than the bar is cut off, so the whole text is on hover.
                    ui.colored_label(color, text.clone()).on_hover_text(text);
                }
                ui.with_layout(craft_ui::direction::trailing(ui), |ui| {
                    let fps = format!("{fps:.2}");
                    let ms = format!("{:.1}", stats.render_avg_ms);
                    ui.monospace(craft_i18n::fmt(tl!("{fps} fps  {ms} ms"), &[("fps", &fps), ("ms", &ms)]));
                    ui.separator();
                    let (dropped, lagged) = (stats.encoder_dropped.to_string(), stats.lagged.to_string());
                    ui.monospace(craft_i18n::fmt(tl!("dropped {dropped}  lagged {lagged}"), &[("dropped", &dropped), ("lagged", &lagged)]));
                    for (s, tag) in [(live, tl!("LIVE")), (rec, tl!("REC")), (replay, tl!("REPLAY"))] {
                        if let Some(s) = s.filter(|s| s.active) {
                            ui.separator();
                            let t = s.elapsed.as_secs();
                            ui.colored_label(Color32::from_rgb(230, 70, 60), format!("{tag} {:02}:{:02}:{:02}", t / 3600, (t / 60) % 60, t % 60));
                        }
                    }
                    if let Some((text, warn, hover)) = tracks {
                        ui.separator();
                        let color = if warn { ui.visuals().warn_fg_color } else { ui.visuals().text_color() };
                        ui.colored_label(color, text).on_hover_text(hover);
                    }
                });
            });
        });
        // The divider is drawn inside the bar's first point, so the bar keeps its height.
        let row = panel.response.rect;
        ui.painter().hline(row.x_range(), row.top() + 0.5, egui::Stroke::new(1.0, line));
    }

    fn windows(&mut self, ctx: &egui::Context, acts: &mut Vec<Act>) {
        let mut browse = Vec::new();
        let mut i = 0;
        while i < self.properties.len() {
            let mut open = true;
            let (cmd, b, audio) = self.properties[i].show(ctx, &self.studio, &mut open);
            let source = self.properties[i].source;
            if let Some(params) = cmd {
                acts.push(Act::Run(CommandRef::with_params("source.settings", params)));
            }
            for (id, params) in audio {
                acts.push(Act::run(id, params));
            }
            if let Some(b) = b {
                let key = b.key.clone();
                browse.push((b, Then::SourcePath { source, key }));
            }
            if open {
                i += 1;
            } else {
                self.properties.remove(i);
            }
        }
        let mut i = 0;
        while i < self.filters.len() {
            let mut open = true;
            for (id, params) in self.filters[i].show(ctx, &self.studio, &mut open) {
                acts.push(Act::run(id, params));
            }
            if open {
                i += 1;
            } else {
                self.filters.remove(i);
            }
        }
        for (b, then) in browse {
            self.browse(b, then);
        }

        if let Some(w) = &mut self.settings {
            let out = w.show(ctx, self.studio.registry(), &self.keymap);
            if let Some(pick) = out.pick {
                let b = match pick {
                    Pick::RecordingFolder => {
                        Browse { key: String::new(), mode: studiocraft_model::PathMode::Folder, filter_name: String::new(), extensions: vec![] }
                    }
                    Pick::Ffmpeg => Browse {
                        key: String::new(),
                        mode: studiocraft_model::PathMode::OpenFile,
                        filter_name: "ffmpeg".into(),
                        extensions: if cfg!(windows) { vec!["exe".into()] } else { vec![] },
                    },
                };
                self.browse(b, Then::Settings(pick));
            }
            // Settings the studio refuses (video changes while an output runs) apply nothing, and
            // the window stays open with the edits.
            let refused = out.apply.is_some_and(|a| !self.apply_settings(a));
            if out.close && !refused {
                self.settings = None;
            }
        }

        if self.store.prefs().stats {
            let s = self.studio.stats();
            let mut open = true;
            egui::Window::new(tid!("view.stats", "Stats")).id(egui::Id::unique("stats")).open(&mut open).default_width(300.0).show(ctx, |ui| {
                egui::Grid::new("stats-grid").num_columns(2).striped(true).show(ui, |ui| {
                    let ms = |v: f64| craft_i18n::fmt(tl!("{ms} ms"), &[("ms", &format!("{v:.2}"))]);
                    let rows = [
                        ("Frames drawn", s.frames.to_string()),
                        ("Frames drawn late", s.lagged.to_string()),
                        ("Frame time (last)", ms(s.render_ms)),
                        ("Frame time (average)", ms(s.render_avg_ms)),
                        ("Frames encoded", s.encoder_frames.to_string()),
                        ("Frames skipped by encoders", s.encoder_dropped.to_string()),
                        ("Encoder queue", s.encoder_queued.to_string()),
                    ];
                    for (k, v) in rows {
                        ui.label(tl!(k));
                        ui.monospace(v);
                        ui.end_row();
                    }
                    for (kind, label) in
                        [(OutputKind::Recording, "Recording"), (OutputKind::Streaming, "Stream"), (OutputKind::Replay, "Replay buffer")]
                    {
                        if let Some(o) = self.studio.output(kind) {
                            ui.label(tl!(label));
                            let mb = format!("{:.1}", o.bytes as f64 / 1e6);
                            ui.monospace(craft_i18n::fmt(tl!("{status} · {mb} MB"), &[("status", &output_status(&o)), ("mb", &mb)]));
                            ui.end_row();
                        }
                    }
                });
            });
            if !open {
                self.store.update(|p| p.stats = false);
            }
        }

        self.scopes_window(ctx, acts);

        if let Some(list) = &self.encoders {
            let mut open = true;
            let missing = self.encoders_ffmpeg.as_ref().map(Msg::shown);
            egui::Window::new(tid!("tools.encoders", "Available Encoders")).id(egui::Id::unique("encoders")).open(&mut open).show(ctx, |ui| {
                if let Some(missing) = missing {
                    ui.colored_label(ui.visuals().warn_fg_color, missing);
                }
                egui::Grid::new("enc-grid").num_columns(3).striped(true).show(ui, |ui| {
                    for (name, codec, ok) in list {
                        ui.label(tl!(name));
                        ui.monospace(codec);
                        if *ok {
                            ui.colored_label(Color32::from_rgb(80, 190, 90), tl!("available"));
                        } else {
                            ui.weak(tl!("not in this ffmpeg"));
                        }
                        ui.end_row();
                    }
                });
            });
            if !open {
                self.encoders = None;
            }
        }

        if self.about {
            let mut open = true;
            let mode = &mut self.credit_names;
            let about = egui::Window::new(tid!("app.about", "About StudioCraft")).id(egui::Id::unique("about"));
            about.open(&mut open).collapsible(false).resizable(false).default_width(360.0).show(ctx, |ui| {
                ui.heading(APP_NAME);
                let (version, os, arch) = (env!("CARGO_PKG_VERSION"), std::env::consts::OS, std::env::consts::ARCH);
                ui.label(craft_i18n::fmt(tl!("Version {version} ({os} {arch})"), &[("version", version), ("os", os), ("arch", arch)]));
                ui.add_space(6.0);
                ui.label(tl!("Record and stream scenes built from your screen, windows, cameras, pictures, text and sound."));
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label(tl!("Free software under the MIT or Apache 2.0 license."));
                    if ui.button(tid!("app.licenses", "Licenses")).clicked() {
                        acts.push(Act::run("app.licenses", json!({})));
                    }
                });
                ui.horizontal(|ui| {
                    for (id, _, label) in commands::COMMUNITY {
                        if ui.link(tid!(id, label)).clicked() {
                            acts.push(Act::run(id, json!({})));
                        }
                    }
                });
                ui.separator();
                craft_ui::direction::horizontal(ui, |ui| {
                    ui.strong(tl!("Credits"));
                    ui.with_layout(craft_ui::direction::trailing(ui), |ui| {
                        craft_ui::ComboBox::from_id_salt("credit-names").selected_text(tl!(mode.label())).show_ui(ui, |ui| {
                            for m in credits::NameMode::ALL {
                                ui.selectable_value(mode, m, tl!(m.label()));
                            }
                        });
                        ui.label(tl!("Show"));
                    });
                });
                let people = credits::sorted(studiocraft_engine::credits::CONTRIBUTORS, *mode);
                if people.is_empty() {
                    ui.weak(tl!("No contributors listed yet"));
                } else {
                    egui::ScrollArea::vertical().max_height(160.0).auto_shrink([false, true]).show(ui, |ui| {
                        for (shown, login) in people {
                            ui.label(shown).on_hover_text(format!("@{login}"));
                        }
                    });
                }
            });
            self.about &= open;
        }

        if let Some(shown) = &mut self.licenses {
            let mut open = true;
            let window = egui::Window::new(tid!("app.licenses", "Licenses")).id(egui::Id::unique("licenses"));
            window.open(&mut open).default_size([520.0, 420.0]).show(ctx, |ui| {
                ui.label(tl!("StudioCraft is available under either license, at your option."));
                ui.label(tl!("The interface fonts Inter and JetBrains Mono are under the SIL Open Font License 1.1."));
                ui.horizontal_wrapped(|ui| {
                    for l in credits::License::ALL {
                        ui.selectable_value(shown, l, tl!(l.label()));
                    }
                });
                ui.weak(tl!("The licenses of the third-party crates StudioCraft uses are checked by `cargo xtask licenses`."));
                ui.separator();
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    ui.monospace(shown.text());
                });
            });
            if !open {
                self.licenses = None;
            }
        }

        // An update never starts while a recording, stream or replay buffer runs.
        let idle = !self.outputs_running();
        match updates_ui::show(&mut self.updates, ctx, idle) {
            updates_ui::Asked::Nothing => {}
            updates_ui::Asked::OpenUrl(url) => {
                if let Err(e) = studiocraft_platform::shell::open_url(&url) {
                    self.notify(e.to_string(), true);
                }
            }
            updates_ui::Asked::Close => self.request_exit(ctx),
        }

        if let Some(p) = &mut self.prompt {
            let title = tl!(p.what.title());
            let mut done: Option<bool> = None;
            egui::Modal::new(egui::Id::unique("name-prompt")).show(ctx, |ui| {
                ui.heading(title);
                ui.label(tl!("Name"));
                let r = ui.add(egui::TextEdit::singleline(&mut p.text).desired_width(280.0));
                r.request_focus();
                if let Some(e) = &p.error {
                    ui.colored_label(ui.visuals().error_fg_color, e.shown());
                }
                ui.horizontal(|ui| {
                    if ui.button(tl!("OK")).clicked() || ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        done = Some(true);
                    }
                    if ui.button(tl!("Cancel")).clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                        done = Some(false);
                    }
                });
            });
            match done {
                Some(true) => {
                    let (what, text) = (p.what, p.text.trim().to_string());
                    match self.named(what, &text) {
                        Ok(()) => self.prompt = None,
                        Err(e) => {
                            if let Some(p) = &mut self.prompt {
                                p.error = Some(e);
                            }
                        }
                    }
                }
                Some(false) => self.prompt = None,
                None => {}
            }
        }

        if self.confirm_exit {
            let mut choice = None;
            egui::Modal::new(egui::Id::unique("confirm-exit")).show(ctx, |ui| {
                ui.heading(tl!("Exit StudioCraft?"));
                ui.label(tl!(
                    "A recording, stream or replay buffer is running. Exiting stops it; a recording is finished and saved first, and the replay buffer's unsaved video is lost."
                ));
                ui.horizontal(|ui| {
                    if ui.button(tl!("Stop and Exit")).clicked() {
                        choice = Some(true);
                    }
                    if ui.button(tl!("Cancel")).clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                        choice = Some(false);
                    }
                });
            });
            match choice {
                Some(true) => {
                    self.confirm_exit = false;
                    self.exit_confirmed = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                Some(false) => self.confirm_exit = false,
                None => {}
            }
        }
    }

    /// Draw one frame of the whole window.
    pub fn frame(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        self.frames += 1;
        self.before(&ctx);
        self.dialog_answers(&ctx);
        let mut acts = Vec::new();
        let bar = theme::sizes(&ctx).bar_menu;
        let fill = theme::palette(&ctx).raised;
        let (left, right) = if craft_ui::direction::is_rtl(&ctx) { (0, 4) } else { (4, 0) };
        let top = egui::Panel::top("menu")
            .exact_size(bar)
            .show_separator_line(false)
            .frame(egui::Frame::NONE.fill(fill).inner_margin(egui::Margin { left, right, top: 0, bottom: 0 }))
            .show(ui, |ui| self.menu(ui, &mut acts));
        // The divider is drawn inside the row's last point, so the content area keeps the full height.
        let row = top.response.rect;
        ui.painter().hline(row.x_range(), row.bottom() - 0.5, egui::Stroke::new(1.0, theme::palette(&ctx).line));
        self.output_bar.rect = Some(row);
        self.status_bar(ui);
        self.docks(ui, &mut acts);
        let background = theme::palette(&ctx).background;
        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(background)).show(ui, |ui| self.central(ui, &mut acts));
        self.windows(&ctx, &mut acts);
        theme::paint_focus_ring(&ctx);
        if self.title_bar.is_some() {
            title_bar::resize_zones(ui);
        }
        for a in acts {
            self.act(&ctx, a);
        }
        // Live video: draw continuously, also while the window is in the background.
        ctx.request_repaint();
    }

    /// Tick the studio, show its notices and check the window's device.
    fn tick_studio(&mut self, ctx: &egui::Context) {
        self.studio.tick();
        for n in self.studio.take_notices() {
            self.notify(Msg::new(n.template, n.args), n.error);
        }
        self.watch_window_gpu(ctx);
    }

    /// Before the frame: the studio ticks, and a close request waits for confirmation while an
    /// output runs.
    pub fn logic(&mut self, ctx: &egui::Context) {
        self.tick_studio(ctx);
        self.ticked_in_logic = true;
        if ctx.input(|i| i.viewport().close_requested()) {
            let busy = self.outputs_running();
            if busy && !self.exit_confirmed {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.confirm_exit = true;
            } else if !self.save_all() && !self.close_despite_failed_save {
                // The window stays open with the save error shown; closing again closes it.
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.close_despite_failed_save = true;
            }
        }
        craft_quit::on_close_request(ctx);
    }

    /// Install the interface fonts for the current interface language on `ctx`. After the first
    /// call, the window installs them again on the first frame after the language changes.
    pub fn install_fonts(&mut self, ctx: &egui::Context) {
        let lang = i18n::lang();
        fonts::install(ctx, i18n::I18N.code(lang));
        self.fonts_for = Some(lang);
    }

    /// Give the app the native file dialogs (a window exists).
    pub fn attach_dialogs(&mut self, ctx: &egui::Context) {
        let c = ctx.clone();
        self.dialogs = Some(craft_dialogs::Dialogs::new(Arc::new(move || c.request_repaint())));
    }

    /// Show the queued file dialog, at the end of a frame.
    pub fn show_dialogs(&mut self, frame: &eframe::Frame) {
        if let Some(d) = &mut self.dialogs {
            d.show_pending(Some(frame));
        }
    }
}

impl eframe::App for StudioApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        StudioApp::logic(self, ctx);
    }

    fn ui(&mut self, ui: &mut Ui, frame: &mut eframe::Frame) {
        if self.dialogs.is_none() {
            self.attach_dialogs(ui.ctx());
        }
        self.frame(ui);
        self.show_dialogs(frame);
    }

    fn on_exit(&mut self) {
        self.save_all();
    }
}

/// The state of a recording or stream in the interface language.
fn output_status(o: &studiocraft_studio::OutputState) -> String {
    use studiocraft_plugin::OutputStatus;
    match (&o.status, o.stopping) {
        (_, true) => tl!("Stopping…").to_string(),
        (OutputStatus::Starting, _) => tl!("Starting…").to_string(),
        (OutputStatus::Running, _) => tl!("Running").to_string(),
        (OutputStatus::Stopped, _) => tl!("Stopped").to_string(),
        (OutputStatus::Failed(e), _) => craft_i18n::fmt(tl!("Failed: {error}"), &[("error", e)]),
    }
}

/// The audio tracks item of the status bar: its text, whether it shows as a warning, and its
/// hover text. It shows while a take runs or finishes, and after a take stopped early while the
/// recording or the stream that started it still runs.
/// `source_limit` says that sources set to record were left out by the limit on tracks at once.
fn tracks_item(s: &studiocraft_studio::AudioTracksStatus, outputs_run: bool, source_limit: bool) -> Option<(String, bool, String)> {
    use studiocraft_studio::AudioTracksState as S;
    let t = s.seconds() as u64;
    let clock = format!("{:02}:{:02}:{:02}", t / 3600, (t / 60) % 60, t % 60);
    let tracks = s.tracks().to_string();
    match s.state()? {
        S::Recording | S::Finishing => {
            let mut warnings = Vec::new();
            if s.low_space() {
                warnings.push(tl!("The disk is almost full; audio tracks stop before the recording or the stream runs out of space"));
            }
            if s.track_limit() {
                warnings.push(tl!("This take has made as many audio track files as it can; sources added since are not recorded to their own files"));
            }
            if source_limit {
                warnings.push(tl!("More sources are set to record their own track than can be recorded at once; the later ones are not recorded"));
            }
            let warn = !warnings.is_empty();
            let hover = if warnings.is_empty() {
                let lost = format!("{:.1}", s.dropped_seconds());
                craft_i18n::fmt(
                    tl!("Audio tracks: {tracks}; {lost} s of audio lost while the disk was slow"),
                    &[("tracks", &tracks), ("lost", &lost)],
                )
            } else {
                warnings.join("\n")
            };
            Some((format!("{} {clock}", tl!("TRACKS")), warn, hover))
        }
        S::Stopped if outputs_run => {
            Some((tl!("TRACKS stopped").to_string(), true, tl!("Audio tracks stopped early; the status message gives the reason").to_string()))
        }
        S::Stopped | S::Done => None,
    }
}

/// `base`, or the first of `base 2`, `base 3` and so on that `taken` does not hold. When those
/// names would be too long, the numbers follow the default name instead.
fn free_name(base: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(base) {
        return base.to_string();
    }
    let stem = if check_name(&format!("{base} 10000")).is_ok() { base } else { DEFAULT_NAME };
    (2..10_000).map(|n| format!("{stem} {n}")).find(|n| !taken(n)).unwrap_or_else(|| format!("{stem} 10000"))
}

/// The size in pixels of the display the window is on, when the windowing system reports it.
/// egui reports the size in points at `zoom` times the display's own pixels per point.
fn window_display(info: &egui::ViewportInfo, zoom: f32) -> Option<(u32, u32)> {
    let size = info.monitor_size?;
    let ppp = info.native_pixels_per_point? * zoom;
    let side = |v: f32| {
        let p = (v * ppp).round();
        (p.is_finite() && p >= 1.0 && p <= u32::MAX as f32).then_some(p as u32)
    };
    Some((side(size.x)?, side(size.y)?))
}

/// The listed display with `id`, or an error naming the listed ids.
fn find_display<'a>(list: &'a [DisplayInfo], id: &str) -> Result<&'a DisplayInfo, Msg> {
    list.iter().find(|d| d.id == id).ok_or_else(|| {
        let ids: Vec<&str> = list.iter().map(|d| d.id.as_str()).collect();
        if ids.is_empty() {
            tmsg!("there is no display `{id}`: the system lists no displays", id = id)
        } else {
            tmsg!("there is no display `{id}` (the displays are {ids})", id = id, ids = ids.join(", "))
        }
    })
}

/// A display size as a canvas size, or an error when a side is outside the canvas's range.
fn canvas_for((width, height): (u32, u32)) -> Result<(u32, u32), Msg> {
    use studiocraft_model::profile::{MAX_SIDE, MIN_SIDE};
    let ok = |v: u32| (MIN_SIDE..=MAX_SIDE).contains(&v);
    if ok(width) && ok(height) {
        Ok((width, height))
    } else {
        Err(tmsg!(
            "a {width} × {height} display can't be the canvas: each side must be from {min} to {max} pixels",
            width = width,
            height = height,
            min = MIN_SIDE,
            max = MAX_SIDE,
        ))
    }
}

#[cfg(test)]
mod tests;
