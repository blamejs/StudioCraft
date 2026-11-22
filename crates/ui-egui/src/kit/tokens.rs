//! The colors and sizes the window's own painters use where the suite theme has no role: the
//! audio level meters, the marks on the preview (snap guides and the outline of a locked item),
//! the loudness target band, the cover behind modal windows, and the widths of the window's
//! columns, in dark and light mode and in both size sets.
//!
//! The suite theme has no meter, guide or locked colors, so the window keeps its own as data
//! colors.

use craft_theme::{Mode, Size};
use craft_ui::style::DataColor;
use egui::Color32;

/// One mode's meter colors.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MeterColors {
    /// The level below the yellow zone.
    pub green: DataColor,
    pub yellow: DataColor,
    pub red: DataColor,
    /// The peak hold mark.
    pub hold: DataColor,
    /// The well behind the bars, dark in both modes.
    pub track: DataColor,
    /// Every zone of a muted channel's bars.
    pub muted: DataColor,
    /// The mark at the average (RMS) level.
    pub rms: DataColor,
}

const fn hex(rgb: u32) -> DataColor {
    DataColor::new(Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8))
}

/// `rgb` at opacity `alpha` (0 to 255), premultiplied as egui paints it.
const fn hex_alpha(rgb: u32, alpha: u8) -> DataColor {
    const fn times(channel: u32, alpha: u8) -> u8 {
        (((channel & 0xFF) * alpha as u32 + 127) / 255) as u8
    }
    DataColor::new(Color32::from_rgba_premultiplied(times(rgb >> 16, alpha), times(rgb >> 8, alpha), times(rgb, alpha), alpha))
}

impl MeterColors {
    pub const DARK: MeterColors = MeterColors {
        green: hex(0x3DBE7A),
        yellow: hex(0xE8C93A),
        red: hex(0xE5484D),
        hold: hex(0xFFFFFF),
        track: hex(0x0F1012),
        muted: hex(0x6E6E6E),
        rms: hex_alpha(0x000000, 140),
    };

    pub const LIGHT: MeterColors = MeterColors {
        green: hex(0x3DBE7A),
        yellow: hex(0xE8C93A),
        red: hex(0xE5484D),
        hold: hex(0xFFFFFF),
        track: hex(0x2B2D33),
        muted: hex(0x6E6E6E),
        rms: hex_alpha(0x000000, 140),
    };

    /// The colors for `mode`.
    pub fn of(mode: Mode) -> &'static MeterColors {
        match mode {
            Mode::Light => &MeterColors::LIGHT,
            Mode::Dark => &MeterColors::DARK,
        }
    }
}

/// One mode's colors of the marks the window draws over the canvas and behind its modal windows.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MarkColors {
    /// The lines a drag snapped to.
    pub guide: DataColor,
    /// The dashed outline of a locked item.
    pub locked: DataColor,
    /// The opacity of the loudness target band, from 0 to 1. [`MarkColors::meter_target`] gives the
    /// band's color.
    pub meter_target_opacity: f32,
    /// The cover behind modal windows and the command palette.
    pub scrim: DataColor,
}

impl MarkColors {
    pub const DARK: MarkColors =
        MarkColors { guide: hex(0xFF40A0), locked: hex(0xABAEB5), meter_target_opacity: 0.38, scrim: hex_alpha(0x050608, 158) };

    pub const LIGHT: MarkColors =
        MarkColors { guide: hex(0xC2187A), locked: hex(0x50545C), meter_target_opacity: 0.30, scrim: hex_alpha(0x1E2024, 115) };

    /// The colors for `mode`.
    pub fn of(mode: Mode) -> &'static MarkColors {
        match mode {
            Mode::Light => &MarkColors::LIGHT,
            Mode::Dark => &MarkColors::DARK,
        }
    }

    /// The loudness target band: the theme's accent text color `accent_text` at the band's
    /// opacity.
    pub fn meter_target(&self, accent_text: Color32) -> DataColor {
        let opacity = if self.meter_target_opacity.is_finite() { self.meter_target_opacity.clamp(0.0, 1.0) } else { 0.0 };
        DataColor::new(accent_text.gamma_multiply(opacity))
    }
}

/// The widths of the window's columns and of its narrowest pieces, in points.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LayoutSizes {
    /// The left column of the Set up layout.
    pub column_left: f32,
    /// The inspector, the right column of the Set up layout.
    pub inspector: f32,
    /// The health rail of the Live layout.
    pub rail: f32,
    /// The left column of the Live layout.
    pub preview_column: f32,
    /// One vertical audio channel.
    pub strip_width: f32,
    /// The smallest scene tile, in the narrow scene bar.
    pub tile_min: f32,
    /// The command palette.
    pub palette_width: f32,
}

impl LayoutSizes {
    pub const COMPACT: LayoutSizes = LayoutSizes {
        column_left: 256.0,
        inspector: 296.0,
        rail: 292.0,
        preview_column: 268.0,
        strip_width: 78.0,
        tile_min: 72.0,
        palette_width: 540.0,
    };

    /// The Compact widths times 1.25, rounded to whole points: the Comfortable size set's control
    /// height (30) over the Compact one (24).
    pub const COMFORTABLE: LayoutSizes = LayoutSizes {
        column_left: 320.0,
        inspector: 370.0,
        rail: 365.0,
        preview_column: 335.0,
        strip_width: 98.0,
        tile_min: 90.0,
        palette_width: 675.0,
    };

    /// The widths for the size set `size`.
    pub fn of(size: Size) -> &'static LayoutSizes {
        match size {
            Size::Compact => &LayoutSizes::COMPACT,
            Size::Comfortable => &LayoutSizes::COMFORTABLE,
        }
    }

    /// Every width with its name.
    pub fn all(&self) -> [(&'static str, f32); 7] {
        [
            ("column_left", self.column_left),
            ("inspector", self.inspector),
            ("rail", self.rail),
            ("preview_column", self.preview_column),
            ("strip_width", self.strip_width),
            ("tile_min", self.tile_min),
            ("palette_width", self.palette_width),
        ]
    }
}

/// `color` as egui paints it.
pub fn paint(color: DataColor) -> Color32 {
    color.0
}

#[cfg(test)]
mod tests {
    use craft_theme::{Rgba, contrast_ratio};
    use craft_ui::runtime::env::Env;
    use craft_ui::runtime::frame::FrameInput;

    use super::*;

    fn rgba(color: DataColor) -> Rgba {
        let c = paint(color);
        Rgba([c.r(), c.g(), c.b(), c.a()])
    }

    /// The meter and mark colors a frame in suite mode `mode` draws with, and the window
    /// background of that frame's theme.
    fn shown_in(mode: Mode) -> (MeterColors, MarkColors, Rgba) {
        let ctx = crate::test_context();
        craft_ui::shell::embed::before_input(&ctx, |_| FrameInput::new(crate::theme::root_env().mode(mode)));
        let mut shown = None;
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            let mode = Env::of(ui).view().mode();
            let background = crate::theme::with_root(ui.ctx(), |cx| cx.tokens.color.surface.background);
            shown = Some((*MeterColors::of(mode), *MarkColors::of(mode), background));
        });
        out.textures_delta.clear();
        let Some(shown) = shown else { panic!("the frame did not run") };
        shown
    }

    /// Every color that must stand out from what it is drawn on, by at least 3:1, with its name
    /// and the color under it: the meter's marks on the meter's track, and the guide and the
    /// locked outline on the window background.
    fn contrast_pairs(m: &MeterColors, k: &MarkColors, background: Rgba) -> Vec<(&'static str, Rgba, Rgba)> {
        let track = rgba(m.track);
        vec![
            ("green", rgba(m.green), track),
            ("yellow", rgba(m.yellow), track),
            ("red", rgba(m.red), track),
            ("hold", rgba(m.hold), track),
            ("guide", rgba(k.guide), background),
            ("locked", rgba(k.locked), background),
        ]
    }

    #[test]
    fn the_meter_and_mark_colors_keep_their_contrast_in_both_suite_modes() {
        for mode in [Mode::Light, Mode::Dark] {
            let (m, k, background) = shown_in(mode);
            assert_eq!(m, *MeterColors::of(mode), "{mode:?}: the frame draws in its own mode's meter colors");
            assert_eq!(k, *MarkColors::of(mode), "{mode:?}: the frame draws in its own mode's mark colors");
            let pairs = contrast_pairs(&m, &k, background);
            assert_eq!(pairs.len(), 6, "four meter marks on the track, the guide and the locked outline on the background");
            for (name, color, under) in pairs {
                let ratio = contrast_ratio(color, under);
                assert!(ratio >= 3.0, "{mode:?}: {name} is {ratio:.2}:1 on what it is drawn on");
            }
        }
    }

    #[test]
    fn the_two_modes_draw_guides_and_locked_outlines_in_their_own_colors() {
        assert_ne!(MarkColors::DARK.guide, MarkColors::LIGHT.guide);
        assert_ne!(MarkColors::DARK.locked, MarkColors::LIGHT.locked);
        assert_ne!(MarkColors::DARK.locked, MarkColors::DARK.guide, "a locked item does not look like a guide");
    }

    #[test]
    fn the_scrim_is_translucent_and_premultiplied() {
        for k in [MarkColors::DARK, MarkColors::LIGHT] {
            let c = paint(k.scrim);
            assert!(c.a() > 0 && c.a() < 255, "the scrim lets the window show through: alpha {}", c.a());
            assert!(c.r() <= c.a() && c.g() <= c.a() && c.b() <= c.a(), "premultiplied channels do not pass the alpha");
        }
        assert_eq!(paint(MarkColors::DARK.scrim), Color32::from_rgba_premultiplied(3, 4, 5, 158), "#050608 at 62%");
        assert_eq!(paint(MarkColors::LIGHT.scrim), Color32::from_rgba_premultiplied(14, 14, 16, 115), "#1E2024 at 45%");
    }

    #[test]
    fn the_meter_target_band_is_the_accent_text_at_its_opacity() {
        let accent = Color32::from_rgb(0x4C, 0x9A, 0xFF);
        let band = paint(MarkColors::DARK.meter_target(accent));
        assert_eq!(band, accent.gamma_multiply(0.38));
        assert!(band.a() < accent.a(), "the band is translucent");
        let hostile = MarkColors { meter_target_opacity: f32::NAN, ..MarkColors::DARK };
        assert_eq!(paint(hostile.meter_target(accent)), Color32::TRANSPARENT, "a broken opacity hides the band");
        let over = MarkColors { meter_target_opacity: 4.0, ..MarkColors::LIGHT };
        assert_eq!(paint(over.meter_target(accent)), accent, "an opacity over 1 is opaque");
    }

    #[test]
    fn comfortable_widths_are_the_compact_ones_scaled_by_the_control_height_ratio() {
        let ratio = 30.0 / 24.0;
        let (compact, comfortable) = (LayoutSizes::of(Size::Compact).all(), LayoutSizes::of(Size::Comfortable).all());
        for ((name, small), (other, large)) in compact.into_iter().zip(comfortable) {
            assert_eq!(name, other);
            assert!(small > 0.0 && small.is_finite(), "{name}: {small}");
            assert_eq!(large, (small * ratio).round(), "{name}: Compact {small}, Comfortable {large}");
        }
        assert_eq!(LayoutSizes::COMPACT.inspector, 296.0);
        assert_eq!(LayoutSizes::COMPACT.palette_width, 540.0);
    }
}
