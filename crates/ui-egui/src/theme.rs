//! The window's look: the suite theme the UI runtime resolves, in dark mode and the Compact size
//! set, and the colors, sizes and fonts the window's own painters read from its tokens.

use std::sync::{Arc, OnceLock};

use craft_theme::{AppDefaults, AppSizes, ComponentSet, Mode, ModeChoice, Rgba, Size, ThemeId, ThemePair};
use craft_ui::runtime::env::RootEnv;
use craft_ui::style::StyleCx;
use egui::{Color32, FontId};

/// What the window shows: the suite theme in both modes, dark, in the Compact size set, with the
/// creative apps' components.
pub fn app_defaults() -> AppDefaults {
    AppDefaults {
        theme: ThemePair::same(ThemeId::suite_default()),
        mode: ModeChoice::Dark,
        size: Size::Compact,
        sizes: AppSizes::default(),
        set: ComponentSet::Creative,
    }
}

/// The root values each frame starts from: the theme, mode, size set, sizes and components of
/// [`app_defaults`]. Every call shares one sizes allocation, so the runtime finds the tokens it
/// resolved on an earlier frame instead of resolving them again.
pub fn root_env() -> RootEnv {
    static SIZES: OnceLock<Arc<AppSizes>> = OnceLock::new();
    let d = app_defaults();
    let mode = match d.mode {
        ModeChoice::Light => Mode::Light,
        _ => Mode::Dark,
    };
    let mut root = RootEnv::new().theme(d.theme.slot(mode).clone()).mode(d.mode).density(d.size).set(d.set);
    root.sizes = SIZES.get_or_init(|| Arc::new(d.sizes)).clone();
    root
}

/// egui's own theme for the resolved `mode`, for the egui widgets the window still draws itself.
pub fn egui_theme(mode: Mode) -> egui::Theme {
    match mode {
        Mode::Light => egui::Theme::Light,
        Mode::Dark => egui::Theme::Dark,
    }
}

/// Runs `f` with the style context of the window's root scope, whose tokens the root values
/// resolved.
pub fn with_root<R>(ctx: &egui::Context, f: impl FnOnce(&StyleCx<'_>) -> R) -> R {
    StyleCx::with_root(ctx, f)
}

/// The color token `c` as egui paints it.
pub fn color(cx: &StyleCx<'_>, c: Rgba) -> Color32 {
    cx.color(c).egui()
}

/// The font of dock and section titles: the title role's size in the semibold family.
pub fn title_font(cx: &StyleCx<'_>) -> FontId {
    FontId::new(cx.tokens.typography.title.size, crate::fonts::semibold())
}

/// The font of the live and recording timers: the mono role.
pub fn timer_font(cx: &StyleCx<'_>) -> FontId {
    FontId::monospace(cx.tokens.typography.mono.size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_frame_starts_from_one_sizes_allocation() {
        let (a, b) = (root_env(), root_env());
        assert!(Arc::ptr_eq(&a.sizes, &b.sizes), "the runtime matches its token cache by the sizes allocation");
        assert_eq!(*a.sizes, app_defaults().sizes);
    }
}
