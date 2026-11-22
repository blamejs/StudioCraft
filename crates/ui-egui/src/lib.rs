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
mod follow;
pub mod fonts;
mod forms;
pub mod hardware;
mod icons;
pub mod kit;
mod logos;
mod output_bar;
mod pause_banner;
pub mod placement;
mod preview;
mod scopes;
mod settings;
pub mod store;
mod streaming;
pub mod strip;
pub mod theme;
mod updates_ui;

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use craft_behavior::{ColumnWidth, GesturePhase};
use craft_host::target::{Caps, DoubleClick, ShellInfo};
use craft_host::{AppInfo, Host};
use craft_i18n::{I18n, Words};
use craft_keymap::{CommandRef, Keymap, Overrides};
use craft_menu::{LanguageChoice, MenuBar};
use craft_theme::variant::Tone;
use craft_ui::bars::{Bar, BarKind};
use craft_ui::contract::overlay::PlacementKind;
use craft_ui::contract::{Bind, Entry, Props as _, Show as _, ShowContainer as _, SizeLimits, Sizing, TooltipSpec, Tracked};
use craft_ui::dialog::{Dialog, DialogId, DialogResult, PromptChoice, StandardPrompt};
use craft_ui::display::{Divider, Shade, Text};
use craft_ui::docking::{DockSizes, FloatingGroup};
use craft_ui::layout_components::Grid;
use craft_ui::notices::{Notice, NoticeCenter, PostedNotice};
use craft_ui::patterns::form_dialogs::{DialogFields, FieldValue, HostedParamDialog, Param, ParamDialog, ParamKind};
use craft_ui::pickers::Select;
use craft_ui::runtime::commands::{AppCatalog, CommandInput, CommandState, CommandStates};
use craft_ui::runtime::env::{Label, WindowKey};
use craft_ui::runtime::frame::{FrameInput, FrameOutput};
use craft_ui::runtime::toast::{self, ToastId};
use craft_ui::runtime::windows::ResizeEdge;
use craft_ui::shell::embed::{self, CloseDecision};
use craft_ui::shell::{AppShell, ShellDialogsCx as _, TitleBar, ViewportWindow, WindowDialogs};
use craft_ui::style::TypeRole;
use craft_ui::{InstallError, InstallSpec};
use craft_window::policy::Decision;
use craft_window::system::{NoWindows, SystemWindows};
use egui::{Color32, RichText, Ui};
use serde_json::{Value, json};
use studiocraft_media::mix::MeterBallistics;
use studiocraft_model::{FitMode, ItemId, Profile, SceneCollection, SettingsSchema, SourceId, Transform, Vec2};
use studiocraft_platform::display::DisplayInfo;
use studiocraft_render::Gpu;
use studiocraft_render::device::{Candidate, Preference};
use studiocraft_studio::files::read_collection_file;
use studiocraft_studio::{CaptionBoard, OutputKind, Studio, UiAction, builtin_registry, default_collection};

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

/// StudioCraft as the updater knows it: where its releases are listed, and the icon the update
/// prompt shows and the menu entry beside an AppImage uses. It sets no accent color, so the update
/// prompt takes the theme's accent.
pub const UPDATE_APP: craft_update::app::App = craft_update::app::App {
    icon_png: Some(app_icon::ICON_128_PNG),
    ..craft_update::app::App::new(APP_ID, APP_NAME, "Recording and live streaming", "blamejs/StudioCraft")
};

/// Something the docks, menus or preview asked for.
pub(crate) enum Act {
    Run(CommandRef),
    Select(Option<ItemId>),
    Properties(SourceId),
    Filters(SourceId),
    /// Ask for a new name for a source in the name prompt.
    Rename(SourceId),
    AddSource {
        kind: String,
    },
    /// End the current drag in the undo history.
    Seal,
    /// Hold `key` for this session under `name` for the stream destinations that use it; an
    /// empty key forgets it. The key is never saved, so it is not a command parameter.
    HoldStreamKey {
        name: String,
        key: String,
    },
    /// Open a public web page in the browser.
    OpenUrl(String),
}

impl Act {
    pub(crate) fn run(id: &str, params: Value) -> Act {
        Act::Run(CommandRef::with_params(id, params))
    }
}

/// What is being renamed in place: a source, whose name field the Sources dock opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Rename {
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
    /// A source's new name, asked for by Rename in an Audio Mixer channel's menu.
    RenameSource(SourceId),
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
            Named::RenameSource(_) => "Rename Source",
        }
    }
}

/// The key of the name prompt on the window's dialog host, and of its one field.
const NAME_DIALOG: &str = "name-prompt";
const NAME_FIELD: &str = "name";

/// A name being asked for: what it is for, the text the prompt opens with, the prompt's id once
/// the window's dialog host shows it, and the last name it refused, in English.
struct Naming {
    what: Named,
    text: String,
    dialog: Option<DialogId>,
    refused: Option<Msg>,
}

/// A notice: kept in English, translated when its card is posted.
struct Note {
    msg: Msg,
    error: bool,
}

/// How long a notice's card stays, in seconds.
const NOTICE_S: f32 = 8.0;
/// How long an error notice's card stays, in seconds.
const ERROR_NOTICE_S: f32 = 24.0;
/// Most notices kept at once, posted or waiting for the next frame; the oldest go first.
const NOTICES_KEPT: usize = 20;
/// How long after the last change the scene collection is saved.
const AUTOSAVE: Duration = Duration::from_millis(800);

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
    /// The command catalog the runtime fires shortcuts from, built once.
    command_catalog: Option<AppCatalog>,
    /// The shortcut overrides handed to the runtime; replaced only when the keymap's change.
    key_overrides: Arc<Overrides>,
    /// The laid-out menu bar the top row draws.
    menu_bar: Tracked<MenuBar>,
    dialogs: Option<craft_dialogs::Dialogs<Then>>,
    render: Option<egui_wgpu::RenderState>,
    textures: Textures,
    editor: Editor,
    selected: Option<ItemId>,
    renaming: Option<(Rename, String)>,
    /// The Scenes and Sources docks' open menus.
    lists: docks::DockLists,
    /// The enabled state and tooltip of each header button of the Scenes and Sources docks.
    header_states: Tracked<CommandStates>,
    meters: HashMap<SourceId, MeterBallistics>,
    /// The Application Audio dock's lanes and meters, apart from the Audio Mixer's meters so that
    /// a source shown in both docks is advanced once per frame in each.
    lanes: docks::LaneCache,
    last_frame: Option<Instant>,
    properties: Vec<Properties>,
    filters: Vec<Filters>,
    settings: Option<SettingsWindow>,
    /// The Streaming window while it is open.
    streaming: Option<streaming::StreamingPanel>,
    /// Whether the command being performed comes from the user's own action in the window (a
    /// button, a menu item or a field), not from the control channel.
    from_user: bool,
    transition_schema: Option<SettingsSchema>,
    /// The name prompt while it is open or waiting for the dialog host.
    naming: Option<Naming>,
    /// The window's dialog host, known from the first frame on: the name prompt, the exit
    /// question, the update prompt and the result of Help › Check for Updates show on it.
    window_dialogs: Option<WindowDialogs>,
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
    /// The exit question is asked: a close came while an output runs.
    confirm_exit: bool,
    /// The exit question's prompt on the window's dialog host, once it shows there.
    exit_prompt: Option<DialogId>,
    exit_confirmed: bool,
    /// A close was canceled because the scene collection could not be saved; the next one goes ahead.
    close_despite_failed_save: bool,
    /// False while the collection made at start has never been written: it is saved at exit
    /// even with no changes.
    collection_on_disk: bool,
    /// The notices on screen, oldest first, with their cards in the window's toast queue; a
    /// notice without a card is posted with the next frame.
    notices: VecDeque<(Note, Option<ToastId>)>,
    /// The interface language the notices' cards were posted in; `None` before the first post.
    notices_lang: Option<craft_i18n::Lang>,
    collections: Vec<String>,
    profiles: Vec<String>,
    changed_at: Option<(u64, Instant)>,
    fullscreen: bool,
    frames: u64,
    /// Where the editable view was drawn last: its rectangle and screen points per canvas pixel.
    last_view: Option<(egui::Rect, f32)>,
    /// Where each docked dock was drawn last: its index in `control::DOCK_NAMES`, its rectangle,
    /// what its header drew and the region it was drawn in.
    dock_rects: Vec<(usize, egui::Rect, docks::Drawn, store::Region)>,
    /// The same for each dock in a window of its own, with the window's rectangle and the
    /// rectangle of the title bar that moves it.
    floating_rects: Vec<(usize, egui::Rect, docks::Drawn, egui::Rect)>,
    /// Where each floating dock's window is while the user moves or resizes it, by dock index. The
    /// place and size are saved when the move or resize ends.
    dock_window_rect: [Option<egui::Rect>; store::DOCK_COUNT],
    /// Counts the dock windows' resets this session. It is part of their ids, so a window opened
    /// after a reset keeps nothing from the old one.
    dock_window_gen: u32,
    /// Where the dock strip was drawn last.
    dock_strip: Option<egui::Rect>,
    /// Where the left and right columns were drawn last.
    dock_columns: [Option<egui::Rect>; 2],
    /// Whether the columns' docks were drawn in the strip last, because the window was too narrow
    /// for the columns.
    docks_collapsed: bool,
    /// Where the top row and the parts of the output bar were drawn last.
    output_bar: output_bar::Drawn,
    /// The paused banner above the program, and the resume announcement after it.
    pause_banner: pause_banner::State,
    /// Help › Check for Updates, the check at start and the update prompt.
    updates: updates_ui::Runtime,
    /// The interface language [`Self::install_fonts`] last built the fonts for; `None` until it
    /// runs. The fonts are built again when the language changes.
    fonts_for: Option<craft_i18n::Lang>,
    /// The UI runtime's host: the catalogs and the interface language that the framework's own
    /// words and the layout direction follow. A new one is handed in when the language changes.
    host: Arc<Host>,
    /// The framework's words in the host's language, for the Language menu the window lays out.
    words: Arc<Words>,
    window_gpu: WindowGpu,
    /// Whether [`Self::logic`] ticked the studio since the last frame was drawn.
    ticked_in_logic: bool,
    /// Lists the displays, and looks at them while the canvas follows one.
    displays: follow::Watch,
    /// The profile whose canvas could not take a followed display's size because the profile
    /// could not be saved, with that size and the store's count of profile saves at the time.
    followed_unsaved: Option<(String, (u32, u32), u64)>,
}

impl StudioApp {
    /// Open the collection and profile used last (or new ones), and start the studio on the
    /// window's graphics device.
    pub fn boot(mut store: Store, render: Option<egui_wgpu::RenderState>, problems: Vec<String>) -> StudioApp {
        studiocraft_platform::display::prefetch();
        let ffmpeg = Arc::new(RwLock::new(String::new()));
        let captions = Arc::new(CaptionBoard::new());
        let mut notices: Vec<Msg> = Vec::new();
        let registry = match builtin_registry(&ffmpeg, &captions) {
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
        let mut studio = Studio::new(gpu, collection, profile, Arc::new(registry), ffmpeg, captions);
        studio.keep_hardware_tests_in(store.dir().map(|d| d.join(studiocraft_studio::HARDWARE_TESTS_FILE)));
        store.update(|p| {
            p.profile = studio.profile().name.clone();
            p.collection = studio.collection().name.clone();
        });
        let keymap = commands::keymap(store.prefs().hotkeys.clone());
        let host = Arc::new(app_host(i18n::catalogs()));
        let mut app = StudioApp {
            studio,
            store,
            keymap,
            command_catalog: None,
            key_overrides: Arc::new(Overrides::new()),
            menu_bar: Tracked::new(MenuBar::new(Vec::new())),
            dialogs: None,
            render,
            textures: Textures::default(),
            editor: Editor::default(),
            selected: None,
            renaming: None,
            lists: docks::DockLists::default(),
            header_states: Tracked::new(CommandStates::new()),
            meters: HashMap::new(),
            lanes: docks::LaneCache::default(),
            last_frame: None,
            properties: Vec::new(),
            filters: Vec::new(),
            settings: None,
            streaming: None,
            from_user: false,
            transition_schema: None,
            naming: None,
            window_dialogs: None,
            about: false,
            credit_names: credits::NameMode::default(),
            licenses: None,
            encoders: None,
            encoders_ffmpeg: None,
            scopes: None,
            confirm_exit: false,
            exit_prompt: None,
            exit_confirmed: false,
            close_despite_failed_save: false,
            collection_on_disk: true,
            notices: VecDeque::new(),
            notices_lang: None,
            collections: Vec::new(),
            profiles: Vec::new(),
            changed_at: None,
            fullscreen: false,
            frames: 0,
            last_view: None,
            dock_rects: Vec::with_capacity(control::DOCK_NAMES.len()),
            floating_rects: Vec::new(),
            dock_window_rect: [None; store::DOCK_COUNT],
            dock_window_gen: 0,
            dock_strip: None,
            dock_columns: [None; 2],
            docks_collapsed: false,
            output_bar: output_bar::Drawn::default(),
            pause_banner: pause_banner::State::default(),
            updates: updates_ui::Runtime::default(),
            fonts_for: None,
            host: Arc::clone(&host),
            words: host_words(&host),
            window_gpu: WindowGpu::default(),
            ticked_in_logic: false,
            displays: follow::Watch::new(Arc::new(studiocraft_platform::display::displays)),
            followed_unsaved: None,
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

    /// The window opened its graphics device for `opened_with` ([`hardware::startup`]), and the
    /// user is told `notices` about the choice. With `may_relaunch` a lost device closes the
    /// window so that the app starts again ([`StudioApp::relaunch_requested`]). It does not when
    /// the window drew on a software adapter, or opened for software and has now lost its device
    /// [`hardware::KEPT_LOSSES`] times in the recovery window (a machine with no software
    /// adapter), since starting again would end the same way; a notice then asks the user to restart.
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
        // The window closes without asking, so only once everything was saved; otherwise a
        // notice says what could not be saved and the window stays open.
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
        self.notices.push_back((Note { msg, error }, None));
        while self.notices.len() > NOTICES_KEPT {
            self.notices.pop_front();
        }
    }

    /// Post the notices that have no card yet to the window's toast queue, in the interface
    /// language, and forget the notices whose cards left it (closed, timed out or dropped for a
    /// newer one). An error's card stays [`ERROR_NOTICE_S`] seconds, any other [`NOTICE_S`].
    /// After the interface language changes, the cards on screen are posted again in the new
    /// language, each with its whole time.
    fn post_notices(&mut self, ctx: &egui::Context) {
        let window = WindowKey::main();
        self.forget_gone_notices(ctx, &window);
        let lang = i18n::lang();
        if self.notices_lang != Some(lang) {
            let center = NoticeCenter::new().window(window.clone());
            for posted in self.notices.iter_mut().filter_map(|(_, id)| id.take()) {
                center.remove(ctx, posted);
            }
            self.notices_lang = Some(lang);
        }
        for (note, id) in self.notices.iter_mut().filter(|(_, id)| id.is_none()) {
            let (tone, seconds) = if note.error { (Tone::Bad, ERROR_NOTICE_S) } else { (Tone::Neutral, NOTICE_S) };
            let center = NoticeCenter::new().window(window.clone()).timeout(seconds);
            *id = center.post(ctx, &PostedNotice::new(note.msg.shown()).tone(tone), &[]);
        }
        // A post past the window's card limit drops the oldest card.
        self.forget_gone_notices(ctx, &window);
    }

    /// Forget the notices whose cards are no longer in `window`'s toast queue.
    fn forget_gone_notices(&mut self, ctx: &egui::Context, window: &WindowKey) {
        let queued: Vec<ToastId> = toast::cards(ctx, window).into_iter().map(|card| card.id).collect();
        self.notices.retain(|(_, id)| id.is_none_or(|id| queued.contains(&id)));
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
    /// An output size that was a share of the canvas in [`studiocraft_model::OUTPUT_SCALES`]
    /// becomes the same share of the new canvas; any other output size stays. The studio lays out
    /// the scenes for the new size. An open Settings window shows the new sizes. A profile that
    /// can't be saved is an error, and the old sizes and layout come back, so the scene
    /// collection is not saved with a layout for a canvas the saved profile does not have.
    fn set_canvas(&mut self, size: (u32, u32)) -> Result<Value, Msg> {
        let previous = self.studio.profile().clone();
        let mut p = previous.clone();
        let v = p.video.with_canvas(size.0, size.1);
        let (width, height) = (v.base_width, v.base_height);
        let changed = self.studio.canvas() != (width, height);
        if changed {
            (p.video.base_width, p.video.base_height) = (width, height);
            (p.video.output_width, p.video.output_height) = (v.output_width, v.output_height);
            self.studio.set_profile(p).map_err(|e| e.to_string())?;
        }
        // Saved even when the size was already in use, so a save that failed before is retried.
        if let Err(e) = self.store.save_profile(self.studio.profile()) {
            if changed {
                let _ = self.studio.restore_profile(previous);
            }
            return Err(tmsg!("The profile could not be saved: {error}", error = e));
        }
        let out = self.studio.video_settings();
        if let Some(w) = &mut self.settings {
            // The draft's own output size follows its canvas by the same rule, so an unsaved
            // output size typed into the draft stays.
            let before = w.draft.video;
            (w.draft.video.base_width, w.draft.video.base_height) = (width, height);
            settings::follow_canvas(before, &mut w.draft.video);
        }
        Ok(json!({"width": width, "height": height, "output": {"width": out.output_width, "height": out.output_height}, "changed": changed}))
    }

    /// Make the canvas follow `display`, when the canvas has that display's size; with `None`, or
    /// another size, the canvas follows no display.
    fn follow_display(&mut self, display: Option<&DisplayInfo>) {
        let canvas = self.studio.canvas();
        let profile = self.studio.profile().name.clone();
        let link = display.filter(|d| canvas_size((d.width, d.height)) == canvas).map(|d| store::CanvasDisplay {
            profile: profile.clone(),
            id: d.id.clone(),
            name: d.name.clone(),
            width: canvas.0,
            height: canvas.1,
        });
        if self.store.prefs().canvas_display(&profile) != link.as_ref() {
            self.store.update(|p| match link {
                Some(l) => p.set_canvas_display(l),
                None => p.forget_canvas_display(&profile),
            });
        }
    }

    /// The display the canvas of the profile in use follows.
    fn followed_display(&self) -> Option<&store::CanvasDisplay> {
        self.store.prefs().canvas_display(&self.studio.profile().name)
    }

    /// While the canvas follows a display, give it that display's new size when the display's
    /// resolution or scaling changes. The displays are listed on another thread, and this takes
    /// the list when it has arrived, so a slow listing never holds up the window.
    fn follow_displays(&mut self, ctx: &egui::Context, now: Instant) {
        let Some(link) = self.followed_display() else { return };
        if self.studio.canvas() != (link.width, link.height) {
            // The canvas size was set another way, so it no longer comes from the display.
            let profile = link.profile.clone();
            self.store.update(|p| p.forget_canvas_display(&profile));
            return;
        }
        let zoom = ctx.zoom_factor();
        let hint = ctx.input(|i| window_display(i.viewport(), zoom));
        if let Some(list) = self.displays.poll(now, hint) {
            self.displays_listed(&list);
        }
    }

    /// The displays a look found while the canvas follows one of them.
    fn displays_listed(&mut self, list: &[DisplayInfo]) {
        let Some(link) = self.followed_display().cloned() else { return };
        if self.studio.canvas() != (link.width, link.height) {
            return;
        }
        // A display that is not connected is followed again once it is.
        let Some(d) = follow::find(list, &link) else { return };
        let (id, name) = (d.id.clone(), d.name.clone());
        let Ok(size) = canvas_for((d.width, d.height)) else { return };
        let target = canvas_size(size);
        let before = self.studio.canvas();
        // A canvas size typed in the open Settings window and not yet applied is kept, with the
        // draft's output size.
        let typed = self.settings.as_ref().map(|w| w.draft.video).filter(|v| (v.base_width, v.base_height) != before);
        // The video settings can't change while an output runs; a later look takes the size once
        // the outputs stop.
        let busy = target != before && self.outputs_running();
        let result = if target == before || busy { Ok(Value::Null) } else { self.set_canvas(size) };
        if let (Some(w), Some(v)) = (&mut self.settings, typed) {
            w.draft.video = v;
        }
        let canvas = self.studio.canvas();
        let now = store::CanvasDisplay { width: canvas.0, height: canvas.1, id, name, ..link.clone() };
        if canvas == target && now != link {
            self.store.update(|p| p.set_canvas_display(now));
        }
        // A size whose profile can't be saved is taken back, and each look tries it again. Its
        // error is shown once, and again after another size fails or a profile save succeeds.
        let failed = (self.studio.profile().name.clone(), target, self.store.profile_saves());
        match result {
            Ok(_) if !busy => self.followed_unsaved = None,
            Ok(_) => {}
            Err(e) => {
                if self.followed_unsaved.as_ref() != Some(&failed) {
                    self.notify(e, true);
                }
                self.followed_unsaved = Some(failed);
            }
        }
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

    /// The place of the scene being edited among the scenes, and the number of scenes.
    fn edit_scene_place(&self) -> Option<(usize, usize)> {
        let c = self.studio.collection();
        Some((c.scene_index(self.studio.engine().edit_scene())?, c.scenes.len()))
    }

    /// The place of the selected item in the scene being edited, counted from the bottom item, and
    /// the number of items in that scene.
    fn selected_place(&self) -> Option<(usize, usize)> {
        let scene = self.studio.collection().scene(self.studio.engine().edit_scene())?;
        Some((scene.item_index(self.selected?)?, scene.items.len()))
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
                    "streaming_open": self.streaming.is_some(),
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

    /// Run a command by id: an engine command or one of the window's. An error is shown as a
    /// notice in the interface language and returned in English.
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
    /// changes while an output runs, and then nothing is applied). A new canvas size whose
    /// profile can't be saved is taken back with its layout, and nothing is applied either.
    fn apply_settings(&mut self, a: settings::Apply) -> bool {
        let previous = self.studio.profile().clone();
        match self.studio.set_profile(a.profile) {
            Ok(()) => {
                let canvas_moved = (previous.video.sanitized().base_width, previous.video.sanitized().base_height) != self.studio.canvas();
                let saved = self.save_profile();
                if !saved && canvas_moved {
                    let _ = self.studio.restore_profile(previous);
                    return false;
                }
                // Once the profile is saved, a canvas size picked from a display follows it, and a
                // pick that names no display ends the following. Any other new size follows no
                // display, which the next frame finds.
                if saved && let Some(pick) = &a.canvas_display {
                    self.follow_display(pick.as_ref());
                    // A later size typed in the open window is not taken as that pick.
                    if let Some(w) = &mut self.settings {
                        w.canvas_display = None;
                    }
                }
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
            self.open_waiting_dialogs(ctx);
        } else {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    /// Show the name prompt and the exit question that wait for the window's dialog host, once the
    /// host is known. The exit question's buttons stand in the platform's order; Escape cancels it.
    fn open_waiting_dialogs(&mut self, ctx: &egui::Context) {
        let Some(host) = self.window_dialogs.clone() else { return };
        if let Some(n) = self.naming.as_mut().filter(|n| n.dialog.is_none()) {
            let title = tl!(n.what.title()).to_string();
            let label = tl!("Name").to_string();
            let fields = DialogFields::from_pairs([(NAME_FIELD, FieldValue::Text(n.text.clone()))]);
            let dialog = HostedParamDialog::new(ctx, NAME_DIALOG, fields, move |fields| {
                ParamDialog::new(NAME_DIALOG, title.clone(), vec![Param::new(NAME_FIELD, label.clone(), ParamKind::text(""))])
                    .values(Bind::bound(fields))
            })
            .held_ok();
            let refused = n.refused.as_ref().map(|e| Label::from(e.shown()));
            n.dialog = Some(host.with(|h| {
                let id = h.open(dialog);
                // A name refused before the prompt could show: OK files it, and the refusal shows
                // under the field as it would have.
                if let Some(refused) = refused {
                    h.confirm_by_id(ctx, id);
                    if let Some(d) = h.get_mut_by_id::<HostedParamDialog>(id) {
                        let _ = d.take_ok();
                        d.answer_ok(Err(refused));
                    }
                }
                id
            }));
        }
        if self.confirm_exit && self.exit_prompt.is_none() {
            let prompt = StandardPrompt::custom(tl!(
                "A recording, stream or replay buffer is running. Exiting stops it; a recording is finished and saved first, and the replay buffer's unsaved video is lost."
            )
            .to_string())
            .title(tl!("Exit StudioCraft?").to_string())
            .button(PromptChoice::Proceed, tl!("Stop and Exit").to_string())
            .button(PromptChoice::Cancel, tl!("Cancel").to_string())
            .instance("exit");
            self.exit_prompt = Some(host.with(|h| h.open(prompt)));
        }
    }

    /// The answers of the name prompt and the exit question: a name OK filed is checked and taken
    /// or refused, and a closed prompt ends its question.
    fn prompt_answers(&mut self, ctx: &egui::Context) {
        let Some(host) = self.window_dialogs.clone() else { return };
        if let Some((what, id)) = self.naming.as_ref().and_then(|n| Some((n.what, n.dialog?))) {
            let filed = host.with(|h| h.get_mut_by_id::<HostedParamDialog>(id).and_then(HostedParamDialog::take_ok));
            if let Some(filed) = filed {
                let name = filed.fields.text(NAME_FIELD).unwrap_or_default().trim().to_string();
                let _ = self.answer_name(&host, what, id, &name);
            }
            if host.with(|h| h.take_by_id::<HostedParamDialog>(id)).is_some() {
                self.naming = None;
            }
        }
        if let Some(id) = self.exit_prompt {
            match host.with(|h| h.take_by_id::<StandardPrompt>(id)) {
                Some(DialogResult::Ok(answer)) => self.exit_answered(ctx, answer.choice == PromptChoice::Proceed),
                // Cancel, Escape or the close action.
                Some(_) => self.exit_answered(ctx, false),
                None => {}
            }
        }
    }

    /// Check the name the prompt `id` filed for `what` and answer the prompt: it closes with a name
    /// that was taken, and shows the refusal under its field otherwise. The refusal is returned.
    fn answer_name(&mut self, host: &WindowDialogs, what: Named, id: DialogId, name: &str) -> Result<(), Msg> {
        let result = self.named(what, name);
        let answer = result.as_ref().map(|_| ()).map_err(|e| Label::from(e.shown()));
        host.with(|h| {
            if let Some(d) = h.get_mut_by_id::<HostedParamDialog>(id) {
                d.answer_ok(answer);
            }
        });
        if let Some(n) = &mut self.naming {
            n.refused = result.as_ref().err().cloned();
        }
        result
    }

    /// The exit question was answered: Stop and Exit (`stop`) closes the window, which stops the
    /// outputs; Cancel keeps them running.
    fn exit_answered(&mut self, ctx: &egui::Context, stop: bool) {
        self.confirm_exit = false;
        self.exit_prompt = None;
        if stop {
            self.exit_confirmed = true;
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
        self.follow_language();
    }

    /// Give the host this thread's interface language when it differs.
    fn follow_language(&mut self) {
        let lang = i18n::lang();
        if self.host.lang() != lang {
            self.host = Arc::new(self.host.with_lang(lang));
            self.words = host_words(&self.host);
        }
    }

    /// Show the framework's words from `catalogs` instead of the embedded ones, as a test that
    /// keeps them apart from StudioCraft's own lookups does.
    #[cfg(test)]
    pub(crate) fn use_catalogs(&mut self, catalogs: Arc<I18n>) {
        self.host = Arc::new(app_host(catalogs));
        self.words = host_words(&self.host);
    }

    /// The `app.language` command: `lang` is `auto` or the code of a built-in language. Any other
    /// code, an empty one or a missing `lang` is an error that lists the codes.
    fn language_command(&mut self, cmd: &CommandRef) -> Result<Value, Msg> {
        let code = cmd.params.get("lang").and_then(Value::as_str).unwrap_or_default();
        let offered = i18n::languages();
        let setting = match LanguageChoice::from_command(cmd) {
            Some(LanguageChoice::Automatic) => i18n::AUTO,
            Some(LanguageChoice::Code(c)) => offered.iter().copied().find(|l| l.eq_ignore_ascii_case(&c)).unwrap_or_default(),
            _ => "",
        };
        if setting.is_empty() {
            let known: Vec<&str> = std::iter::once(i18n::AUTO).chain(offered.iter().copied()).collect();
            return Err(tmsg!(
                "`{command}`: unknown language `{code}` (one of {languages})",
                command = cmd.id,
                code = code,
                languages = known.join(", ")
            ));
        }
        if self.store.prefs().language != setting {
            self.set_language(setting);
        }
        Ok(json!({"language": setting}))
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
    /// It opens in its own OS window where the system has them, else over the main window.
    fn scopes_window(&mut self, ui: &mut Ui, acts: &mut Vec<Act>) {
        let ctx = ui.ctx().clone();
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
        let window = ViewportWindow::new(tid!("view.scopes", "Scopes")).overlay(|o| o.size(egui::vec2(520.0, 520.0)));
        let shown = window.show(ui, |ui, _| {
            let kinds: Vec<_> = scopes::MENU_ORDER.iter().map(|&k| Entry::new(k, tl!(k.label()))).collect();
            let picked = Select::new(tid!("view.scopes", "Scopes"), Bind::given(kind), &kinds).id_salt("scope-kind").show(ui);
            if let Some(&k) = picked.events.committed()
                && k != kind
            {
                acts.push(Act::run("scopes.kind", json!({"kind": k.name()})));
            }
            let latest = s.latest.as_ref().filter(|p| p.kind == kind);
            match latest {
                None if !has_gpu => {
                    let _ = Text::new(tl!("The scopes need a graphics device to read the program.")).shade(Shade::Muted).show(ui);
                }
                None => {
                    let _ = Text::new(tl!("Waiting for the program picture…")).shade(Shade::Muted).show(ui);
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
                let _ = Text::new(craft_i18n::fmt(tl!("The scope could not be computed: {error}"), &[("error", e)])).tone(Tone::Bad).show(ui);
            }
        });
        if shown.close_requested.is_some() {
            self.store.update(|p| p.scopes = false);
            self.sync_scopes();
        }
    }

    /// The Stats window, a tool window over the main window while View › Stats is on: the frame
    /// and encoder counts, and the state of each output. Closing it turns View › Stats off.
    fn stats_window(&mut self, ui: &mut Ui) {
        const COLUMNS: [ColumnWidth; 2] = [ColumnWidth::Content; 2];
        if !self.store.prefs().stats {
            return;
        }
        let s = self.studio.stats();
        let ms = |v: f64| craft_i18n::fmt(tl!("{ms} ms"), &[("ms", &format!("{v:.2}"))]);
        let mut rows: Vec<(String, String)> = [
            ("Frames drawn", s.frames.to_string()),
            ("Frames drawn late", s.lagged.to_string()),
            ("Frame time (last)", ms(s.render_ms)),
            ("Frame time (average)", ms(s.render_avg_ms)),
            ("Frames encoded", s.encoder_frames.to_string()),
            ("Frames skipped by encoders", s.encoder_dropped.to_string()),
            ("Encoder queue", s.encoder_queued.to_string()),
        ]
        .into_iter()
        .map(|(k, v)| (tl!(k).to_string(), v))
        .collect();
        for (kind, label) in [(OutputKind::Recording, "Recording"), (OutputKind::Streaming, "Stream"), (OutputKind::Replay, "Replay buffer")] {
            if let Some(o) = self.studio.output(kind) {
                let mb = format!("{:.1}", o.bytes as f64 / 1e6);
                rows.push((tl!(label).to_string(), craft_i18n::fmt(tl!("{status} · {mb} MB"), &[("status", &output_status(&o)), ("mb", &mb)])));
            }
        }
        let title = tid!("view.stats", "Stats");
        let mut open = true;
        let _ = Dialog::new(title).modeless().opened(Bind::bound(&mut open)).show(ui, |ui, _| {
            let _ = Grid::new(&COLUMNS).label(title).show(ui, |ui, _| {
                for (k, v) in &rows {
                    let _ = Text::new(k.clone()).show(ui);
                    let _ = Text::new(v.clone()).role(TypeRole::Mono).show(ui);
                }
            });
        });
        if !open {
            self.store.update(|p| p.stats = false);
        }
    }

    /// The Available Encoders window, a tool window over the main window after Tools › Available
    /// Encoders: each encoder with its codec and whether it can run, under a warning when the
    /// ffmpeg program was not found. Closing it drops the list.
    fn encoders_window(&mut self, ui: &mut Ui) {
        const COLUMNS: [ColumnWidth; 3] = [ColumnWidth::Content; 3];
        let Some(list) = &self.encoders else { return };
        let missing = self.encoders_ffmpeg.as_ref().map(Msg::shown);
        let title = tid!("tools.encoders", "Available Encoders");
        let mut open = true;
        let _ = Dialog::new(title).modeless().opened(Bind::bound(&mut open)).show(ui, |ui, _| {
            if let Some(missing) = &missing {
                let _ = Notice::new(Tone::Warn, missing.clone()).show(ui);
            }
            let _ = Grid::new(&COLUMNS).label(title).show(ui, |ui, _| {
                for (name, codec, ok) in list {
                    let _ = Text::new(tl!(name).to_string()).show(ui);
                    let _ = Text::new(codec.clone()).role(TypeRole::Mono).show(ui);
                    let _ = if *ok { Text::new(tl!("available")).tone(Tone::Good) } else { Text::new(tl!("not in this ffmpeg")).shade(Shade::Faint) }
                        .show(ui);
                }
            });
        });
        if !open {
            self.encoders = None;
        }
    }

    fn selection_transform(&mut self, f: impl FnOnce(Transform, (u32, u32), (u32, u32)) -> Transform) -> Result<Value, Msg> {
        self.selection_placed(None, f)
    }

    /// Fit, fill or stretch the selected item to the canvas, and keep the mode for a new canvas
    /// size.
    fn selection_fit(&mut self, mode: FitMode) -> Result<Value, Msg> {
        self.selection_placed(Some(mode), |t, n, c| mode.place(t, n, c))
    }

    fn selection_placed(&mut self, fit: Option<FitMode>, f: impl FnOnce(Transform, (u32, u32), (u32, u32)) -> Transform) -> Result<Value, Msg> {
        let (item, source, t) = self.selection().ok_or_else(|| tmsg!("select a source first"))?;
        let native = self.studio.source_size(source).ok_or_else(|| tmsg!("the source has not shown a picture yet"))?;
        let canvas = self.studio.canvas();
        // The command refuses values out of range; a fit or a nudge stops at the limits instead.
        let t = f(t, native, canvas).sanitized();
        let r = self.studio.run("item.transform", &json!({"item": item, "transform": t, "fit": fit})).map_err(|e| e.to_string())?;
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
            id if matches!(id, "view.streaming" | "stream.reconnect_set" | "stream.diagnose" | "stream.check_service_set")
                || id.starts_with("stream.destination") =>
            {
                return self.streaming_command(cmd);
            }
            id @ ("window.next_pane" | "window.previous_pane" | "window.focus_menu_bar") => {
                if !(cmd.params.is_null() || cmd.params.as_object().is_some_and(serde_json::Map::is_empty)) {
                    return Err(tmsg!("`{command}` takes no parameters", command = id));
                }
                // The runtime moves the focus in the next pass, as F6, Shift+F6 and F10 do.
                let fired = craft_ui::runtime::commands::fire(ctx, CommandRef::new(id), craft_ui::runtime::commands::FireSource::Automation);
                return Ok(json!({"fired": fired}));
            }
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
                    "left" | "up" => true,
                    "right" | "down" => false,
                    to => return Err(tmsg!("`dock.move`: `to` is `left`, `right`, `up` or `down`, not `{to}`", to = to)),
                };
                let mut docks = self.store.prefs().docks;
                let moved = docks.shift(i, left, self.docks_collapsed);
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
                let mut docks = self.store.prefs().docks;
                let mut changed = docks.floating.get_mut(usize::from(i)).is_some_and(|f| std::mem::replace(f, floating) != floating);
                // `false` puts the dock in the strip, from a window of its own or from a column.
                if !floating {
                    changed |= docks.set_region(i, store::Region::Bottom);
                }
                if changed {
                    self.store.update(|p| p.docks = docks);
                }
                return Ok(json!({"floating": floating, "changed": changed}));
            }
            "dock.region" => {
                let i = Self::param_dock(cmd)?;
                let name = Self::param_str(cmd, "region")?;
                let region = store::Region::parse(name)
                    .ok_or_else(|| tmsg!("`dock.region`: `region` is `left`, `right` or `bottom`, not `{region}`", region = name))?;
                let mut docks = self.store.prefs().docks;
                let moved = docks.set_region(i, region);
                // A dock in a window of its own is docked in the region.
                let docked = docks.floating.get_mut(usize::from(i)).is_some_and(|f| std::mem::replace(f, false));
                let changed = moved || docked;
                if changed {
                    self.store.update(|p| p.docks = docks);
                }
                return Ok(json!({"region": region.name(), "changed": changed}));
            }
            "dock.reset" => self.set_docks(store::Docks::default()),
            "dock.layout_save" => {
                let Some(name) = cmd.params.get("name") else {
                    self.ask_name(ctx, Named::SaveDockLayout, "");
                    return Ok(json!({"asking": true}));
                };
                let name = name.as_str().ok_or_else(|| tmsg!("`dock.layout_save`: `name` is a string"))?;
                let replaced = self.save_dock_layout(name)?;
                return Ok(json!({"saved": name, "replaced": replaced}));
            }
            "dock.layout_apply" => {
                let name = Self::param_str(cmd, "name")?;
                let layout = self.store.prefs().dock_layout(name).ok_or_else(|| tmsg!("there is no dock layout named `{name}`", name = name))?;
                self.set_docks(layout.docks);
                return Ok(json!({"applied": layout.name, "built_in": store::is_built_in_layout(&layout.name)}));
            }
            "dock.layout_remove" => {
                let name = Self::param_str(cmd, "name")?.to_string();
                let mut removed = Ok(());
                self.store.update(|p| removed = p.remove_dock_layout(&name));
                removed?;
                return Ok(json!({"removed": name}));
            }
            "profile.new" => self.ask_name(ctx, Named::NewProfile, ""),
            "profile.duplicate" => {
                let n = format!("{} copy", self.studio.profile().name);
                self.ask_name(ctx, Named::DuplicateProfile, &n);
            }
            "profile.rename" => {
                let n = self.studio.profile().name.clone();
                self.ask_name(ctx, Named::RenameProfile, &n);
            }
            "profile.remove" => self.remove_profile()?,
            "profile.switch" => {
                let name = Self::param_str(cmd, "name")?.to_string();
                self.switch_profile(&name)?;
            }
            "collection.new" => self.ask_name(ctx, Named::NewCollection, ""),
            "collection.duplicate" => {
                let n = format!("{} copy", self.studio.collection().name);
                self.ask_name(ctx, Named::DuplicateCollection, &n);
            }
            "collection.rename" => {
                let n = self.studio.collection().name.clone();
                self.ask_name(ctx, Named::RenameCollection, &n);
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
                let list: Vec<Value> = self
                    .displays
                    .list()
                    .into_iter()
                    .map(|d| json!({"id": d.id, "name": d.name, "width": d.width, "height": d.height, "primary": d.primary}))
                    .collect();
                let zoom = ctx.zoom_factor();
                let window = ctx.input(|i| window_display(i.viewport(), zoom)).map(|(w, h)| json!({"width": w, "height": h}));
                return Ok(json!({"displays": list, "window": window}));
            }
            "video.canvas_from_display" => {
                let zoom = ctx.zoom_factor();
                let list = self.displays.list();
                // The canvas follows the display it came from. The display the window is on is
                // followed when it is the only listed display of its size.
                let (size, display) = match cmd.params.get("display") {
                    None => {
                        let size = ctx.input(|i| window_display(i.viewport(), zoom)).ok_or_else(|| {
                            tmsg!("`video.canvas_from_display`: the size of the display the window is on is not known; pass `display`")
                        })?;
                        (size, follow::of_size(&list, size))
                    }
                    Some(Value::String(id)) => {
                        let d = find_display(&list, id)?;
                        ((d.width, d.height), Some(d))
                    }
                    Some(_) => return Err(tmsg!("`video.canvas_from_display`: `display` is a display id from `video.displays`")),
                };
                let display = display.cloned();
                let size = canvas_for(size)?;
                // The canvas follows the display only once the profile is saved.
                let mut r = self.set_canvas(size)?;
                self.follow_display(display.as_ref());
                r["follows"] = self.followed_display().map_or(Value::Null, |l| json!(l.id));
                return Ok(r);
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
            id if commands::SCENE_HEADER.iter().chain(&commands::SOURCE_HEADER).any(|(_, _, c, _)| *c == id) => {
                if !(cmd.params.is_null() || cmd.params.as_object().is_some_and(serde_json::Map::is_empty)) {
                    return Err(tmsg!("`{command}` takes no parameters", command = id));
                }
                return self.header_command(id);
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
            "selection.fit" => return self.selection_fit(FitMode::Fit),
            "selection.fill" => return self.selection_fit(FitMode::Fill),
            "selection.stretch" => return self.selection_fit(FitMode::Stretch),
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
            "app.language" => return self.language_command(cmd),
            "pause.settings" => {
                self.open_settings(Tab::Stream);
                if let Some(w) = &mut self.settings {
                    w.show_pause_screen();
                }
            }
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

    /// A header button of the Scenes or Sources dock (`commands::SCENE_HEADER`,
    /// `commands::SOURCE_HEADER`). A command that its button shows disabled is an error.
    /// `sources.add` opens the Add menu, or closes it when it is open, and replies whether it is
    /// open.
    fn header_command(&mut self, id: &str) -> Result<Value, Msg> {
        if !self.item_state(id).enabled {
            return Err(tmsg!("`{command}` cannot run now", command = id));
        }
        let edit = self.studio.engine().edit_scene();
        let item = self.selection().map(|(item, source, _)| (item, source));
        let run = |app: &mut StudioApp, id: &str, params: Value| app.studio.run(id, &params).map(|(v, _)| v).map_err(|e| Msg::plain(e.to_string()));
        match (id, item) {
            ("scenes.add", _) => run(self, "scene.add", json!({})),
            ("scenes.remove", _) => run(self, "scene.remove", json!({"scene": edit})),
            ("scenes.move_up", _) => run(self, "scene.move", json!({"scene": edit, "to": "up"})),
            ("scenes.move_down", _) => run(self, "scene.move", json!({"scene": edit, "to": "down"})),
            ("sources.add", _) => {
                self.lists.add_open = !self.lists.add_open;
                Ok(json!({"open": self.lists.add_open}))
            }
            ("sources.remove", Some((item, _))) => {
                let r = run(self, "item.remove", json!({"item": item}))?;
                self.selected = None;
                Ok(r)
            }
            ("sources.move_up", Some((item, _))) => run(self, "item.move", json!({"item": item, "to": "up"})),
            ("sources.move_down", Some((item, _))) => run(self, "item.move", json!({"item": item, "to": "down"})),
            ("sources.properties", Some((_, source))) => {
                self.open_properties(source);
                Ok(Value::Null)
            }
            _ => Err(tmsg!("select a source first")),
        }
    }

    /// Put every dock setting to `docks`. Floating dock windows open again at the places and sizes
    /// `docks` holds.
    fn set_docks(&mut self, docks: store::Docks) {
        self.store.update(|p| p.docks = docks);
        self.dock_window_rect = [None; store::DOCK_COUNT];
        self.dock_window_gen = self.dock_window_gen.wrapping_add(1);
    }

    /// Save the current docks as the layout `name`; whether a layout with that name was replaced.
    fn save_dock_layout(&mut self, name: &str) -> Result<bool, Msg> {
        let mut result = Ok(false);
        self.store.update(|p| result = p.save_dock_layout(name));
        result
    }

    /// Ask for a name for `what` in the name prompt, opening with `text`. A prompt already open is
    /// closed first.
    fn ask_name(&mut self, ctx: &egui::Context, what: Named, text: &str) {
        if let Some((host, id)) = self.window_dialogs.clone().zip(self.naming.as_ref().and_then(|n| n.dialog)) {
            host.with(|h| {
                h.cancel_by_id(ctx, id);
                h.take_by_id::<HostedParamDialog>(id);
            });
        }
        self.naming = Some(Naming { what, text: text.to_string(), dialog: None, refused: None });
        self.open_waiting_dialogs(ctx);
    }

    fn named(&mut self, what: Named, name: &str) -> Result<(), Msg> {
        // A source's name is not a file name: `source.rename` checks it, and an unchanged name
        // runs no command and leaves no undo step.
        if let Named::RenameSource(source) = what {
            if self.studio.collection().source(source).is_none_or(|s| s.name != name) {
                self.studio.run("source.rename", &json!({"source": source, "name": name})).map_err(|e| e.to_string())?;
            }
            return Ok(());
        }
        check_name(name)?;
        let taken = |list: &[String]| list.iter().any(|n| n.eq_ignore_ascii_case(name));
        match what {
            Named::NewProfile | Named::DuplicateProfile | Named::RenameProfile => {
                if taken(&self.profiles) && !(what == Named::RenameProfile && name.eq_ignore_ascii_case(&self.studio.profile().name)) {
                    return Err(tmsg!("there is already a profile named `{name}`", name = name));
                }
                // A new or duplicated profile becomes the open one, which is a switch.
                if what != Named::RenameProfile && self.studio.paused().is_some() {
                    return Err(tmsg!("Resume before switching the profile"));
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
                    let _ = self.studio.restore_profile(previous);
                    return Err(e.into());
                }
                if what == Named::RenameProfile && old != name && !case_only {
                    self.store.remove_profile(&old)?;
                }
                if what == Named::RenameProfile {
                    self.studio.stream_profile_renamed(&old, name);
                    if let Some(panel) = self.streaming.as_mut().filter(|p| p.profile == old) {
                        panel.profile = name.to_string();
                    }
                }
                self.store.update(|pr| {
                    pr.profile = name.to_string();
                    // A renamed or duplicated profile's canvas follows the display the original's
                    // did; a new profile's follows none.
                    let link = if what == Named::NewProfile || old == name { None } else { pr.canvas_display(&old).cloned() };
                    if what == Named::RenameProfile && old != name {
                        pr.forget_canvas_display(&old);
                    }
                    match link {
                        Some(mut l) => {
                            l.profile = name.to_string();
                            pr.set_canvas_display(l);
                        }
                        None if old != name => pr.forget_canvas_display(name),
                        None => {}
                    }
                });
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
            Named::RenameSource(_) => {}
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
        // The running pause shows the settings of the profile it started with.
        if self.studio.paused().is_some() {
            return Err(tmsg!("Resume before switching the profile"));
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
        self.store.update(|p| p.forget_canvas_display(&current));
        self.studio.stream_profile_removed(&current);
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
                self.from_user = true;
                let _ = self.perform(ctx, &cmd);
                self.from_user = false;
            }
            Act::Select(id) => self.selected = id,
            Act::Properties(s) => self.open_properties(s),
            Act::Filters(s) => self.open_filters(s),
            Act::Rename(s) => {
                if let Some(name) = self.studio.collection().source(s).map(|src| src.name.clone()) {
                    self.ask_name(ctx, Named::RenameSource(s), &name);
                }
            }
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
            Act::HoldStreamKey { name, key } => self.studio.hold_stream_key(&name, &key),
            Act::OpenUrl(url) => {
                if let Err(e) = studiocraft_platform::shell::open_url(&url) {
                    self.notify(e.to_string(), true);
                }
            }
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
                    self.notify(tmsg!("No file dialog could be shown: {error}", error = e), true);
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

    /// Housekeeping before drawing: studio ticks, autosave, shortcuts.
    fn before(&mut self, ctx: &egui::Context) {
        if self.fonts_for.is_some_and(|l| l != i18n::lang()) {
            self.install_fonts(ctx);
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
        self.follow_displays(ctx, now);
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
    }

    /// Before each frame's passes, from `embed::before_input`: runs the commands the runtime
    /// fired (shortcuts and menu rows) unless a prompt, the exit question or Settings is open, and
    /// returns the frame's input: the root values of [`theme::root_env`] with the interface
    /// direction, the command catalog with the user's shortcuts, and the host in the interface
    /// language. egui's own theme follows the resolved mode, for the egui widgets the window still
    /// draws itself. The window keys the runtime carried out itself (F6, F10) and the edit
    /// commands the window has no use for (Copy, Paste, Backspace's Delete) are skipped.
    pub fn frame_input(&mut self, ctx: &egui::Context, out: FrameOutput) -> FrameInput {
        if !self.modal_open() {
            for fired in &out.fired {
                let cmd = commands::fired_command(&fired.command);
                if !commands::runtime_runs(&cmd) && !commands::unused_edit(&cmd) {
                    let _ = self.perform(ctx, &cmd);
                }
            }
        }
        if *self.key_overrides != *self.keymap.overrides() {
            self.key_overrides = Arc::new(self.keymap.overrides().clone());
        }
        let mut input = CommandInput::new().overrides(self.key_overrides.clone()).gate(commands::gate());
        if let Some(catalog) = self.catalog() {
            input = input.catalog(catalog);
        }
        self.follow_language();
        // The header buttons' tooltips are in the interface language the line above settled.
        let mut states = CommandStates::new();
        for (_, _, id, tip) in commands::SCENE_HEADER.iter().chain(&commands::SOURCE_HEADER) {
            states.set(CommandRef::new(*id), CommandState::new().enabled(self.item_state(id).enabled).title(tl!(*tip)));
        }
        if *self.header_states != states {
            self.header_states.edit(|s| *s = states);
        }
        input = input.states(&self.header_states);
        let root = theme::root_env().direction(host_direction(&self.host));
        let shown = theme::egui_theme(root.resolved(ctx).mode);
        if ctx.theme() != shown {
            ctx.set_theme(shown);
        }
        FrameInput::new(root).commands(input).host(Arc::clone(&self.host))
    }

    /// Whether the name prompt, the exit question or Settings is open: the window's shortcuts and
    /// menu keys wait until it closes.
    fn modal_open(&self) -> bool {
        self.naming.is_some() || self.confirm_exit || self.settings.is_some()
    }

    /// The runtime's command catalog: every command a shortcut can be given. The commands a linked
    /// framework set defines (`edit.undo`, `edit.redo`, `edit.delete`) stay the set's and take
    /// this app's default shortcuts ([`commands::linked_defaults`]).
    fn catalog(&mut self) -> Option<AppCatalog> {
        if self.command_catalog.is_none() {
            match AppCatalog::new(commands::runtime_catalog(), 1) {
                Ok(catalog) => {
                    let linked = commands::linked_defaults();
                    self.command_catalog = Some(linked.into_iter().fold(catalog, |c, (id, bindings)| c.replace_defaults(id, bindings)));
                }
                Err(e) => log::warn!("command catalog: {e:?}"),
            }
        }
        self.command_catalog.clone()
    }

    /// The title bar: the menu titles and the output bar, with the window buttons at the ends where
    /// the app draws the window frame. The bar's empty area moves the window, and a double-click
    /// on it maximizes or restores the window. The Close button closes the window as the system's
    /// does, through the close confirmation.
    fn title_bar(&mut self, ui: &mut Ui, acts: &mut Vec<Act>) {
        let laid = self.laid_menu();
        if *self.menu_bar != laid {
            self.menu_bar.edit(|bar| *bar = laid);
        }
        let bar = std::mem::replace(&mut self.menu_bar, Tracked::new(MenuBar::new(Vec::new())));
        // The menu rows' commands come back through the frame output (`frame_input`). F10 and Alt
        // leave the menu bar alone while a prompt, the exit confirmation or Settings is open, as
        // `frame_input` drops the commands then.
        let keyboard = !self.modal_open();
        let shown = TitleBar::new().show(ui, |ui, _| {
            let room = ui.available_width();
            self.top_row(ui, room, &bar, keyboard, acts);
        });
        self.menu_bar = bar;
        self.output_bar.rect = Some(shown.output.response.rect);
        if shown.output.close_requested {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
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
        // A right-click opens the menu at the pointer; a row that runs closes it.
        craft_ui::menu::Menu::context(tl!("Sources").to_string(), &resp).show(ui, |ui, _| {
            if let Some((item, source)) = sel {
                docks::item_menu(ui, item, source, &name, renaming, acts);
                craft_ui::display::Divider::horizontal().show(ui);
            }
            craft_ui::menu::Menu::submenu_row(ui, tl!("Add").to_string(), |ui, _| {
                for p in studio.registry().sources() {
                    if craft_ui::menu::MenuItem::new(tl!(p.name()).to_string()).show(ui).ran {
                        acts.push(Act::AddSource { kind: p.id().to_string() });
                    }
                }
            });
            if craft_ui::menu::MenuItem::new(tid!("view.lock_preview", "Lock Preview").to_string()).check(locked).show(ui).ran {
                acts.push(Act::run("view.lock_preview", json!({})));
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
        let mut area = ui.available_rect_before_wrap();
        let mut top =
            ui.new_child(egui::UiBuilder::new().max_rect(area.shrink2(egui::vec2(4.0, 0.0))).layout(egui::Layout::top_down(egui::Align::Min)));
        area.min.y += self.show_pause_banner(&mut top, acts);
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
        self.dock_rects.clear();
        self.dock_strip = None;
        self.dock_columns = [None; 2];
        let now = Instant::now();
        let dt = self.last_frame.map_or(0.0, |t| now.duration_since(t).as_secs_f32());
        self.last_frame = Some(now);
        let ctx = ui.ctx().clone();
        // A floating dock's window is a layer of its own, outside the shell's measuring run, so it
        // is shown in the real run only.
        if !craft_ui::runtime::measure::active(&ctx) {
            self.dock_windows(ui, acts, dt);
        }
        // The columns beside the canvas, unless the window is too narrow for them; then their docks
        // are drawn in the strip.
        let body = ui.available_rect_before_wrap();
        let columns = [d.shown_in(store::Region::Left, false), d.shown_in(store::Region::Right, false)];
        let column_widths = [strip::column_width(d.left_width, strip::LEFT_WIDTH), strip::column_width(d.right_width, strip::RIGHT_WIDTH)];
        let span: f32 = columns.iter().zip(column_widths).filter(|(c, _)| !c.is_empty()).map(|(_, w)| w + strip::DIVIDER).sum();
        let collapsed = strip::collapsed(body.width(), span);
        self.docks_collapsed = collapsed;
        // The transition and output controls are in the output bar; Docks shows or hides them there.
        let shown = d.shown_in(store::Region::Bottom, collapsed);
        if shown.is_empty() && (collapsed || columns.iter().all(Vec::is_empty)) {
            return;
        }
        let (panel_fill, reach) = theme::with_root(&ctx, |cx| (theme::color(cx, cx.tokens.color.surface.panel), cx.tokens.size.hit.target));
        let mut st = docks::DockState {
            selected: self.selected,
            renaming: &mut self.renaming,
            lists: &mut self.lists,
            meters: &mut self.meters,
            lanes: &mut self.lanes,
            dt,
            place: None,
        };
        let studio = &self.studio;
        let rects = &mut self.dock_rects;
        // The columns' rectangles, and their widths after an edge moved this frame.
        let mut column_rects: [Option<egui::Rect>; 2] = [None; 2];
        let mut new_columns: [Option<f32>; 2] = [None; 2];
        for (c, (stacked, w)) in columns.iter().zip(column_widths).enumerate() {
            if collapsed || stacked.is_empty() {
                continue;
            }
            let left = c == 0;
            let (region, id) = if left { (store::Region::Left, "dock-left") } else { (store::Region::Right, "dock-right") };
            let panel = if left { egui::Panel::left(id) } else { egui::Panel::right(id) };
            let shown_column = panel
                .exact_size(w + strip::DIVIDER)
                .resizable(false)
                .show_separator_line(false)
                .frame(egui::Frame::NONE.fill(panel_fill))
                .show(ui, |ui| {
                    let (full, _) = ui.allocate_exact_size(ui.available_rect_before_wrap().size(), egui::Sense::hover());
                    // The 1 point line between the column and the canvas is on the column's inner side.
                    let area = if left {
                        egui::Rect::from_min_size(full.min, egui::vec2(w, full.height()))
                    } else {
                        egui::Rect::from_min_max(egui::pos2(full.right() - w, full.top()), full.max)
                    };
                    let heights = strip::stack(area.height(), stacked.len());
                    let first = rects.len();
                    let mut y = area.top();
                    for (k, (i, h)) in stacked.iter().zip(&heights).enumerate() {
                        let rect = egui::Rect::from_min_size(egui::pos2(area.left(), y), egui::vec2(area.width(), *h));
                        let mut col = ui
                            .new_child(egui::UiBuilder::new().id_salt(("dock", *i)).max_rect(rect).layout(egui::Layout::top_down(egui::Align::Min)));
                        col.set_clip_rect(rect.intersect(ui.clip_rect()));
                        st.place = Some(docks::Place { k, n: stacked.len(), column: true, region });
                        let drawn = Self::draw_dock(*i, &mut col, studio, &mut st, acts);
                        rects.push((usize::from(*i), rect, drawn, region));
                        y += h + strip::DIVIDER;
                    }
                    let most = strip::column_most(body.width(), span, w + strip::DIVIDER);
                    let edge = strip::column_edge(area, left, reach, most);
                    let out = strip::Handles::new(tl!("Resize the column"), vec![edge]).id_salt((id, "edge")).show_at(ui, edge.bar);
                    // As under the strip's top edge, a drag that starts on a header action under the
                    // edge's band is the action's.
                    for (n, (_, r)) in rects.iter().skip(first).flat_map(|(_, _, drawn, _)| drawn.actions.iter()).enumerate() {
                        if r.intersects(edge.bar) {
                            ui.interact(*r, egui::Id::unique(id).with(n), egui::Sense::drag());
                        }
                    }
                    out.moved.map(|(_, at)| strip::column_drag(&edge, area.width(), left, at, most))
                });
            column_rects[c] = Some(shown_column.response.rect);
            new_columns[c] = shown_column.inner;
        }
        let window = ctx.viewport_rect().height();
        let height = strip::height(window, body.height(), d.height);
        let saved = d.widths();
        // Widths after a divider moved this frame, by dock index.
        let mut new_widths: Option<[f32; store::DOCK_COUNT]> = None;
        let most = strip::max_height(body.height());
        let panel = (!shown.is_empty()).then(|| {
            egui::Panel::bottom("docks").exact_size(height).show_separator_line(false).frame(egui::Frame::NONE.fill(panel_fill)).show(ui, |ui| {
                let (area, _) = ui.allocate_exact_size(ui.available_rect_before_wrap().size(), egui::Sense::hover());
                let widths = strip::widths(area.width(), &shown, saved);
                let first = rects.len();
                let mut x = area.left();
                for (k, (i, w)) in shown.iter().zip(&widths).enumerate() {
                    let rect = egui::Rect::from_min_size(egui::pos2(x, area.top()), egui::vec2(*w, area.height()));
                    let mut col =
                        ui.new_child(egui::UiBuilder::new().id_salt(("dock", *i)).max_rect(rect).layout(egui::Layout::top_down(egui::Align::Min)));
                    col.set_clip_rect(rect.intersect(ui.clip_rect()));
                    st.place = Some(docks::Place { k, n: shown.len(), column: false, region: d.region(*i) });
                    let drawn = Self::draw_dock(*i, &mut col, studio, &mut st, acts);
                    rects.push((usize::from(*i), rect, drawn, store::Region::Bottom));
                    x += w + strip::DIVIDER;
                }
                // The dividers and the top edge are splitters shown after the docks, so that their
                // hit bands take the pointer over the docks' edges and their lines are drawn over
                // the docks. Each one covers only its own band, so a right click on a dock still opens
                // the dock's menu. The top edge's band reaches above the strip.
                for (k, line) in strip::dividers(area, &widths, reach).into_iter().enumerate() {
                    let out = strip::Handles::new(tl!("Resize the docks"), vec![line]).id_salt(("dock-divider", k)).show_at(ui, line.bar);
                    if let Some((_, at)) = out.moved {
                        let moved = strip::drag(&widths, k, at - line.at);
                        new_widths = Some(strip::save(saved, &shown, &moved, k));
                    }
                }
                let edge = strip::edge(area, most, reach);
                let mut edge_ui = ui.new_child(egui::UiBuilder::new().id_salt("dock-strip-edge").max_rect(edge.bar));
                edge_ui.set_clip_rect(edge.bar);
                let out = strip::Handles::new(tl!("Resize the dock strip"), vec![edge]).id_salt("dock-strip-edge").show_at(&mut edge_ui, edge.bar);
                // Each header action under the edge's band gets a shield over the edge that takes a
                // drag starting on the action, so the edge resizes only outside the actions. A click
                // still reaches the action, which is the topmost widget there that takes clicks.
                for (n, (_, r)) in rects.iter().skip(first).flat_map(|(_, _, drawn, _)| drawn.actions.iter()).enumerate() {
                    ui.interact(*r, egui::Id::unique("dock-action-shield").with(n), egui::Sense::drag());
                }
                out.moved.map(|(_, at)| at - edge.at)
            })
        });
        self.dock_strip = panel.as_ref().map(|p| p.response.rect);
        self.dock_columns = column_rects;
        // The top edge moved down by this much makes the strip that much shorter.
        let new_height = panel.and_then(|p| p.inner).map(|down| strip::height(window, body.height(), height - down));
        let [new_left, new_right] = new_columns;
        let new_columns = [new_left.filter(|w| *w != column_widths[0]), new_right.filter(|w| *w != column_widths[1])];
        if new_widths.is_some() || new_height.is_some_and(|h| h != height) || new_columns.iter().any(Option::is_some) {
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
                let [l, r] = new_columns;
                p.docks.left_width = l.unwrap_or(p.docks.left_width);
                p.docks.right_width = r.unwrap_or(p.docks.right_width);
            });
        }
    }

    /// Draw dock `i` (an index in `control::DOCK_NAMES`) in `ui`.
    fn draw_dock(i: u8, ui: &mut Ui, studio: &Studio, st: &mut docks::DockState, acts: &mut Vec<Act>) -> docks::Drawn {
        match i {
            0 => docks::scenes(ui, studio, st, acts),
            1 => docks::sources(ui, studio, st, acts),
            3 => docks::app_audio(ui, studio, st, acts),
            _ => docks::mixer(ui, studio, st, acts),
        }
    }

    /// The docks shown in windows of their own, each a framework floating group. A window is moved
    /// by its title bar and resized by its edges and corners, keeps inside the main window, and
    /// opens where it was last moved to at the size it was last given. The place and size are saved
    /// when a move or resize ends, and the first time the window is shown.
    fn dock_windows(&mut self, ui: &mut Ui, acts: &mut Vec<Act>, dt: f32) {
        self.floating_rects.clear();
        let d = self.store.prefs().docks;
        let ctx = ui.ctx().clone();
        let mut st = docks::DockState {
            selected: self.selected,
            renaming: &mut self.renaming,
            lists: &mut self.lists,
            meters: &mut self.meters,
            lanes: &mut self.lanes,
            dt,
            place: None,
        };
        let studio = &self.studio;
        let mut changed: Vec<(usize, [f32; 2], [f32; 2])> = Vec::new();
        let bounds = ctx.viewport_rect();
        let sizes = theme::with_root(&ctx, DockSizes::style);
        // The window's size limits are the body's; the chrome over the body adds its height, so the
        // whole window is at least `MIN_WIDTH` by `MIN_HEIGHT`.
        let chrome = sizes.window_chrome(true);
        let least = egui::vec2(strip::MIN_WIDTH, (strip::MIN_HEIGHT - chrome).max(0.0));
        for i in d.floating_shown() {
            let k = usize::from(i).min(store::DOCK_COUNT - 1);
            // Window sizes are outer sizes, with the window's chrome in them.
            let size = d.window_size(i).map_or(egui::vec2(strip::default_width(k), strip::DEFAULT_HEIGHT), |[w, h]| egui::vec2(w, h));
            let size = size.min(egui::Vec2::splat(SizeLimits::BOUND));
            let place = d.window_pos(i).map_or_else(|| strip::window_place(bounds, size, k), |[x, y]| egui::pos2(x, y));
            let rect = self.dock_window_rect[k].unwrap_or(egui::Rect::from_min_size(place, size));
            let shown = FloatingGroup::new(tl!(control::DOCK_TITLES[k]).to_string(), rect)
                .id_salt(("dock-window", k, self.dock_window_gen))
                .bounds(bounds)
                .limits(SizeLimits::at_least(least))
                .sized(Sizing::Fill)
                .actions([])
                .show_group(ui, |ui, _| match i {
                    0 => docks::scenes(ui, studio, &mut st, acts),
                    1 => docks::sources(ui, studio, &mut st, acts),
                    3 => docks::app_audio(ui, studio, &mut st, acts),
                    _ => docks::mixer(ui, studio, &mut st, acts),
                });
            let window = shown.output.response.rect;
            // The place and size are saved when a move or resize ends; while it runs, the window
            // shows at the rect it reported. A canceled one saves nothing. A resize keeps the saved
            // coordinate on each axis its edge did not move, which can differ from the drawn one
            // while a smaller main window pushes the window back on screen.
            let mut save = d.window_pos(i).is_none().then_some(egui::Rect::from_min_size(place, size));
            if let Some(change) = shown.output.placed {
                match change.phase {
                    GesturePhase::Commit => {
                        self.dock_window_rect[k] = None;
                        let (moves_x, moves_y) = match change.kind {
                            PlacementKind::Size(edge) => (
                                matches!(edge, ResizeEdge::Left | ResizeEdge::TopLeft | ResizeEdge::BottomLeft),
                                matches!(edge, ResizeEdge::Top | ResizeEdge::TopLeft | ResizeEdge::TopRight),
                            ),
                            _ => (true, true),
                        };
                        let at = egui::pos2(if moves_x { change.rect.min.x } else { place.x }, if moves_y { change.rect.min.y } else { place.y });
                        save = Some(egui::Rect::from_min_size(at, change.rect.size()));
                    }
                    GesturePhase::Cancel => self.dock_window_rect[k] = None,
                    _ => self.dock_window_rect[k] = Some(change.rect),
                }
            }
            if let Some(r) = save {
                let (at, outer) = ([r.min.x, r.min.y], [r.width(), r.height()]);
                if at.iter().chain(&outer).all(|v| v.is_finite()) && { d }.place_window(k, at, outer) {
                    changed.push((k, at, outer));
                }
            }
            let Some(drawn) = shown.inner else { continue };
            let top = window.top() + sizes.top_band;
            let bar =
                egui::Rect::from_min_max(egui::pos2(window.left(), top), egui::pos2(window.right(), (top + sizes.title_row).min(window.bottom())));
            self.floating_rects.push((k, window, drawn, bar));
        }
        if !changed.is_empty() {
            self.store.update(|p| {
                for (k, place, outer) in &changed {
                    p.docks.place_window(*k, *place, *outer);
                }
            });
        }
    }

    /// The status bar in the shell's status slot. Its members stand at the end edge: the pause
    /// with its countdown, the audio tracks state, the running replay buffer, recording and stream
    /// with their times and the count of destinations reconnecting, the drop counts and, last, the
    /// frame rate and frame time.
    fn status_bar(&mut self, ui: &mut Ui) {
        let stats = self.studio.stats();
        let fps = self.studio.video_settings().fps();
        let rec = self.studio.output(OutputKind::Recording);
        let live = self.studio.output(OutputKind::Streaming);
        let replay = self.studio.output(OutputKind::Replay);
        let outputs_run = [&rec, &live].into_iter().flatten().any(|s| s.active);
        let source_limit = self.studio.audio_tracks_source_limit();
        let tracks = self.studio.audio_tracks_status().and_then(|s| tracks_item(s, outputs_run, source_limit));
        let pause = pause_item(self.studio.paused(), self.studio.paused_for_ms());
        let clock = |s: &studiocraft_studio::OutputState| {
            let t = s.elapsed.as_secs();
            format!("{:02}:{:02}:{:02}", t / 3600, (t / 60) % 60, t % 60)
        };
        // Each timer: its text, its tone and its hover text.
        let mut timers: Vec<(String, Tone, Option<&str>)> = Vec::new();
        for (s, tag) in [(replay, tl!("REPLAY")), (rec, tl!("REC"))] {
            if let Some(s) = s.filter(|s| s.active) {
                timers.push((format!("{tag} {}", clock(&s)), Tone::Bad, None));
            }
        }
        if let Some(s) = live.filter(|s| s.active) {
            let (tag, waiting, warn) = stream_tag(s.destinations);
            if let Some(waiting) = waiting {
                let hover = tl!("Destinations waiting to connect again; the Streaming window shows each one and why");
                timers.push((waiting, Tone::Warn, Some(hover)));
            }
            timers.push((format!("{tag} {}", clock(&s)), if warn { Tone::Warn } else { Tone::Bad }, None));
        }
        let rate = craft_i18n::fmt(tl!("{fps} fps  {ms} ms"), &[("fps", &format!("{fps:.2}")), ("ms", &format!("{:.1}", stats.render_avg_ms))]);
        let (dropped, lagged) = (stats.encoder_dropped.to_string(), stats.lagged.to_string());
        let drops = craft_i18n::fmt(tl!("dropped {dropped}  lagged {lagged}"), &[("dropped", &dropped), ("lagged", &lagged)]);
        // An empty label names the bar with the framework's words for a status bar.
        let _ = Bar::new("", BarKind::Status)
            .trailing(|ui, _| {
                if let Some((text, hover)) = &pause {
                    let _ = Text::new(text.clone()).tooltip(TooltipSpec::new(hover.clone())).tone(Tone::Warn).show(ui);
                    Divider::vertical().show(ui);
                }
                if let Some((text, warning, hover)) = &tracks {
                    let text = Text::new(text.clone()).tooltip(TooltipSpec::new(hover.clone()));
                    let _ = if *warning { text.tone(Tone::Warn) } else { text }.show(ui);
                    Divider::vertical().show(ui);
                }
                for (timer, tone, hover) in &timers {
                    let text = Text::new(timer.clone()).tone(*tone);
                    let _ = match hover {
                        Some(hover) => text.tooltip(TooltipSpec::new(hover.to_string())),
                        None => text,
                    }
                    .show(ui);
                    Divider::vertical().show(ui);
                }
                let _ = Text::new(drops.clone()).role(TypeRole::Mono).show(ui);
                Divider::vertical().show(ui);
                let _ = Text::new(rate.clone()).role(TypeRole::Mono).show(ui);
            })
            .show(ui, |_, _| {});
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
                    Pick::AudioEditor => Browse {
                        key: String::new(),
                        mode: studiocraft_model::PathMode::OpenFile,
                        filter_name: tl!("Programs").to_string(),
                        extensions: if cfg!(windows) { vec!["exe".into()] } else { vec![] },
                    },
                    Pick::PauseMedia => Browse {
                        key: String::new(),
                        mode: studiocraft_model::PathMode::OpenFile,
                        filter_name: tl!("Pictures and videos").to_string(),
                        extensions: studiocraft_studio::pause_media_extensions(self.studio.registry()),
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
                i18n::horizontal(ui, |ui| {
                    ui.strong(tl!("Credits"));
                    ui.with_layout(i18n::trailing(ui), |ui| {
                        let modes: Vec<_> = credits::NameMode::ALL.iter().map(|&m| Entry::new(m, tl!(m.label()))).collect();
                        let _ = Select::new(tl!("Show"), Bind::bound(&mut *mode), &modes).id_salt("credit-names").show(ui);
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
    }

    /// Draw one frame of the whole window.
    pub fn frame(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        // egui runs this again in the same frame when a pass asks for a discard; the frame
        // counts once.
        if ctx.current_pass_index() == 0 {
            self.frames += 1;
        }
        self.before(&ctx);
        self.dialog_answers(&ctx);
        let background = theme::with_root(&ctx, |cx| theme::color(cx, cx.tokens.color.surface.background));
        // The title bar is the shell's title slot, and its edges resize the window where the app
        // draws the frame. The status bar is the shell's status slot, shown while View › Status Bar
        // is on, and the docks and the canvas fill the shell's body. The
        // shell runs the window's dialog host after the body, so a prompt opened in the body shows
        // in the same pass.
        let status_bar = self.store.prefs().status_bar;
        let mut pass = Pass { app: self, acts: Vec::new() };
        AppShell::new(WindowKey::main())
            .title_bar(|ui, _, pass: &mut Pass<'_>| pass.app.title_bar(ui, &mut pass.acts))
            .status(status_bar, |ui, _, pass: &mut Pass<'_>| pass.app.status_bar(ui))
            .body(move |ui, bcx, pass: &mut Pass<'_>| {
                if let Some(host) = bcx.dialogs() {
                    pass.app.window_dialogs = Some(host);
                    pass.app.open_waiting_dialogs(ui.ctx());
                }
                pass.app.docks(ui, &mut pass.acts);
                egui::CentralPanel::default().frame(egui::Frame::NONE.fill(background)).show(ui, |ui| pass.app.central(ui, &mut pass.acts));
            })
            .show(ui, &mut pass);
        let mut acts = pass.acts;
        self.prompt_answers(&ctx);
        self.windows(&ctx, &mut acts);
        self.stats_window(ui);
        self.streaming_window(ui, &mut acts);
        self.encoders_window(ui);
        self.scopes_window(ui, &mut acts);
        // An update never starts while a recording, stream or replay buffer runs.
        let idle = !self.outputs_running();
        match updates_ui::show(&mut self.updates, self.window_dialogs.as_ref(), &ctx, idle) {
            updates_ui::Asked::Nothing => {}
            updates_ui::Asked::OpenUrl(url) => {
                if let Err(e) = studiocraft_platform::shell::open_url(&url) {
                    self.notify(e.to_string(), true);
                }
            }
            updates_ui::Asked::Close => self.request_exit(&ctx),
        }
        for a in acts {
            self.act(&ctx, a);
        }
        // The notices of this frame go to the window's toast queue. The shell shows them as cards
        // at the end-bottom corner of its body from the next frame on.
        self.post_notices(&ctx);
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
        embed::logic(ctx, || self.close_decision(ctx));
    }

    /// Whether the window may close now that it was asked to: it is held with the exit question
    /// open while an output runs and the user has not confirmed, and once when the scene
    /// collection could not be saved.
    fn close_decision(&mut self, ctx: &egui::Context) -> CloseDecision {
        let busy = self.outputs_running();
        if busy && !self.exit_confirmed {
            self.confirm_exit = true;
            self.open_waiting_dialogs(ctx);
            CloseDecision::Hold
        } else if !self.save_all() && !self.close_despite_failed_save {
            // The window stays open with the save error shown; closing again closes it.
            self.close_despite_failed_save = true;
            CloseDecision::Hold
        } else {
            CloseDecision::Close
        }
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
        self.dialogs = Some(craft_dialogs::Dialogs::new(craft_dialogs::Wake::new(move || c.request_repaint())));
    }

    /// Show the queued file dialog, at the end of a frame.
    pub fn show_dialogs(&mut self, frame: &eframe::Frame) {
        if let Some(d) = &mut self.dialogs {
            d.show_pending(Some(frame));
        }
    }

    /// Before each frame's passes: hand the runtime this frame's input and run what it fired last
    /// frame. The desktop calls it from eframe's `raw_input_hook`; a test calls it before each
    /// pass it runs.
    pub fn before_input(&mut self, ctx: &egui::Context) {
        embed::before_input(ctx, |out| self.frame_input(ctx, out));
    }
}

/// The host the UI runtime reads `catalogs` and the interface language from.
fn app_host(catalogs: Arc<I18n>) -> Host {
    Host::builder(AppInfo { id: APP_ID, name: APP_NAME, version: env!("CARGO_PKG_VERSION") }).i18n(catalogs).lang(i18n::lang()).build()
}

/// The framework's words in `host`'s catalogs and language.
fn host_words(host: &Host) -> Arc<Words> {
    Arc::new(match host.i18n_shared() {
        Some(catalogs) => Words::from_i18n(Arc::clone(catalogs), host.lang()),
        None => Words::english(),
    })
}

/// The layout direction of `host`'s language: right to left for Arabic and Hebrew.
fn host_direction(host: &Host) -> craft_ui::runtime::env::Direction {
    use craft_ui::runtime::env::Direction;
    if host.i18n().is_some_and(|catalogs| catalogs.rtl(host.lang())) { Direction::Rtl } else { Direction::Ltr }
}

/// The desktop window the UI runtime serves: who draws its frame, the system's title bar
/// double-click setting and where the window's placement is kept.
pub struct DesktopWindow {
    /// The frame decision the window opened with (`craft_window::policy::decide_for`).
    pub frame: Decision,
    /// What a double-click on the title bar does.
    pub double_click: DoubleClick,
    /// The store the runtime records the window's size and place in ([`placement::store`]).
    pub placement: Arc<std::sync::Mutex<craft_window::restore::Store>>,
}

/// Install the UI runtime and the interface fonts on `ctx`, once, before the first frame. On the
/// desktop the runtime reads the running platform's shell ([`desktop_shell`]), the window's frame
/// and the window services, and keeps the window's placement; otherwise (tests) it gets the
/// default shell, the system's frame and no window services.
pub fn install_runtime(ctx: &egui::Context, desktop: Option<DesktopWindow>) -> Result<(), InstallError> {
    let spec = match desktop {
        Some(desktop) => InstallSpec::new(desktop_shell(ShellInfo::current(), desktop.double_click), Arc::new(SystemWindows))
            .app_id(BUNDLE_ID)
            .frame(desktop.frame)
            .placement(desktop.placement),
        None => InstallSpec::new(ShellInfo::default(), Arc::new(NoWindows)),
    };
    install_spec(ctx, spec)
}

/// What the shell hands its title bar and its body each frame: the app and the actions they ask
/// for.
struct Pass<'a> {
    app: &'a mut StudioApp,
    acts: Vec<Act>,
}

/// The desktop window's shell: `shell` with the system's title bar double-click setting
/// `double_click`, and without the OS menu bar capability, so the menu bar draws in the window on
/// every platform. StudioCraft installs no OS menu bar.
pub(crate) fn desktop_shell(shell: ShellInfo, double_click: DoubleClick) -> ShellInfo {
    let caps = shell.caps.difference(Caps::NATIVE_MENU_BAR);
    shell.caps(caps).double_click(double_click)
}

/// Install the UI runtime from `spec` and the interface fonts on `ctx`.
pub(crate) fn install_spec(ctx: &egui::Context, spec: InstallSpec) -> Result<(), InstallError> {
    let lang = i18n::lang();
    embed::install(ctx, spec, &fonts::ui_fonts(i18n::I18N.code(lang), fonts::CRAFT_FONTS)).map(|_| ())
}

/// A context with the UI runtime installed, as tests drive the window.
#[cfg(test)]
pub(crate) fn test_context() -> egui::Context {
    let ctx = egui::Context::default();
    if let Err(e) = install_runtime(&ctx, None) {
        panic!("the UI runtime did not install: {e:?}");
    }
    ctx
}

impl eframe::App for StudioApp {
    fn raw_input_hook(&mut self, ctx: &egui::Context, _raw: &mut egui::RawInput) {
        self.before_input(ctx);
    }

    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        StudioApp::logic(self, ctx);
    }

    fn ui(&mut self, ui: &mut Ui, frame: &mut eframe::Frame) {
        embed::before_ui(ui.ctx(), frame);
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

/// The status bar's tag for a running stream with destinations `c`, the count of those waiting to
/// connect again when some are live, and whether the tag takes the warning color: `RECONNECTING`
/// when none is live and at least one waits, otherwise `LIVE`.
fn stream_tag(c: studiocraft_plugin::DestinationCounts) -> (&'static str, Option<String>, bool) {
    if c.live == 0 && c.retrying > 0 {
        return (tl!("RECONNECTING"), None, true);
    }
    let waiting = (c.retrying > 0).then(|| craft_i18n::fmt(tl!("{count} reconnecting"), &[("count", &c.retrying.to_string())]));
    (tl!("LIVE"), waiting, false)
}

/// The pause item of the status bar while the program is paused: `PAUSED` with the countdown's
/// time left, and the time paused on hover. `paused` holds the countdown's time left in
/// milliseconds, or `None` when the pause has no countdown.
fn pause_item(paused: Option<Option<u64>>, for_ms: Option<u64>) -> Option<(String, String)> {
    let left = paused?;
    let text = match left {
        Some(ms) => format!("{} {}", tl!("PAUSED"), studiocraft_studio::countdown_text(ms)),
        None => tl!("PAUSED").to_string(),
    };
    let since = studiocraft_studio::paused_text(for_ms.unwrap_or(0));
    Some((text, craft_i18n::fmt(tl!("Paused for {time}"), &[("time", &since)])))
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

/// The canvas size a profile takes for `size`: each side in the canvas's range and even.
fn canvas_size((width, height): (u32, u32)) -> (u32, u32) {
    let v = studiocraft_model::VideoSettings { base_width: width, base_height: height, ..Default::default() }.sanitized();
    (v.base_width, v.base_height)
}

#[cfg(test)]
mod tests;
