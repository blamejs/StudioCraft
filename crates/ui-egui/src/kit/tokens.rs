//! The colors of the audio level meters: the green, yellow and red zones, the peak hold mark and
//! the well behind the bars, in dark and light mode.
//!
//! The suite theme has no meter colors, so a meter keeps its own as data colors.

use craft_theme::Mode;
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
}

const fn hex(rgb: u32) -> DataColor {
    DataColor::new(Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8))
}

impl MeterColors {
    pub const DARK: MeterColors =
        MeterColors { green: hex(0x3DBE7A), yellow: hex(0xE8C93A), red: hex(0xE5484D), hold: hex(0xFFFFFF), track: hex(0x0F1012) };

    pub const LIGHT: MeterColors =
        MeterColors { green: hex(0x3DBE7A), yellow: hex(0xE8C93A), red: hex(0xE5484D), hold: hex(0xFFFFFF), track: hex(0x2B2D33) };

    /// The colors for `mode`.
    pub fn of(mode: Mode) -> &'static MeterColors {
        match mode {
            Mode::Light => &MeterColors::LIGHT,
            Mode::Dark => &MeterColors::DARK,
        }
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

    /// The meter colors a frame in suite mode `mode` draws with.
    fn shown_in(mode: Mode) -> MeterColors {
        let ctx = crate::test_context();
        craft_ui::shell::embed::before_input(&ctx, |_| FrameInput::new(crate::theme::root_env().mode(mode)));
        let mut shown = None;
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| shown = Some(*MeterColors::of(Env::of(ui).view().mode())));
        out.textures_delta.clear();
        let Some(shown) = shown else { panic!("the frame did not run") };
        shown
    }

    #[test]
    fn the_meter_colors_keep_their_contrast_on_the_track_in_both_suite_modes() {
        for mode in [Mode::Light, Mode::Dark] {
            let m = shown_in(mode);
            assert_eq!(m, *MeterColors::of(mode), "{mode:?}: the frame draws in its own mode's colors");
            for (name, color) in [("green", m.green), ("yellow", m.yellow), ("red", m.red), ("hold", m.hold)] {
                let ratio = contrast_ratio(rgba(color), rgba(m.track));
                assert!(ratio >= 3.0, "{mode:?}: {name} on the track is {ratio:.2}:1");
            }
        }
    }
}
