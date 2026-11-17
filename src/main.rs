//! StudioCraft desktop app.
//!
//! Usage: `studiocraft [--config-dir <dir> | --no-prefs] [--control <port>]`,
//! `studiocraft --screenshot <file.png>`, `studiocraft --self-test <file.json>`,
//! `studiocraft --version`, `studiocraft --help`.
//!
//! `--control <port>` starts a JSON-lines control server on `127.0.0.1:<port>`; every request
//! carries the token in `STUDIOCRAFT_CONTROL_TOKEN` (64 hexadecimal characters). See
//! `docs/control-protocol.md`.
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
use studiocraft_render::device::Preference;
use studiocraft_ui_egui::hardware;
use studiocraft_ui_egui::store::Store;
use studiocraft_ui_egui::{APP_NAME, BUNDLE_ID, StudioApp};

use crate::args::{Action, Args};

struct Shell {
    app: StudioApp,
    harness: Option<harness::Harness>,
    control: Option<control::Control>,
    /// The command line, to start the app again with after its graphics device was lost.
    relaunch_args: Vec<std::ffi::OsString>,
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
            match std::env::current_exe().and_then(|exe| std::process::Command::new(exe).args(&self.relaunch_args).spawn()) {
                Ok(_) => log::warn!("the graphics device was lost; StudioCraft starts again"),
                Err(e) => eprintln!("studiocraft: the graphics device was lost, and StudioCraft could not start again: {e}"),
            }
        }
    }
}

const HELP: &str = "StudioCraft: record and stream scenes.

Usage:
  studiocraft [--config-dir <dir> | --no-prefs] [--control <port>]
  studiocraft --screenshot <file.png>
  studiocraft --self-test <file.json>
  studiocraft --version

Options:
  --config-dir <dir>      keep settings, scene collections and profiles in <dir>
  --no-prefs              keep everything in memory; nothing is read or written
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
    let control = match args.control {
        Some(port) => Some(control::Control::start(port)?),
        None => None,
    };
    let (store, problems) = Store::open(location.clone());
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
            let mut app = StudioApp::boot(store, cc.wgpu_render_state.clone(), problems);
            // After boot, which sets the interface language from the saved settings.
            app.install_fonts(&cc.egui_ctx);
            app.set_update_checks(update_checks);
            app.set_title_bar(buttons);
            let notices = after_losses.iter().chain(picked.get().into_iter().flatten()).map(hardware::notice_msg).collect();
            app.set_window_gpu(adapter, notices, may_relaunch);
            Ok(Box::new(Shell { app, harness, control, relaunch_args: raw }))
        }),
    );
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
