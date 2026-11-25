//! The page's language.
//!
//! Every string the page shows is English source text looked up in StudioCraft's own catalogs, the
//! ones in `crates/ui-egui/i18n/locales` that the app's window uses. A string without a row in the
//! language shows in English. The page reads the language from the browser's preferred languages.
//!
//! - [`tl`]: a label.
//! - [`tmsg`]: a status or error message with `{name}` placeholders, from its `@msg` row.

use std::cell::Cell;
use std::sync::LazyLock;

use craft_i18n::{I18n, Lang};

/// The catalogs built into the page: (locale, shared catalog, StudioCraft's catalog). The `en`
/// folder holds the English source rows and is not built in.
pub static CATALOGS: &craft_i18n::Embedded = &[
    ("ar", "", include_str!("../../../crates/ui-egui/i18n/locales/ar/studiocraft.tsv")),
    ("cs", "", include_str!("../../../crates/ui-egui/i18n/locales/cs/studiocraft.tsv")),
    ("de", "", include_str!("../../../crates/ui-egui/i18n/locales/de/studiocraft.tsv")),
    ("es", "", include_str!("../../../crates/ui-egui/i18n/locales/es/studiocraft.tsv")),
    ("fr", "", include_str!("../../../crates/ui-egui/i18n/locales/fr/studiocraft.tsv")),
    ("he", "", include_str!("../../../crates/ui-egui/i18n/locales/he/studiocraft.tsv")),
    ("it", "", include_str!("../../../crates/ui-egui/i18n/locales/it/studiocraft.tsv")),
    ("ja", "", include_str!("../../../crates/ui-egui/i18n/locales/ja/studiocraft.tsv")),
    ("ko", "", include_str!("../../../crates/ui-egui/i18n/locales/ko/studiocraft.tsv")),
    ("nl", "", include_str!("../../../crates/ui-egui/i18n/locales/nl/studiocraft.tsv")),
    ("pl", "", include_str!("../../../crates/ui-egui/i18n/locales/pl/studiocraft.tsv")),
    ("pt-br", "", include_str!("../../../crates/ui-egui/i18n/locales/pt-br/studiocraft.tsv")),
    ("ru", "", include_str!("../../../crates/ui-egui/i18n/locales/ru/studiocraft.tsv")),
    ("sv", "", include_str!("../../../crates/ui-egui/i18n/locales/sv/studiocraft.tsv")),
    ("tr", "", include_str!("../../../crates/ui-egui/i18n/locales/tr/studiocraft.tsv")),
    ("uk", "", include_str!("../../../crates/ui-egui/i18n/locales/uk/studiocraft.tsv")),
    ("zh-hans", "", include_str!("../../../crates/ui-egui/i18n/locales/zh-hans/studiocraft.tsv")),
    ("zh-hant", "", include_str!("../../../crates/ui-egui/i18n/locales/zh-hant/studiocraft.tsv")),
];

/// The codes the page can show: English, then every built-in catalog.
pub fn languages() -> Vec<&'static str> {
    std::iter::once("en").chain(CATALOGS.iter().map(|c| c.0)).collect()
}

/// The built-in catalogs.
pub static I18N: LazyLock<I18n> = LazyLock::new(|| I18n::new(CATALOGS, &languages()).0);

thread_local! {
    static CURRENT: Cell<Lang> = const { Cell::new(Lang::EN) };
}

/// The page's language.
pub fn lang() -> Lang {
    CURRENT.get()
}

/// Show the page in `lang` from now on.
pub fn set_lang(lang: Lang) {
    CURRENT.set(lang);
}

/// `text` in the page's language.
pub fn tl(text: &str) -> &str {
    I18N.t(lang(), text)
}

/// A status or error message: the `@msg` row of `template` in the page's language, with each
/// `{name}` replaced by its value.
pub fn tmsg(template: &str, args: &[(&str, &str)]) -> String {
    let text = I18N.tc(lang(), "@msg", template);
    if args.is_empty() { text.to_string() } else { craft_i18n::fmt(text, args) }
}
