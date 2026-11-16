//! Design tokens: the colors of the dark and light palettes, the compact and comfortable size
//! sets and the type scale.
//!
//! The module depends on egui only. Color roles are named after the roles of a shared Craft
//! theme (background, surface, raised, text, text_muted, line, accent, on_accent, selection,
//! focus, good, warn, bad), with a few more for controls, meters and live outputs.

use egui::{Color32, Vec2, vec2};

/// A color role of a [`Palette`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Role {
    /// The window and the area behind the canvases.
    Background,
    /// Docks, the status bar and settings pages.
    Surface,
    /// Menu bars, menus, popups, dock headers and dialogs.
    Raised,
    /// The fill of secondary buttons and fields.
    Control,
    ControlHover,
    ControlActive,
    /// Dividers between regions. Decorative: never the only mark of a control.
    Line,
    /// Field and combo borders, unchecked boxes, unfilled slider rails.
    LineStrong,
    Text,
    /// Dock titles, field labels, shortcuts and the status bar.
    TextMuted,
    /// Placeholders and empty-state hints.
    TextHint,
    TextDisabled,
    /// Primary button fill, checked boxes, slider fill.
    Accent,
    AccentHover,
    AccentPressed,
    /// Labels and glyphs on an accent or on-air fill.
    OnAccent,
    /// Links, accent glyphs and the selected-row bar.
    AccentText,
    /// The fill of a selected row.
    Selection,
    /// The keyboard focus ring.
    Focus,
    /// The fill of the live and record pills and the program tally.
    OnAir,
    /// Red text and marks on surfaces.
    OnAirText,
    /// The record pill and dot.
    Recording,
    /// Mute, warnings and dropped frames.
    Warn,
    /// Text on a warning fill.
    OnWarn,
    /// Connected or healthy.
    Good,
    MeterGreen,
    MeterYellow,
    MeterRed,
    /// The well behind the meter bars, dark in both palettes.
    MeterTrack,
}

/// One theme's colors.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palette {
    pub dark: bool,
    pub background: Color32,
    pub surface: Color32,
    pub raised: Color32,
    pub control: Color32,
    pub control_hover: Color32,
    pub control_active: Color32,
    pub line: Color32,
    pub line_strong: Color32,
    pub text: Color32,
    pub text_muted: Color32,
    pub text_hint: Color32,
    pub text_disabled: Color32,
    pub accent: Color32,
    pub accent_hover: Color32,
    pub accent_pressed: Color32,
    pub on_accent: Color32,
    pub accent_text: Color32,
    pub selection: Color32,
    pub focus: Color32,
    pub on_air: Color32,
    pub on_air_text: Color32,
    pub recording: Color32,
    pub warn: Color32,
    pub on_warn: Color32,
    pub good: Color32,
    pub meter_green: Color32,
    pub meter_yellow: Color32,
    pub meter_red: Color32,
    pub meter_track: Color32,
    /// The opacity of window and menu shadows, 0 to 255.
    pub shadow_alpha: u8,
}

const fn hex(rgb: u32) -> Color32 {
    Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

impl Palette {
    pub const DARK: Palette = Palette {
        dark: true,
        background: hex(0x141518),
        surface: hex(0x1C1D21),
        raised: hex(0x25272C),
        control: hex(0x2E3036),
        control_hover: hex(0x383B42),
        control_active: hex(0x42454D),
        line: hex(0x3A3D44),
        line_strong: hex(0x7A7E88),
        text: hex(0xE6E7EA),
        text_muted: hex(0xABAEB5),
        text_hint: hex(0x9599A1),
        text_disabled: hex(0x5C6068),
        accent: hex(0x2F6FDB),
        accent_hover: hex(0x2A67CF),
        accent_pressed: hex(0x2357B0),
        on_accent: hex(0xFFFFFF),
        accent_text: hex(0x7FB0FF),
        selection: hex(0x24406B),
        focus: hex(0x7FB0FF),
        on_air: hex(0xD92D35),
        on_air_text: hex(0xFF6B6E),
        recording: hex(0xD92D35),
        warn: hex(0xE8A23A),
        on_warn: hex(0x1B1C1F),
        good: hex(0x3DBE7A),
        meter_green: hex(0x3DBE7A),
        meter_yellow: hex(0xE8C93A),
        meter_red: hex(0xE5484D),
        meter_track: hex(0x0F1012),
        shadow_alpha: 102,
    };

    pub const LIGHT: Palette = Palette {
        dark: false,
        background: hex(0xE8E9EC),
        surface: hex(0xF4F5F7),
        raised: hex(0xFFFFFF),
        control: hex(0xE3E5E9),
        control_hover: hex(0xD9DCE1),
        control_active: hex(0xCDD1D8),
        line: hex(0xC9CDD3),
        line_strong: hex(0x7C818A),
        text: hex(0x1B1C1F),
        text_muted: hex(0x50545C),
        text_hint: hex(0x5F636B),
        text_disabled: hex(0x8E939C),
        accent: hex(0x2563D9),
        accent_hover: hex(0x1F57C3),
        accent_pressed: hex(0x1A4CAB),
        on_accent: hex(0xFFFFFF),
        accent_text: hex(0x1F57C3),
        selection: hex(0xCFE0FF),
        focus: hex(0x2563D9),
        on_air: hex(0xC8232C),
        on_air_text: hex(0xC8232C),
        recording: hex(0xC8232C),
        warn: hex(0x8C5200),
        on_warn: hex(0xFFFFFF),
        good: hex(0x17733F),
        meter_green: hex(0x3DBE7A),
        meter_yellow: hex(0xE8C93A),
        meter_red: hex(0xE5484D),
        meter_track: hex(0x2B2D33),
        shadow_alpha: 46,
    };

    /// The color of `role`.
    pub fn color(&self, role: Role) -> Color32 {
        match role {
            Role::Background => self.background,
            Role::Surface => self.surface,
            Role::Raised => self.raised,
            Role::Control => self.control,
            Role::ControlHover => self.control_hover,
            Role::ControlActive => self.control_active,
            Role::Line => self.line,
            Role::LineStrong => self.line_strong,
            Role::Text => self.text,
            Role::TextMuted => self.text_muted,
            Role::TextHint => self.text_hint,
            Role::TextDisabled => self.text_disabled,
            Role::Accent => self.accent,
            Role::AccentHover => self.accent_hover,
            Role::AccentPressed => self.accent_pressed,
            Role::OnAccent => self.on_accent,
            Role::AccentText => self.accent_text,
            Role::Selection => self.selection,
            Role::Focus => self.focus,
            Role::OnAir => self.on_air,
            Role::OnAirText => self.on_air_text,
            Role::Recording => self.recording,
            Role::Warn => self.warn,
            Role::OnWarn => self.on_warn,
            Role::Good => self.good,
            Role::MeterGreen => self.meter_green,
            Role::MeterYellow => self.meter_yellow,
            Role::MeterRed => self.meter_red,
            Role::MeterTrack => self.meter_track,
        }
    }
}

/// The least contrast for text (WCAG 2.2 SC 1.4.3).
pub const TEXT_CONTRAST: f32 = 4.5;
/// The least contrast for state marks and focus rings (WCAG 2.2 SC 1.4.11 and 2.4.13).
pub const MARK_CONTRAST: f32 = 3.0;

/// A foreground role, the role it is drawn on, and the least contrast ratio between the two.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pair {
    pub fg: Role,
    pub bg: Role,
    pub min: f32,
}

const fn text(fg: Role, bg: Role) -> Pair {
    Pair { fg, bg, min: TEXT_CONTRAST }
}

const fn mark(fg: Role, bg: Role) -> Pair {
    Pair { fg, bg, min: MARK_CONTRAST }
}

/// Every pair of roles that the interface draws one on the other, with its threshold. Both
/// palettes meet every one.
pub const PAIRS: &[Pair] = {
    use Role::*;
    &[
        text(Text, Background),
        text(Text, Surface),
        text(Text, Raised),
        text(Text, Control),
        text(Text, ControlHover),
        text(Text, Selection),
        text(TextMuted, Surface),
        text(TextMuted, Raised),
        text(TextMuted, Control),
        text(TextMuted, ControlHover),
        text(TextMuted, Selection),
        text(TextHint, Surface),
        text(TextHint, Raised),
        text(TextHint, Control),
        text(OnAccent, Accent),
        text(OnAccent, AccentHover),
        text(OnAccent, AccentPressed),
        text(AccentText, Surface),
        text(AccentText, Raised),
        text(AccentText, Control),
        text(AccentText, Selection),
        text(OnAirText, Surface),
        text(OnAirText, Raised),
        text(OnAccent, OnAir),
        text(Warn, Surface),
        text(Warn, Raised),
        text(Warn, Control),
        text(Warn, Selection),
        text(OnWarn, Warn),
        text(Good, Surface),
        text(Good, Raised),
        mark(Focus, Background),
        mark(Focus, Surface),
        mark(Focus, Raised),
        mark(Focus, Control),
        mark(LineStrong, Background),
        mark(LineStrong, Surface),
        mark(LineStrong, Raised),
        mark(LineStrong, Control),
        mark(Accent, Surface),
        mark(Accent, Raised),
        mark(OnAir, Background),
        mark(OnAir, Surface),
        mark(OnAir, Raised),
        mark(MeterGreen, MeterTrack),
        mark(MeterYellow, MeterTrack),
        mark(MeterRed, MeterTrack),
    ]
};

/// The relative luminance of an opaque sRGB color, as WCAG 2.2 defines it.
pub fn relative_luminance(c: Color32) -> f32 {
    let channel = |v: u8| {
        let s = f32::from(v) / 255.0;
        if s <= 0.040_45 { s / 12.92 } else { ((s + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * channel(c.r()) + 0.7152 * channel(c.g()) + 0.0722 * channel(c.b())
}

/// The WCAG contrast ratio of two opaque colors, from 1 to 21, in either order.
pub fn contrast(a: Color32, b: Color32) -> f32 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// The pairs of `palette` below their threshold, with the ratio each has.
pub fn contrast_failures(palette: &Palette) -> Vec<(Pair, f32)> {
    PAIRS.iter().map(|p| (*p, contrast(palette.color(p.fg), palette.color(p.bg)))).filter(|(p, ratio)| *ratio < p.min).collect()
}

/// Spacing, control sizes and corner radii, in points.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sizes {
    /// Space between neighboring widgets.
    pub item_spacing: Vec2,
    /// Space between a button's edge and its label.
    pub button_padding: Vec2,
    /// The smallest a control is: every pointer target is at least this size.
    pub interact_size: Vec2,
    /// An inline icon button.
    pub control_sm: Vec2,
    /// The height of buttons, combo boxes and fields.
    pub control_md: f32,
    /// The height of the Go Live and Record buttons.
    pub control_lg: f32,
    /// The height of a list or menu row.
    pub row: f32,
    /// The height of the menu and output bar.
    pub bar_menu: f32,
    /// The height of the status bar.
    pub bar_status: f32,
    /// The height of a dock header.
    pub panel_header: f32,
    /// The padding inside a dock.
    pub pad_panel: Vec2,
    pub field_md: f32,
    pub field_sm: f32,
    pub field_number: f32,
    /// The narrowest a menu is; a menu grows with its content.
    pub menu_min_width: f32,
    pub radius_control: u8,
    pub radius_surface: u8,
    pub radius_window: u8,
    pub margin_menu: i8,
    pub margin_window: i8,
    /// The checkbox and radio box: outer size, inner size, and the space before the label.
    pub icon_width: f32,
    pub icon_width_inner: f32,
    pub icon_spacing: f32,
    /// The width of the keyboard focus ring and its gap to the control.
    pub focus_width: f32,
    pub focus_gap: f32,
}

impl Sizes {
    /// The default density.
    pub const COMPACT: Sizes = Sizes {
        item_spacing: vec2(6.0, 4.0),
        button_padding: vec2(8.0, 4.0),
        interact_size: vec2(24.0, 24.0),
        control_sm: vec2(24.0, 24.0),
        control_md: 24.0,
        control_lg: 28.0,
        row: 24.0,
        bar_menu: 32.0,
        bar_status: 22.0,
        panel_header: 24.0,
        pad_panel: vec2(6.0, 4.0),
        field_md: 192.0,
        field_sm: 160.0,
        field_number: 72.0,
        menu_min_width: 160.0,
        radius_control: 4,
        radius_surface: 4,
        radius_window: 6,
        margin_menu: 4,
        margin_window: 12,
        icon_width: 16.0,
        icon_width_inner: 8.0,
        icon_spacing: 6.0,
        focus_width: 2.0,
        focus_gap: 2.0,
    };

    /// Four points more on every control height and row, for larger targets.
    pub const COMFORTABLE: Sizes = Sizes {
        item_spacing: vec2(8.0, 6.0),
        button_padding: vec2(10.0, 6.0),
        interact_size: vec2(28.0, 28.0),
        control_sm: vec2(28.0, 28.0),
        control_md: 28.0,
        control_lg: 32.0,
        row: 28.0,
        bar_menu: 36.0,
        bar_status: 24.0,
        panel_header: 28.0,
        pad_panel: vec2(8.0, 6.0),
        field_md: 224.0,
        field_sm: 176.0,
        field_number: 80.0,
        menu_min_width: 180.0,
        radius_control: 4,
        radius_surface: 6,
        radius_window: 8,
        margin_menu: 6,
        margin_window: 16,
        icon_width: 16.0,
        icon_width_inner: 8.0,
        icon_spacing: 6.0,
        focus_width: 2.0,
        focus_gap: 2.0,
    };
}

/// Text sizes in points.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TypeScale {
    /// List rows, fields and dialog text.
    pub body: f32,
    /// Buttons and menu items.
    pub button: f32,
    /// Field labels and menu shortcuts.
    pub label: f32,
    /// The status bar, captions, chips and meter scales. No text is smaller.
    pub small: f32,
    /// Dock and section titles.
    pub title: f32,
    /// Dialog and settings page titles.
    pub heading: f32,
    /// dB readouts, stats and encoder names.
    pub monospace: f32,
    /// The live and recording timers.
    pub timer: f32,
}

impl TypeScale {
    pub const DEFAULT: TypeScale =
        TypeScale { body: 12.0, button: 12.0, label: 11.5, small: 11.0, title: 12.0, heading: 13.0, monospace: 11.5, timer: 12.0 };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_pair_meets_its_contrast_threshold_in_both_palettes() {
        for (name, palette) in [("dark", Palette::DARK), ("light", Palette::LIGHT)] {
            let failures = contrast_failures(&palette);
            assert!(failures.is_empty(), "{name} palette pairs below their threshold: {failures:?}");
        }
    }

    #[test]
    fn the_pair_list_covers_text_and_marks() {
        assert_eq!(PAIRS.len(), 47);
        assert_eq!(PAIRS.iter().filter(|p| p.min == TEXT_CONTRAST).count(), 31);
        assert_eq!(PAIRS.iter().filter(|p| p.min == MARK_CONTRAST).count(), 16);
    }

    #[test]
    fn contrast_matches_known_ratios() {
        assert!((contrast(Color32::BLACK, Color32::WHITE) - 21.0).abs() < 1e-3);
        assert!((contrast(Color32::WHITE, Color32::WHITE) - 1.0).abs() < 1e-6);
        // The order of the two colors does not matter.
        let (a, b) = (Palette::DARK.text, Palette::DARK.surface);
        assert_eq!(contrast(a, b), contrast(b, a));
        // `text` on `surface`, dark: 13.6:1 by the formula.
        assert!((contrast(a, b) - 13.6).abs() < 0.1, "{}", contrast(a, b));
    }

    #[test]
    fn comfortable_is_four_points_larger_than_compact() {
        let (c, f) = (Sizes::COMPACT, Sizes::COMFORTABLE);
        assert_eq!(f.interact_size - c.interact_size, vec2(4.0, 4.0));
        assert_eq!(f.control_md - c.control_md, 4.0);
        assert_eq!(f.row - c.row, 4.0);
        // Every target is at least 24 x 24.
        for s in [c, f] {
            assert!(s.interact_size.x >= 24.0 && s.interact_size.y >= 24.0);
            assert!(s.control_sm.min_elem() >= 24.0);
        }
    }

    #[test]
    fn no_text_is_smaller_than_eleven_points() {
        let t = TypeScale::DEFAULT;
        for size in [t.body, t.button, t.label, t.small, t.title, t.heading, t.monospace, t.timer] {
            assert!(size >= 11.0, "{size}");
        }
    }
}
