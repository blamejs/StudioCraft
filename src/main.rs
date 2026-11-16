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

use craft_settings::Location;
use studiocraft_ui_egui::store::Store;
use studiocraft_ui_egui::{APP_NAME, BUNDLE_ID, StudioApp};

use crate::args::{Action, Args};

struct Shell {
    app: StudioApp,
    harness: Option<harness::Harness>,
    control: Option<control::Control>,
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

fn run(args: Args) -> Result<(), String> {
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
    let (store, problems) = Store::open(location);
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
    let options = eframe::NativeOptions { viewport, renderer: eframe::Renderer::Wgpu, run_and_return: true, ..eframe::NativeOptions::default() };
    // Scripted runs and sessions that keep nothing never look for updates.
    let update_checks = !args.no_prefs && harness.is_none();
    eframe::run_native(
        APP_NAME,
        options,
        Box::new(move |cc| {
            studiocraft_ui_egui::theme::install(&cc.egui_ctx);
            let mut app = StudioApp::boot(store, cc.wgpu_render_state.clone(), problems);
            // After boot, which sets the interface language from the saved settings.
            app.install_fonts(&cc.egui_ctx);
            app.set_update_checks(update_checks);
            app.set_title_bar(buttons);
            Ok(Box::new(Shell { app, harness, control }))
        }),
    )
    .map_err(|e| format!("the window could not be opened: {e}"))
}

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
        raw.into_iter().map(|a| a.into_string().map_err(|a| format!("`{}` is not valid Unicode", a.to_string_lossy()))).collect();
    match unicode.and_then(args::parse) {
        Ok(Action::Version) => {
            println!("{APP_NAME} {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Ok(Action::Help) => {
            print!("{HELP}");
            ExitCode::SUCCESS
        }
        Ok(Action::Run(a)) => match run(a) {
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
