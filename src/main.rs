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
use std::sync::{Arc, Mutex, OnceLock};

use craft_settings::Location;
use craft_window::restore;
use studiocraft_render::device::{Notice, Preference};
use studiocraft_ui_egui::store::Store;
use studiocraft_ui_egui::{APP_NAME, BUNDLE_ID, DesktopWindow, StudioApp};
use studiocraft_ui_egui::{hardware, placement};

use crate::args::{Action, Args};

struct Shell {
    app: StudioApp,
    harness: Option<harness::Harness>,
    control: Option<control::Control>,
    /// The command line, to start the app again with after its graphics device was lost.
    relaunch_args: Vec<std::ffi::OsString>,
    /// Where the window's size and place are kept. The UI runtime records them as the window
    /// moves and writes the file once a second; the exit writes the last change.
    placement: Arc<Mutex<restore::Store>>,
    /// The geometry the window opened with and whether it came from a saved record, until it has
    /// been checked against the monitors.
    unchecked: Option<(placement::Placement, bool)>,
    /// How many more frames the check waits for the system to list a monitor.
    monitor_waits: u32,
}

/// The frames the monitor check waits for the system to list a monitor before it gives up.
const MONITOR_WAITS: u32 = 120;

impl Shell {
    /// Once the monitors are listed: moves a window that opened off every monitor onto one, and
    /// shrinks one larger than its monitor to it. A first window on a monitor smaller than it is
    /// shrunk and maximized. A maximized window gets the corrected normal geometry and is
    /// maximized again, so it restores to a size that fits.
    fn fit_to_monitors(&mut self, ctx: &egui::Context, frame: &eframe::Frame) {
        let Some(window) = frame.winit_window() else { return };
        let Some((opened, saved)) = self.unchecked.take() else { return };
        let handles: Vec<_> = window.available_monitors().collect();
        let monitors: Vec<placement::Monitor> = handles
            .iter()
            .map(|m| {
                let (pos, size) = (m.position(), m.size());
                placement::monitor(m.name(), [pos.x, pos.y], [size.width, size.height], m.scale_factor())
            })
            .collect();
        if monitors.is_empty() {
            if self.monitor_waits > 0 {
                self.monitor_waits -= 1;
                self.unchecked = Some((opened, saved));
            }
            return;
        }
        let current = window.current_monitor();
        let home = current.and_then(|c| handles.iter().position(|m| *m == c));
        let outer_px = window.outer_position().ok().map(|p| [p.x, p.y]);
        let defaults = placement::defaults();
        let Some(next) = placement::on_monitors(&opened, saved, &monitors, home, outer_px, &defaults) else { return };
        if opened.maximized {
            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(false));
        }
        let least = placement::least_size(&next, &defaults);
        if least != defaults.min {
            ctx.send_viewport_cmd(egui::ViewportCommand::MinInnerSize(least.into()));
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(next.normal.size.into()));
        if let Some([x, y]) = next.position_px {
            let ppp = ctx.pixels_per_point().max(f32::EPSILON);
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(x as f32 / ppp, y as f32 / ppp)));
        }
        if next.maximized {
            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(true));
        }
    }
}

impl eframe::App for Shell {
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        if let Some(h) = &mut self.harness {
            h.inject(raw);
        }
        if let Some(c) = &mut self.control {
            c.inject(raw);
        }
        self.app.before_input(ctx);
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
        self.fit_to_monitors(ui.ctx(), frame);
        eframe::App::ui(&mut self.app, ui, frame);
    }

    fn on_exit(&mut self) {
        eframe::App::on_exit(&mut self.app);
        let mut placement = self.placement.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if placement.is_dirty()
            && let Err(e) = placement.save()
        {
            // The window has closed, so the error goes to the terminal, as below.
            let path = placement.path().map(|p| p.display().to_string()).unwrap_or_default();
            eprintln!("studiocraft: the window's size and place could not be saved in {path}: {e}");
        }
        drop(placement);
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
    let frame = titlebar::decide(store.prefs().system_title_bar);
    // The window opens where it was, else at the first window's size. A scripted run keeps
    // nothing, so it always opens at the first size, and is not moved after it opens.
    let window = placement::store(store.dir());
    let defaults = placement::defaults();
    let saved = placement::saved(&window);
    let opened = placement::opening(saved.as_ref(), &defaults);
    let unchecked = harness.is_none().then(|| (opened.clone(), saved.is_some()));
    let mut viewport = placement::apply(egui::ViewportBuilder::default().with_title(APP_NAME).with_app_id(BUNDLE_ID), &opened, &defaults);
    if let Some(icon) = studiocraft_ui_egui::app_icon::window_icon() {
        viewport = viewport.with_icon(icon);
    }
    viewport = titlebar::viewport(viewport, &frame.frame);
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
            let mut app = StudioApp::boot(store, cc.wgpu_render_state.clone(), problems);
            // After boot, which sets the interface language from the saved settings.
            let desktop = DesktopWindow { frame, double_click: titlebar::system_double_click(), placement: Arc::clone(&window) };
            studiocraft_ui_egui::install_runtime(&cc.egui_ctx, Some(desktop)).map_err(|e| format!("the interface could not start: {e:?}"))?;
            app.install_fonts(&cc.egui_ctx);
            app.set_update_checks(update_checks);
            let picked = picked.get().map_or(&[][..], Vec::as_slice);
            // A run in software after Automatic could not open the window says so, unless no
            // software adapter was found and the window draws on hardware after all.
            let fell_back = (software_once && !picked.contains(&Notice::NoSoftwareAdapter)).then_some(Notice::SoftwareOnly);
            let notices = after_losses.iter().chain(&fell_back).chain(picked).map(hardware::notice_msg).collect();
            app.set_window_gpu(adapter, notices, may_relaunch);
            Ok(Box::new(Shell { app, harness, control, relaunch_args: raw, placement: window, unchecked, monitor_waits: MONITOR_WAITS }))
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
