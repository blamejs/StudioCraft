//! The interface language.
//!
//! Every string the window shows is English source text looked up in the catalogs `build.rs`
//! embedded: StudioCraft's own from `i18n/locales`, merged with a craft-i18n checkout's when
//! `CRAFT_I18N_DIR` is set. A string without a row in the language returns the English. The
//! macros look a string up in the language of the current thread:
//!
//! - `tl!(text)`: plain text.
//! - `tlc!(context, text)`: text with a context, such as `menu`.
//! - `tid!(id, label)`: a command's label, by its command id.
//! - `tmsg!(template, name = value, …)`: a status or error [`Msg`] with `{name}` placeholders. It
//!   keeps its English and is translated when shown.
//!
//! The window hands the same catalogs and the thread's language to the UI runtime's host each
//! frame, so the words the framework draws itself (dialog buttons, the Language menu) and the
//! layout direction follow the interface language.

use std::borrow::Cow;
use std::cell::Cell;
use std::sync::{Arc, LazyLock};

use craft_i18n::{I18n, Lang};

include!(concat!(env!("OUT_DIR"), "/craft_i18n.rs"));

/// `text` in the interface language.
macro_rules! tl {
    ($s:expr) => {
        $crate::i18n::I18N.t($crate::i18n::lang(), $s)
    };
}

/// `text` with a context (`menu`, `@msg`) in the interface language.
macro_rules! tlc {
    ($ctx:expr, $s:expr) => {
        $crate::i18n::I18N.tc($crate::i18n::lang(), $ctx, $s)
    };
}

/// A command's label by its command id, in the interface language.
macro_rules! tid {
    ($id:expr, $label:expr) => {
        $crate::i18n::I18N.tid($crate::i18n::lang(), $id, $label)
    };
}

/// A status or error [`Msg`]: `tmsg!("Exported to {path}", path = p)`.
macro_rules! tmsg {
    ($t:expr $(, $k:ident = $v:expr)* $(,)?) => {
        $crate::i18n::Msg::new($t, vec![$((stringify!($k), ($v).to_string())),*])
    };
}

/// The Language setting that follows the system's preferred languages.
pub const AUTO: &str = "auto";

/// The codes the Language setting offers: English, then every locale with a StudioCraft catalog.
pub fn languages() -> Vec<&'static str> {
    std::iter::once("en").chain(APP_LOCALES.iter().copied()).collect()
}

/// The embedded catalogs.
pub static I18N: LazyLock<Arc<I18n>> = LazyLock::new(|| {
    let offered = languages();
    let (i18n, problems) = I18n::new(CRAFT_I18N, &offered);
    for p in problems {
        log::warn!("translation catalog: {p}");
    }
    Arc::new(i18n)
});

/// The embedded catalogs as the shared handle the UI runtime's host keeps.
pub fn catalogs() -> Arc<I18n> {
    Arc::clone(&I18N)
}

thread_local! {
    static CURRENT: Cell<Lang> = const { Cell::new(Lang::EN) };
}

/// The interface language of this thread.
pub fn lang() -> Lang {
    CURRENT.get()
}

/// Show the interface of this thread in `lang` from now on.
pub fn set_lang(lang: Lang) {
    CURRENT.set(lang);
}

/// Whether `ui` lays out right to left: the direction its env has, which the frame's root values
/// set.
pub(crate) fn is_rtl(ui: &egui::Ui) -> bool {
    craft_ui::runtime::env::Env::of(ui).view().direction().is_rtl()
}

/// A row that starts at the reading start.
pub(crate) fn row(ui: &egui::Ui) -> egui::Layout {
    if is_rtl(ui) { egui::Layout::right_to_left(egui::Align::Center) } else { egui::Layout::left_to_right(egui::Align::Center) }
}

/// A row that starts at the reading end.
pub(crate) fn trailing(ui: &egui::Ui) -> egui::Layout {
    if is_rtl(ui) { egui::Layout::left_to_right(egui::Align::Center) } else { egui::Layout::right_to_left(egui::Align::Center) }
}

/// A column aligned to the reading start.
pub(crate) fn column(ui: &egui::Ui) -> egui::Layout {
    egui::Layout::top_down(if is_rtl(ui) { egui::Align::Max } else { egui::Align::Min })
}

/// A row one control tall that starts at the reading start.
pub(crate) fn horizontal<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> egui::InnerResponse<R> {
    let size = egui::vec2(ui.available_size_before_wrap().x, ui.spacing().interact_size.y);
    let layout = row(ui);
    ui.allocate_ui_with_layout(size, layout, add)
}

/// The x of `rect`'s reading start.
pub(crate) fn start_x(ui: &egui::Ui, rect: egui::Rect) -> f32 {
    if is_rtl(ui) { rect.right() } else { rect.left() }
}

/// The language a Language setting stands for: `auto`, or a code that is not offered, follows the
/// system.
pub fn resolve(setting: &str) -> Lang {
    if setting == AUTO {
        return I18N.system_lang();
    }
    I18N.from_code(setting).unwrap_or_else(|| I18N.system_lang())
}

/// The saved Language setting as the window reads it: `auto` unless it names an offered
/// language.
pub fn normalize(setting: &str) -> &str {
    if setting != AUTO && languages().iter().any(|c| c.eq_ignore_ascii_case(setting)) { setting } else { AUTO }
}

/// A command's label as the menus and the shortcut list show it: by its command id when `label`
/// is the command's own label, else `label` as menu text (a short label such as `New…`).
pub fn command_label<'a>(id: &str, label: &'a str) -> &'a str {
    let own = crate::commands::UI_COMMANDS
        .iter()
        .find(|(c, _)| *c == id)
        .map(|(_, l)| *l)
        .or_else(|| studiocraft_engine::specs().iter().find(|s| s.id == id).map(|s| s.label));
    if own == Some(label) { I18N.tid(lang(), id, label) } else { I18N.tc(lang(), "menu", label) }
}

/// A label craft-menu writes itself (the platform's Quit, About and Settings items, the titles of
/// the menus it adds, the overflow menu), in the interface language.
pub fn menu_text(english: &'static str) -> String {
    tlc!("menu", english).to_string()
}

/// A status or error message: an English template with `{name}` placeholders and their values.
/// It is kept in English (logs, the control channel) and translated when the window shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct Msg {
    template: Cow<'static, str>,
    args: Vec<(&'static str, String)>,
}

impl Msg {
    pub fn new(template: &'static str, args: Vec<(&'static str, String)>) -> Msg {
        Msg { template: Cow::Borrowed(template), args }
    }

    /// A message with no placeholders, such as an error text from another crate.
    pub fn plain(text: impl Into<String>) -> Msg {
        Msg { template: Cow::Owned(text.into()), args: Vec::new() }
    }

    fn args(&self) -> Vec<(&str, &str)> {
        self.args.iter().map(|(k, v)| (*k, v.as_str())).collect()
    }

    /// The message in English.
    pub fn english(&self) -> String {
        if self.args.is_empty() { self.template.to_string() } else { craft_i18n::fmt_plain(&self.template, &self.args()) }
    }

    /// The message in the interface language.
    pub fn shown(&self) -> String {
        self.shown_in(&I18N, lang())
    }

    /// The message in `lang` of `i18n`: its template's `@msg` row, with the values filled in.
    pub fn shown_in(&self, i18n: &I18n, lang: Lang) -> String {
        let template = i18n.tc(lang, "@msg", &self.template);
        if self.args.is_empty() { template.to_string() } else { craft_i18n::fmt(template, &self.args()) }
    }
}

impl From<String> for Msg {
    fn from(text: String) -> Msg {
        Msg::plain(text)
    }
}

impl From<&str> for Msg {
    fn from(text: &str) -> Msg {
        Msg::plain(text)
    }
}

#[cfg(test)]
mod tests;
