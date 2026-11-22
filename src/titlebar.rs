//! Who draws the window frame.
//!
//! The shared `craft-window` policy decides from the platform, the desktop and Settings › General ›
//! "Use system title bar": StudioCraft's own bar is the title bar on Windows, on Linux desktops
//! that float their windows and, with the traffic lights over it, on macOS. Tiling window managers,
//! the web build and the setting keep the system's title bar. `STUDIOCRAFT_TITLEBAR=app` or
//! `=system` overrides the policy, and `STUDIOCRAFT_CAPTION_STYLE` and
//! `STUDIOCRAFT_CAPTION_LAYOUT` pick the look and order of the window buttons.

use craft_host::target::DoubleClick;
use craft_window::Prefix;
use craft_window::policy::{self, Backend, Decision, Frame, TitleBarPref};

/// The override variables are `STUDIOCRAFT_TITLEBAR`, `STUDIOCRAFT_CAPTION_STYLE` and
/// `STUDIOCRAFT_CAPTION_LAYOUT`.
pub const PREFIX: Prefix = Prefix::new("STUDIOCRAFT");

/// The policy's title bar preference for the "Use system title bar" setting.
pub fn pref(system_title_bar: bool) -> TitleBarPref {
    if system_title_bar { TitleBarPref::System } else { TitleBarPref::Integrated }
}

/// Probes the desktop once and decides who draws the frame of the main window. Call before the
/// window opens.
pub fn decide(system_title_bar: bool) -> Decision {
    policy::decide_for(PREFIX, Backend::from_env(), pref(system_title_bar))
}

/// The window's decorations for `frame`: off where StudioCraft's bar is the title bar, and on
/// macOS a full-size content view under a hidden title.
pub fn viewport(builder: egui::ViewportBuilder, frame: &Frame) -> egui::ViewportBuilder {
    policy::apply(builder, frame)
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use craft_window::policy::{Desktop, DesktopKind, Os, Runtime, WindowKind};

    /// The frame the policy gives the main window on `os` with the setting `system_title_bar`.
    fn frame_on(os: Os, kind: DesktopKind, system_title_bar: bool) -> Frame {
        let desktop = Desktop { kind, ssd: None, gtk_theme: None, icon_theme: None };
        policy::decide(os, Runtime::Native, Backend::Wayland, &desktop, WindowKind::Main, pref(system_title_bar), None).frame
    }

    #[test]
    fn use_system_title_bar_keeps_the_systems_frame_and_otherwise_the_bar_is_the_title_bar() {
        assert_eq!(pref(false), TitleBarPref::Integrated);
        assert_eq!(pref(true), TitleBarPref::System);
        for os in [Os::Windows, Os::Linux, Os::Mac] {
            assert_eq!(frame_on(os, DesktopKind::Gnome, true), Frame::Native, "{os:?} with the setting on");
        }
        assert!(matches!(frame_on(Os::Windows, DesktopKind::Other, false), Frame::Integrated(_)), "Windows");
        assert!(matches!(frame_on(Os::Linux, DesktopKind::Gnome, false), Frame::Integrated(_)), "GNOME");
        assert!(matches!(frame_on(Os::Mac, DesktopKind::Other, false), Frame::Inset(_)), "macOS keeps its traffic lights");
        assert_eq!(frame_on(Os::Linux, DesktopKind::Tiling, false), Frame::Native, "a tiling window manager");
    }

    #[test]
    fn the_window_keeps_its_decorations_unless_the_app_draws_the_frame() {
        let native = viewport(egui::ViewportBuilder::default(), &Frame::Native);
        assert_eq!(native.decorations, Some(true));
        let integrated = frame_on(Os::Windows, DesktopKind::Other, false);
        assert_eq!(viewport(egui::ViewportBuilder::default(), &integrated).decorations, Some(false));
        assert_eq!(PREFIX.var_name(), "STUDIOCRAFT_TITLEBAR");
    }

    #[test]
    fn the_system_setting_reads_as_defaults_prints_it() {
        assert_eq!(parse_double_click("Minimize\n"), DoubleClick::Minimize);
        assert_eq!(parse_double_click("None\n"), DoubleClick::Nothing);
        assert_eq!(parse_double_click("Maximize\n"), DoubleClick::Zoom);
        assert_eq!(parse_double_click("Fill\n"), DoubleClick::Zoom);
        assert_eq!(parse_double_click(""), DoubleClick::Zoom);
    }
}
