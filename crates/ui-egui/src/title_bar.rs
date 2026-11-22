//! The menu strip as the window's title bar, when StudioCraft draws the window frame.
//!
//! `craft-window` decides at start who draws the frame. When the app draws it, the window has no
//! decorations: the empty part of the menu strip moves the window and a double-click on it does
//! what the system's title bar setting says (maximize where there is no such setting), the window
//! buttons sit at the ends of the strip in the order the desktop names, and the window edges resize
//! the window.

use craft_host::target::DoubleClick;
use craft_window::{Button, Layout};
use egui::{Color32, CursorIcon, Id, LayerId, Order, PointerButton, Rect, ResizeDirection, Sense, Ui, ViewportCommand, pos2, vec2};

use crate::icons::{self, Icon};

/// The size of one window button.
const BUTTON: egui::Vec2 = vec2(40.0, 22.0);
/// How far into the window an edge resizes it.
const EDGE: f32 = 5.0;
/// How far along each edge a corner reaches.
const CORNER: f32 = 12.0;

/// What a window button asks of the window.
pub fn command(button: Button, maximized: bool) -> ViewportCommand {
    match button {
        Button::Minimize => ViewportCommand::Minimized(true),
        Button::Maximize => ViewportCommand::Maximized(!maximized),
        _ => ViewportCommand::Close,
    }
}

/// The icon and the accessible name of a window button.
fn face(button: Button, maximized: bool) -> (Icon, String) {
    match button {
        Button::Minimize => (Icon::Minimize, tl!("Minimize").to_string()),
        Button::Maximize if maximized => (Icon::Restore, tl!("Restore").to_string()),
        Button::Maximize => (Icon::Maximize, tl!("Maximize").to_string()),
        _ => (Icon::Close, tl!("Close").to_string()),
    }
}

/// Draws `buttons` in the order given, in the current layout direction, and sends the command of
/// the one clicked.
fn buttons(ui: &mut Ui, buttons: &[Button]) {
    let maximized = ui.input(|i| i.viewport().maximized).unwrap_or(false);
    for b in buttons {
        let (icon, name) = face(*b, maximized);
        if icons::sized(ui, icon, &name, true, BUTTON, Color32::TRANSPARENT) {
            ui.ctx().send_viewport_cmd(command(*b, maximized));
        }
    }
}

/// The width a row of `count` window buttons takes with `spacing` between them.
fn width(count: usize, spacing: f32) -> f32 {
    if count == 0 { 0.0 } else { count as f32 * BUTTON.x + (count - 1) as f32 * spacing + spacing }
}

/// The menu strip as the title bar: the window buttons `layout` puts on the left, the menus
/// (`menus` gets the width they may take), then the buttons on the right. The empty part of the
/// strip moves and maximizes the window. Everything is centered in the strip's height.
pub fn strip<R>(ui: &mut Ui, layout: &Layout, menus: impl FnOnce(&mut Ui, f32) -> R) -> R {
    empty_area(ui, ui.max_rect());
    ui.allocate_ui_with_layout(ui.available_size(), egui::Layout::left_to_right(egui::Align::Center), |ui| {
        buttons(ui, &layout.left);
        let spacing = ui.spacing().item_spacing.x;
        let room = (ui.available_width() - width(layout.right.len(), spacing)).max(0.0);
        let r = menus(ui, room);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let right: Vec<Button> = layout.right.iter().rev().copied().collect();
            buttons(ui, &right);
        });
        r
    })
    .inner
}

// port: P5 (title bar and window chrome) replaces this with the framework's title bar, which
// moves the window from the strip's empty area.

/// The system's setting for a double-click on a title bar: on macOS the global default
/// `AppleActionOnDoubleClick` (System Settings > Desktop & Dock), elsewhere, or when the setting is
/// absent or unreadable, maximize.
pub fn system_double_click() -> DoubleClick {
    #[cfg(target_os = "macos")]
    {
        match std::process::Command::new("/usr/bin/defaults").args(["read", "-g", "AppleActionOnDoubleClick"]).output() {
            Ok(out) if out.status.success() => parse_double_click(&String::from_utf8_lossy(&out.stdout)),
            _ => DoubleClick::Zoom,
        }
    }
    #[cfg(not(target_os = "macos"))]
    DoubleClick::Zoom
}

/// The value of `AppleActionOnDoubleClick` as `defaults read` prints it. "Minimize" minimizes and
/// "None" does nothing; any other value maximizes, as the default "Maximize" and the "Fill" of
/// macOS 15 do.
#[cfg(any(target_os = "macos", test))]
fn parse_double_click(value: &str) -> DoubleClick {
    match value.trim() {
        "Minimize" => DoubleClick::Minimize,
        "None" => DoubleClick::Nothing,
        _ => DoubleClick::Zoom,
    }
}

/// What a double-click on the empty strip asks of the window under the system setting `setting`.
fn double_click_command(setting: DoubleClick, maximized: bool) -> Option<ViewportCommand> {
    match setting {
        DoubleClick::Minimize => Some(ViewportCommand::Minimized(true)),
        DoubleClick::Nothing => None,
        _ => Some(ViewportCommand::Maximized(!maximized)),
    }
}

/// Makes the empty part of `rect` move the window on a drag and act on a double-click as the
/// shell's double-click setting says. Call it before drawing the strip's controls: they sit on top
/// and take their own clicks. The area senses clicks only, so a drag is a press on the empty area
/// that has stopped being a click.
fn empty_area(ui: &mut Ui, rect: Rect) {
    let resp = ui.interact(rect, ui.scope_id().with("title_bar_empty"), Sense::CLICK);
    let armed = resp.id.with("drag_press");
    if ui.input(|i| i.viewport().fullscreen).unwrap_or(false) {
        ui.data_mut(|d| d.remove::<Option<f64>>(armed));
        return;
    }
    if resp.double_clicked() {
        let maximized = ui.input(|i| i.viewport().maximized).unwrap_or(false);
        let setting = craft_ui::runtime::env::Env::of(ui).view().shell().double_click;
        if let Some(cmd) = double_click_command(setting, maximized) {
            ui.ctx().send_viewport_cmd(cmd);
        }
    }
    let press = ui.input(|i| i.pointer.press_start_time());
    if resp.is_pointer_button_down_on() {
        ui.data_mut(|d| d.insert_temp(armed, press));
    }
    if press.is_some() && ui.input(|i| i.pointer.is_decidedly_dragging()) && ui.data(|d| d.get_temp::<Option<f64>>(armed)) == Some(press) {
        ui.data_mut(|d| d.remove::<Option<f64>>(armed));
        ui.ctx().send_viewport_cmd(ViewportCommand::StartDrag);
    }
}

/// The window's edges and corners that resize it: each area, the direction it resizes in and its
/// pointer.
pub fn zones(window: Rect) -> [(Rect, ResizeDirection, CursorIcon); 8] {
    let (l, r, t, b) = (window.left(), window.right(), window.top(), window.bottom());
    let rect = |x0: f32, y0: f32, x1: f32, y1: f32| Rect::from_min_max(pos2(x0, y0), pos2(x1.max(x0), y1.max(y0)));
    [
        (rect(l, t, l + CORNER, t + CORNER), ResizeDirection::NorthWest, CursorIcon::ResizeNorthWest),
        (rect(r - CORNER, t, r, t + CORNER), ResizeDirection::NorthEast, CursorIcon::ResizeNorthEast),
        (rect(l, b - CORNER, l + CORNER, b), ResizeDirection::SouthWest, CursorIcon::ResizeSouthWest),
        (rect(r - CORNER, b - CORNER, r, b), ResizeDirection::SouthEast, CursorIcon::ResizeSouthEast),
        (rect(l + CORNER, t, r - CORNER, t + EDGE), ResizeDirection::North, CursorIcon::ResizeNorth),
        (rect(l + CORNER, b - EDGE, r - CORNER, b), ResizeDirection::South, CursorIcon::ResizeSouth),
        (rect(l, t + CORNER, l + EDGE, b - CORNER), ResizeDirection::West, CursorIcon::ResizeWest),
        (rect(r - EDGE, t + CORNER, r, b - CORNER), ResizeDirection::East, CursorIcon::ResizeEast),
    ]
}

/// Makes the window's edges resize it. Call once per frame after the panels, so the zones lie on
/// top. A maximized or full-screen window has no zones.
pub fn resize_zones(ui: &mut Ui) {
    let ctx = ui.ctx().clone();
    let (maximized, fullscreen) = ctx.input(|i| (i.viewport().maximized.unwrap_or(false), i.viewport().fullscreen.unwrap_or(false)));
    if maximized || fullscreen {
        return;
    }
    let window = ctx.content_rect();
    let layer = LayerId::new(Order::Foreground, Id::unique("window-resize"));
    let zui = ui.new_child(egui::UiBuilder::new().layer_id(layer).max_rect(window));
    for (i, (rect, dir, cursor)) in zones(window).into_iter().enumerate() {
        let resp = zui.interact(rect, Id::unique(("window-resize", i)), Sense::DRAG).on_hover_cursor(cursor);
        if resp.drag_started_by(PointerButton::Primary) {
            ctx.send_viewport_cmd(ViewportCommand::BeginResize(dir));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_window_button_sends_its_command() {
        assert_eq!(command(Button::Minimize, false), ViewportCommand::Minimized(true));
        assert_eq!(command(Button::Maximize, false), ViewportCommand::Maximized(true));
        assert_eq!(command(Button::Maximize, true), ViewportCommand::Maximized(false));
        assert_eq!(command(Button::Close, false), ViewportCommand::Close);
        assert_eq!(face(Button::Maximize, false).0, Icon::Maximize);
        assert_eq!(face(Button::Maximize, true).0, Icon::Restore);
    }

    #[test]
    fn the_resize_zones_cover_every_edge_and_stay_in_the_window() {
        let window = Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 600.0));
        let zones = zones(window);
        let directions = [
            ResizeDirection::North,
            ResizeDirection::South,
            ResizeDirection::East,
            ResizeDirection::West,
            ResizeDirection::NorthEast,
            ResizeDirection::NorthWest,
            ResizeDirection::SouthEast,
            ResizeDirection::SouthWest,
        ];
        for d in directions {
            assert_eq!(zones.iter().filter(|z| z.1 == d).count(), 1, "{d:?}");
        }
        for (r, ..) in zones {
            assert!(window.contains_rect(r), "{r:?}");
        }
        // A window smaller than two corners gets no inverted areas.
        let tiny = Rect::from_min_size(pos2(0.0, 0.0), vec2(10.0, 10.0));
        for (r, ..) in super::zones(tiny) {
            assert!(r.width() >= 0.0 && r.height() >= 0.0, "{r:?}");
        }
    }

    #[test]
    fn the_strip_puts_the_buttons_where_the_layout_says() {
        let ctx = egui::Context::default();
        let left = Layout { left: vec![Button::Close], right: vec![Button::Minimize, Button::Maximize] };
        let (mut room, mut spacing) = (0.0, 0.0);
        let raw = egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(600.0, 30.0))), ..Default::default() };
        let mut out = ctx.run_ui(raw, |ui| {
            spacing = ui.spacing().item_spacing.x;
            room = strip(ui, &left, |_, room| room);
        });
        out.textures_delta.clear();
        assert!(room > 0.0 && room < 600.0 - BUTTON.x - width(2, spacing) + 1.0, "{room}");
        assert_eq!(width(0, spacing), 0.0);
    }

    #[test]
    fn the_system_setting_reads_as_defaults_prints_it() {
        assert_eq!(parse_double_click("Minimize\n"), DoubleClick::Minimize);
        assert_eq!(parse_double_click("None\n"), DoubleClick::Nothing);
        assert_eq!(parse_double_click("Maximize\n"), DoubleClick::Zoom);
        assert_eq!(parse_double_click("Fill\n"), DoubleClick::Zoom);
        assert_eq!(parse_double_click(""), DoubleClick::Zoom);
    }

    /// The window commands a double-click on the empty strip sends, with the shell's double-click
    /// setting `setting`.
    fn double_click_commands(setting: DoubleClick) -> Vec<ViewportCommand> {
        use craft_host::target::ShellInfo;
        use craft_ui::InstallSpec;
        use craft_ui::runtime::env::RootEnv;
        use craft_ui::runtime::frame::FrameInput;
        use craft_window::system::NoWindows;

        let ctx = egui::Context::default();
        if let Err(e) = crate::install_spec(&ctx, InstallSpec::new(ShellInfo::default().double_click(setting), std::sync::Arc::new(NoWindows))) {
            panic!("the UI runtime did not install: {e:?}");
        }
        let layout = Layout { left: Vec::new(), right: Vec::new() };
        let at = pos2(100.0, 15.0);
        let press = |pressed| egui::Event::PointerButton { pos: at, button: PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
        let frames = [vec![egui::Event::PointerMoved(at)], vec![press(true)], vec![press(false)], vec![press(true)], vec![press(false)], Vec::new()];
        let mut commands = Vec::new();
        for (n, events) in frames.into_iter().enumerate() {
            craft_ui::shell::embed::before_input(&ctx, |_| FrameInput::new(RootEnv::new()));
            let raw = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(600.0, 30.0))),
                time: Some(n as f64 * 0.05),
                events,
                ..Default::default()
            };
            let mut out = ctx.run_ui(raw, |ui| strip(ui, &layout, |_, _| ()));
            out.textures_delta.clear();
            if let Some(v) = out.viewport_output.remove(&egui::ViewportId::ROOT) {
                commands.extend(v.commands);
            }
        }
        commands
    }

    #[test]
    fn a_double_click_on_the_empty_strip_follows_the_system_setting() {
        let maximize = |c: &ViewportCommand| matches!(c, ViewportCommand::Maximized(_));
        let minimize = |c: &ViewportCommand| matches!(c, ViewportCommand::Minimized(_));

        let zoom = double_click_commands(DoubleClick::Zoom);
        assert!(zoom.contains(&ViewportCommand::Maximized(true)), "{zoom:?}");
        assert!(!zoom.iter().any(minimize), "{zoom:?}");

        let min = double_click_commands(DoubleClick::Minimize);
        assert!(min.contains(&ViewportCommand::Minimized(true)), "{min:?}");
        assert!(!min.iter().any(maximize), "{min:?}");

        let nothing = double_click_commands(DoubleClick::Nothing);
        assert!(!nothing.iter().any(|c| maximize(c) || minimize(c)), "{nothing:?}");
    }
}
