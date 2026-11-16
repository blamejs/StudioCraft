//! The window's fonts, through craft-ui's font stack ([`craft_ui::fonts::UiFonts`]).
//!
//! The app's own faces come first: Inter for text, Inter SemiBold for titles and JetBrains Mono for
//! readouts, embedded from `assets/fonts`. The craft-fonts faces the build embedded from
//! `CRAFT_FONTS_DIR` follow them for the scripts they lack, and egui's built-in faces come last.
//! Without `CRAFT_FONTS_DIR` the stack is the three app faces and egui's faces.

use craft_fonts_build::EmbeddedFace;
use craft_ui::fonts::{CraftFace, Family, I18nScripts, Stack, UiFonts};
use egui::{FontData, FontFamily};

/// The faces `build.rs` embedded; empty when the app was built without `CRAFT_FONTS_DIR`.
pub static CRAFT_FONTS: &[EmbeddedFace] = include!(concat!(env!("OUT_DIR"), "/craft_fonts.rs"));

/// Inter Regular, SIL Open Font License 1.1 (`assets/fonts/OFL-Inter.txt`).
pub static INTER_REGULAR: &[u8] = include_bytes!("../../../assets/fonts/Inter-Regular.ttf");
/// Inter SemiBold, SIL Open Font License 1.1 (`assets/fonts/OFL-Inter.txt`).
pub static INTER_SEMIBOLD: &[u8] = include_bytes!("../../../assets/fonts/Inter-SemiBold.ttf");
/// JetBrains Mono Regular, SIL Open Font License 1.1 (`assets/fonts/OFL-JetBrainsMono.txt`).
pub static JETBRAINS_MONO: &[u8] = include_bytes!("../../../assets/fonts/JetBrainsMono-Regular.ttf");

/// The egui name of Inter Regular.
pub const INTER: &str = "Inter Regular";
/// The egui name of Inter SemiBold.
pub const INTER_SEMI: &str = "Inter SemiBold";
/// The egui name of JetBrains Mono Regular.
pub const MONO: &str = "JetBrains Mono Regular";

/// The named family for dock, section and dialog titles.
pub const SEMIBOLD: &str = "semibold";

/// The semibold family as egui names it.
pub fn semibold() -> FontFamily {
    FontFamily::Name(SEMIBOLD.into())
}

/// The font stack for an interface in `language` (a code such as `en` or `pt-br`), with `craft` as
/// the craft-fonts faces.
pub fn stack(language: &str, craft: &[EmbeddedFace]) -> Stack {
    let craft: Vec<CraftFace> = craft.iter().copied().map(CraftFace::from).collect();
    UiFonts::new(&I18nScripts)
        .app_face(INTER, FontData::from_static(INTER_REGULAR), &[FontFamily::Proportional])
        .app_face(INTER_SEMI, FontData::from_static(INTER_SEMIBOLD), &[semibold()])
        .app_face(MONO, FontData::from_static(JETBRAINS_MONO), &[FontFamily::Monospace])
        .family(Family::named(SEMIBOLD).weight(600))
        .craft_faces(&craft)
        .languages(&crate::i18n::languages())
        .ui_language(language)
        .stack()
}

/// Install the fonts for an interface in `language` on `ctx`. Call it again when the interface
/// language changes.
pub fn install(ctx: &egui::Context, language: &str) {
    stack(language, CRAFT_FONTS).install(ctx);
}

#[cfg(test)]
mod tests {
    use craft_ui::fonts::{Sample, missing_glyphs};

    use super::*;
    use crate::theme::{CHECK_MARK, RADIO_MARK};

    fn marks() -> [Sample<'static>; 2] {
        [Sample { language: "en", key: "check mark", text: CHECK_MARK }, Sample { language: "en", key: "radio mark", text: RADIO_MARK }]
    }

    /// A craft face for `scripts`, made from Inter's file.
    fn craft(family: &'static str, scripts: &'static [&'static str]) -> EmbeddedFace {
        EmbeddedFace { family, style: "Regular", scripts, index: 0, bytes: INTER_REGULAR }
    }

    #[test]
    fn the_menu_marks_draw_with_a_real_glyph() {
        for (built, faces) in [("without CRAFT_FONTS_DIR", &[][..]), ("with this build's craft faces", CRAFT_FONTS)] {
            let stack = stack("en", faces);
            for family in [FontFamily::Proportional, semibold(), FontFamily::Monospace] {
                let missing = missing_glyphs(&stack, &family, marks(), false);
                let names: Vec<String> = missing.iter().map(ToString::to_string).collect();
                assert!(missing.is_empty(), "{built}, {family:?}: {}", names.join("; "));
            }
        }
    }

    #[test]
    fn the_app_faces_lead_their_families() {
        let stack = stack("en", &[]);
        assert_eq!(stack.chain(&FontFamily::Proportional).first().map(String::as_str), Some(INTER));
        assert_eq!(stack.chain(&semibold()).first().map(String::as_str), Some(INTER_SEMI));
        assert_eq!(stack.chain(&FontFamily::Monospace).first().map(String::as_str), Some(MONO));
        assert!(stack.weights().contains(&(semibold(), 600)), "{:?}", stack.weights());
    }

    #[test]
    fn craft_faces_follow_the_app_face_unless_the_language_needs_its_own_script_first() {
        let arabic = [craft("Test Arabic", &["Arab"])];
        let english = stack("en", &arabic);
        let chain = english.chain(&FontFamily::Proportional);
        assert_eq!(chain.first().map(String::as_str), Some(INTER), "{chain:?}");
        assert!(chain.iter().any(|n| n == "Test Arabic Regular"), "{chain:?}");

        let arabic_ui = stack("ar", &arabic);
        let chain = arabic_ui.chain(&FontFamily::Proportional);
        assert_eq!(chain.first().map(String::as_str), Some("Test Arabic Regular"), "{chain:?}");
        let mono = arabic_ui.chain(&FontFamily::Monospace);
        assert_eq!(mono.first().map(String::as_str), Some(MONO), "{mono:?}");
    }

    #[test]
    fn a_label_with_the_marks_lays_out_after_installing_for_a_second_language() {
        let ctx = egui::Context::default();
        install(&ctx, "en");
        install(&ctx, "de");
        let mut drawn = (false, false);
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            ui.label(format!("{CHECK_MARK} {RADIO_MARK}"));
            drawn = ui.ctx().fonts_mut(|f| {
                let id = egui::FontId::proportional(12.0);
                (f.has_glyph(&id, '\u{2713}'), f.has_glyph(&id, '\u{25cf}'))
            });
        });
        out.textures_delta.clear();
        assert_eq!(drawn, (true, true));
    }
}
