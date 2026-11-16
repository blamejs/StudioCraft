//! Who draws the window frame.
//!
//! The shared `craft-window` crate decides: the desktop draws the frame on macOS, Windows, X11 and
//! any Wayland compositor that decorates windows itself, and the app draws it where the compositor
//! leaves that to the app (GNOME on Wayland). `STUDIOCRAFT_TITLEBAR=app` or `=system` overrides
//! the probe. StudioCraft draws its own title bar only when `craft-window` gives the frame to the
//! app and Settings › General › "Use system title bar" is off.

use craft_window::{Choice, Frame, Layout, Prefix};

/// The override variable is `STUDIOCRAFT_TITLEBAR`.
pub const PREFIX: Prefix = Prefix::new("STUDIOCRAFT");

/// Whether StudioCraft draws its own title bar, given the frame `craft-window` decided and the
/// "Use system title bar" setting.
pub fn custom_titlebar_from(frame: &Frame, system_title_bar: bool) -> bool {
    matches!(frame, Frame::App(_)) && !system_title_bar
}

/// The window buttons of the title bar StudioCraft draws, in the places the desktop puts them, or
/// `None` when the desktop draws the frame.
pub fn buttons_from(frame: &Frame, system_title_bar: bool) -> Option<Layout> {
    if custom_titlebar_from(frame, system_title_bar) { frame.layout().cloned() } else { None }
}

/// Probes the desktop once and returns the window buttons of the title bar StudioCraft draws, or
/// `None` when the desktop draws the frame. Call before the window opens.
pub fn decide(system_title_bar: bool) -> Option<Layout> {
    buttons_from(Choice::probe(PREFIX).frame(), system_title_bar)
}

/// The window's decorations: on unless StudioCraft draws its own title bar.
pub fn viewport(builder: egui::ViewportBuilder, buttons: Option<&Layout>) -> egui::ViewportBuilder {
    builder.with_decorations(buttons.is_none())
}

#[cfg(test)]
mod tests {
    use super::*;
    use craft_window::Button;

    #[test]
    fn the_app_draws_the_title_bar_only_when_the_frame_is_its_own_and_the_setting_is_off() {
        let app = Frame::App(Layout { left: vec![Button::Close], right: vec![Button::Minimize, Button::Maximize] });
        assert!(!custom_titlebar_from(&Frame::Desktop, false), "the desktop's frame");
        assert!(!custom_titlebar_from(&Frame::Desktop, true));
        assert!(custom_titlebar_from(&app, false), "the app's frame");
        assert!(!custom_titlebar_from(&app, true), "Use system title bar keeps the desktop's frame");

        assert_eq!(buttons_from(&Frame::Desktop, false), None);
        assert_eq!(buttons_from(&app, true), None);
        assert_eq!(buttons_from(&app, false), app.layout().cloned(), "the buttons go where the desktop puts them");
    }

    #[test]
    fn the_window_keeps_its_decorations_unless_the_app_draws_the_frame() {
        let none = viewport(egui::ViewportBuilder::default(), None);
        assert_eq!(none.decorations, Some(true));
        let own = viewport(egui::ViewportBuilder::default(), Some(&Layout::default()));
        assert_eq!(own.decorations, Some(false));
        assert_eq!(PREFIX.var_name(), "STUDIOCRAFT_TITLEBAR");
    }
}
