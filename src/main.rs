//! StudioCraft desktop app.
//!
//! Usage: `studiocraft [--config-dir <dir> | --no-prefs] [--safe-mode] [--control <port>]`,
//! `studiocraft --screenshot <file.png>`, `studiocraft --self-test <file.json>`,
//! `studiocraft --version`, `studiocraft --help`.
//!
//! `--control <port>` starts a JSON-lines control server on `127.0.0.1:<port>`; every request
//! carries the token in `STUDIOCRAFT_CONTROL_TOKEN` (64 hexadecimal characters). See
//! `docs/control-protocol.md`.
//!
//! `--safe-mode` starts with no source running and the control server not started, until the
//! user leaves Safe Mode in the window. A start after a session that ended without closing (its
//! marker in the settings folder's `sessions` folder was left behind) asks whether to start in
//! Safe Mode.
//!
//! `--screenshot` draws about 120 frames, saves the window's picture and exits; `--self-test`
//! drives the window through a short script and writes what happened. Both open the window
//! without focus (behind other windows on Windows) and keep their settings in memory.
#![cfg_attr(all(target_os = "windows", not(debug_assertions)), windows_subsystem = "windows")]

mod args;
mod control;
mod harness;
mod input;
mod titlebar;

use std::process::ExitCode;
use std::sync::{Arc, OnceLock};

use craft_settings::Location;
use studiocraft_render::device::{Notice, Preference};
use studiocraft_studio::session_marker::{Previous, SessionMarker};
use studiocraft_ui_egui::hardware;
use studiocraft_ui_egui::store::Store;
use studiocraft_ui_egui::{APP_NAME, BUNDLE_ID, SafeStart, StudioApp};

use crate::args::{Action, Args};

/// End the session after the window and the studio have closed: remove its marker for a clean
/// end, or with `keep_unclean` leave it in place, so that the next start still finds a session
/// that did not close.
fn end_session(marker: Option<SessionMarker>, keep_unclean: bool) {
    match marker {
        Some(m) if !keep_unclean => {
            if let Err(e) = m.finish() {
                log::warn!("the session marker could not be removed: {e}");
            }
        }
        _ => {}
    }
}

struct Shell {
    app: StudioApp,
    harness: Option<harness::Harness>,
    control: Option<control::Control>,
    /// The `--control` port while Safe Mode keeps the control server from starting.
    held_control: Option<u16>,
    /// The command line, to start the app again with after its graphics device was lost.
    relaunch_args: Vec<std::ffi::OsString>,
}

impl Shell {
    /// Start the control server held back by Safe Mode once Safe Mode is off.
    fn start_held_control(&mut self) {
        if self.app.safe_mode() {
            return;
        }
        if let Some(port) = self.held_control.take() {
            match control::Control::start(port) {
                Ok(c) => self.control = Some(c),
                Err(e) => self.app.control_failed(e),
            }
        }
    }
}

impl eframe::App for Shell {
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw: &mut egui::RawInput) {
        if let Some(h) = &mut self.harness {
            h.inject(raw);
        }
        if let Some(c) = &mut self.control {
            c.inject(raw);
        }
    }

    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.start_held_control();
        if let Some(c) = &mut self.control {
            c.serve(ctx, &mut self.app);
        }
        if let Some(h) = &mut self.harness {
            h.step(ctx, &mut self.app);
        }
        eframe::App::logic(&mut self.app, ctx, frame);
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        eframe::App::ui(&mut self.app, ui, frame);
    }

    fn on_exit(&mut self) {
        eframe::App::on_exit(&mut self.app);
        if self.app.relaunch_requested() {
            // The new copy starts in Safe Mode when this session is in it, and not when it was left.
            let mut args: Vec<&std::ffi::OsStr> = self.relaunch_args.iter().map(|a| a.as_os_str()).filter(|a| *a != "--safe-mode").collect();
            if self.app.safe_mode() {
                args.push(std::ffi::OsStr::new("--safe-mode"));
            }
            match std::env::current_exe().and_then(|exe| std::process::Command::new(exe).args(args).spawn()) {
                Ok(_) => log::warn!("the graphics device was lost; StudioCraft starts again"),
                Err(e) => eprintln!("studiocraft: the graphics device was lost, and StudioCraft could not start again: {e}"),
            }
        }
    }
}

const HELP: &str = "StudioCraft: record and stream scenes.

Usage:
  studiocraft [--config-dir <dir> | --no-prefs] [--safe-mode] [--control <port>]
  studiocraft --screenshot <file.png>
  studiocraft --self-test <file.json>
  studiocraft --version

Options:
  --config-dir <dir>      keep settings, scene collections and profiles in <dir>
  --no-prefs              keep everything in memory; nothing is read or written
  --safe-mode             start with no source running and remote control off, until
                          Leave Safe Mode is clicked in the window
  --control <port>        accept control requests on 127.0.0.1:<port>; requests carry the
                          token in STUDIOCRAFT_CONTROL_TOKEN (64 hexadecimal characters)
  --screenshot <file>     draw about 120 frames, save the window as a PNG and exit
  --self-test <file>      run a short scripted session, write its steps as JSON and exit
  --version               print `StudioCraft <version>` and exit

Environment:
  STUDIOCRAFT_TITLEBAR    `app` draws StudioCraft's own title bar, `system` keeps the desktop's
";

fn run(args: Args, raw: Vec<std::ffi::OsString>) -> Result<(), String> {
    let harness = harness::Harness::from_args(&args);
    let location = if args.no_prefs || harness.is_some() {
        Location::Memory
    } else if let Some(dir) = &args.config_dir {
        Location::Dir(dir.clone())
    } else {
        Location::Auto
    };
    let (store, mut problems) = Store::open(location.clone());
    // Settings kept in memory have no folder for a marker, and scripted runs keep their settings
    // in memory.
    let (marker, previous) = match store.dir().map(SessionMarker::begin) {
        Some(Ok((m, previous, unread))) => {
            problems.extend(unread.into_iter().map(|p| format!("a session marker could not be read or removed: {p}")));
            (Some(m), previous)
        }
        Some(Err(e)) => {
            problems.push(format!("a crash in this session will not be noticed at the next start: the session marker could not be written: {e}"));
            (None, Previous::Clean)
        }
        None => (None, Previous::Clean),
    };
    let start = if args.safe_mode {
        SafeStart::Safe
    } else if previous == Previous::Unclean {
        SafeStart::Ask
    } else {
        SafeStart::Normal
    };
    // A session that stops before its window opens did not run: the marker it took over from a
    // session that did not close stays for the start that follows.
    let unclean = previous == Previous::Unclean;
    // In Safe Mode the control server starts when Safe Mode is left; a missing or bad token is
    // still reported now.
    let (control, held_control) = match args.control {
        Some(port) if start != SafeStart::Normal => {
            if let Err(e) = control::token() {
                end_session(marker, unclean);
                return Err(e);
            }
            (None, Some(port))
        }
        Some(port) => match control::Control::start(port) {
            Ok(c) => (Some(c), None),
            Err(e) => {
                end_session(marker, unclean);
                return Err(e);
            }
        },
        None => (None, None),
    };
    let buttons = titlebar::decide(store.prefs().system_title_bar);
    let mut viewport = egui::ViewportBuilder::default()
        .with_title(APP_NAME)
        .with_app_id(BUNDLE_ID)
        .with_inner_size([1280.0, 800.0])
        .with_min_inner_size([760.0, 480.0]);
    if let Some(icon) = studiocraft_ui_egui::app_icon::window_icon() {
        viewport = viewport.with_icon(icon);
    }
    viewport = titlebar::viewport(viewport, buttons.as_ref());
    if harness.is_some() {
        viewport = viewport.with_active(false);
        if cfg!(windows) {
            viewport = viewport.with_window_level(egui::WindowLevel::AlwaysOnBottom);
        }
    }
    // The graphics adapter: the saved choice, or software after repeated device losses, or
    // software for this run when the automatic choice could not open the window.
    let software_once = std::env::var_os(SOFTWARE_ONCE).is_some();
    let (adapter, after_losses) = if software_once {
        (Preference::Software, None)
    } else {
        hardware::startup(&store.prefs().graphics_adapter, &store.prefs().graphics_losses, hardware::now_secs())
    };
    let tried = adapter.clone();
    let retry_args = raw.clone();
    let picked = Arc::new(OnceLock::new());
    let mut options = eframe::NativeOptions { viewport, renderer: eframe::Renderer::Wgpu, run_and_return: true, ..eframe::NativeOptions::default() };
    options.wgpu_options.wgpu_setup = hardware::wgpu_setup(adapter.clone(), Arc::clone(&picked));
    // Scripted runs and sessions that keep nothing never look for updates, and do not start
    // again after a device loss. Nor does a session driven through `--control`: its listener
    // holds the port until this process ends.
    let update_checks = !args.no_prefs && harness.is_none();
    let may_relaunch = update_checks && args.control.is_none();
    let opened = eframe::run_native(
        APP_NAME,
        options,
        Box::new(move |cc| {
            studiocraft_ui_egui::theme::install(&cc.egui_ctx);
            let mut app = StudioApp::boot_with(store, cc.wgpu_render_state.clone(), problems, start);
            // After boot, which sets the interface language from the saved settings.
            app.install_fonts(&cc.egui_ctx);
            app.set_update_checks(update_checks);
            app.set_title_bar(buttons);
            let picked = picked.get().map_or(&[][..], Vec::as_slice);
            // A run in software after Automatic could not open the window says so, unless no
            // software adapter was found and the window draws on hardware after all.
            let fell_back = (software_once && !picked.contains(&Notice::NoSoftwareAdapter)).then_some(Notice::SoftwareOnly);
            let notices = after_losses.iter().chain(&fell_back).chain(picked).map(hardware::notice_msg).collect();
            app.set_window_gpu(adapter, notices, may_relaunch);
            Ok(Box::new(Shell { app, harness, control, held_control, relaunch_args: raw }))
        }),
    );
    // The window and the studio have closed, outputs included. A window that could not open did
    // not run a session.
    end_session(marker, opened.is_err() && unclean);
    // A process has one window event loop, so another adapter is tried by starting again: a
    // choice other than Automatic becomes Automatic (with the losses forgotten, so it would not
    // stop every later start), Automatic is followed by one run in software, and that run is not
    // retried.
    match opened {
        Ok(()) => Ok(()),
        Err(eframe::Error::Wgpu(e)) if may_relaunch && !software_once => {
            let next = if tried == Preference::Auto {
                "software"
            } else {
                let (mut store, _) = Store::open(location);
                store.update(|p| {
                    p.graphics_adapter = Preference::Auto;
                    p.graphics_losses.clear();
                });
                if let Err(saved) = store.flush() {
                    return Err(format!("the window could not be opened: {e}; the graphics adapter setting could not be reset: {saved}"));
                }
                "the automatic choice"
            };
            let mut again = std::process::Command::new(std::env::current_exe().map_err(|x| format!("the window could not be opened: {e}; {x}"))?);
            again.args(&retry_args);
            if tried == Preference::Auto {
                again.env(SOFTWARE_ONCE, "1");
            }
            match again.spawn() {
                Ok(_) => {
                    log::warn!("the window could not be opened on {tried:?} ({e}); StudioCraft starts again on {next}");
                    Ok(())
                }
                Err(spawn) => Err(format!("the window could not be opened: {e}; StudioCraft could not start again on {next}: {spawn}")),
            }
        }
        Err(e) => Err(format!("the window could not be opened: {e}")),
    }
}

/// Set for a run that starts in software because the automatic adapter choice could not open the
/// window. The settings are not changed.
const SOFTWARE_ONCE: &str = "STUDIOCRAFT_GPU_SOFTWARE";

fn main() -> ExitCode {
    // On Windows the update helper is a copy of this program started with the updater's own
    // arguments: it finishes the update, starts the app again and exits.
    let raw: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    if let Some(result) = craft_update::self_update::finish(&studiocraft_ui_egui::UPDATE_APP, &raw) {
        return match result {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("studiocraft: {e}");
                ExitCode::FAILURE
            }
        };
    }
    let unicode: Result<Vec<String>, String> =
        raw.iter().cloned().map(|a| a.into_string().map_err(|a| format!("`{}` is not valid Unicode", a.to_string_lossy()))).collect();
    match unicode.and_then(args::parse) {
        Ok(Action::Version) => {
            println!("{APP_NAME} {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Ok(Action::Help) => {
            print!("{HELP}");
            ExitCode::SUCCESS
        }
        Ok(Action::Run(a)) => match run(a, raw) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("studiocraft: {e}");
                ExitCode::FAILURE
            }
        },
        Err(e) => {
            eprintln!("studiocraft: {e}\n\n{HELP}");
            ExitCode::from(2)
        }
    }
}
