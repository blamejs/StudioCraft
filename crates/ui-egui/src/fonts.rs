//! The window's fonts, through craft-ui's font stack ([`craft_ui::fonts::UiFonts`]).
//!
//! The app's own faces come first: Inter for text, Inter SemiBold for titles and JetBrains Mono for
//! readouts, embedded from `assets/fonts`. The craft-fonts faces the build embedded from
//! `CRAFT_FONTS_DIR` follow them for the scripts they lack, and egui's built-in faces come last.
//! Without `CRAFT_FONTS_DIR` the stack is the three app faces and egui's faces.

use craft_fonts_build::EmbeddedFace;
use craft_ui::fonts::{Family, FontRef, I18nScripts, Stack, UiFonts};
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

/// The fonts for an interface in `language` (a code such as `en` or `pt-br`), with `craft` as the
/// craft-fonts faces.
pub fn ui_fonts(language: &str, craft: &[EmbeddedFace]) -> UiFonts<'static> {
    UiFonts::new(&I18nScripts)
        .app_face(INTER, FontData::from_static(INTER_REGULAR), &[FontRef::proportional()])
        .app_face(INTER_SEMI, FontData::from_static(INTER_SEMIBOLD), &[FontRef::named(SEMIBOLD)])
        .app_face(MONO, FontData::from_static(JETBRAINS_MONO), &[FontRef::monospace()])
        .family(Family::named(SEMIBOLD).weight(600))
        .craft_faces(craft)
        .languages(&crate::i18n::languages())
        .ui_language(language)
}

/// The font stack for an interface in `language`, with `craft` as the craft-fonts faces.
pub fn stack(language: &str, craft: &[EmbeddedFace]) -> Stack {
    ui_fonts(language, craft).stack()
}

/// Install the fonts for an interface in `language` on `ctx`. Call it again when the interface
/// language changes.
pub fn install(ctx: &egui::Context, language: &str) {
    ui_fonts(language, CRAFT_FONTS).install(ctx);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_app_faces_lead_their_families() {
        let stack = stack("en", &[]);
        assert_eq!(stack.chain(&FontRef::proportional()).first().map(String::as_str), Some(INTER));
        assert_eq!(stack.chain(&FontRef::named(SEMIBOLD)).first().map(String::as_str), Some(INTER_SEMI));
        assert_eq!(stack.chain(&FontRef::monospace()).first().map(String::as_str), Some(MONO));
        assert!(stack.weights().contains(&(FontRef::named(SEMIBOLD), 600)), "{:?}", stack.weights());
    }

    #[test]
    fn a_label_lays_out_in_each_family_after_installing_for_a_second_language() {
        let ctx = egui::Context::default();
        install(&ctx, "en");
        install(&ctx, "de");
        let mut drawn = Vec::new();
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            ui.label("Szenen");
            drawn = ui.ctx().fonts_mut(|f| {
                [FontFamily::Proportional, semibold(), FontFamily::Monospace]
                    .into_iter()
                    .map(|family| f.has_glyph(&egui::FontId::new(12.0, family), 'S'))
                    .collect()
            });
        });
        out.textures_delta.clear();
        assert_eq!(drawn, [true, true, true]);
    }
}
