use std::collections::BTreeSet;

use craft_i18n::Lang;
use craft_keymap::{Binding, CommandRef};
use craft_menu::Node;
use craft_settings::Location;
use serde_json::{Value, json};

use super::*;
use crate::StudioApp;
use crate::settings::{TABS, Tab};
use crate::store::Store;

/// One frame of the window; every text it painted.
fn painted(app: &mut StudioApp, ctx: &egui::Context) -> Vec<String> {
    let raw = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1600.0, 1000.0))), ..Default::default() };
    app.before_input(ctx);
    let mut out = ctx.run_ui(raw, |ui| app.frame(ui));
    out.textures_delta.clear();
    fn walk(shape: &egui::Shape, texts: &mut Vec<String>) {
        match shape {
            egui::Shape::Text(t) => texts.push(t.galley.text().to_string()),
            egui::Shape::Vec(v) => v.iter().for_each(|s| walk(s, texts)),
            _ => {}
        }
    }
    let mut texts = Vec::new();
    for clipped in &out.shapes {
        walk(&clipped.shape, &mut texts);
    }
    texts
}

/// Two frames (a new window lays itself out unseen in its first); the texts of the second.
fn shown(app: &mut StudioApp, ctx: &egui::Context) -> Vec<String> {
    painted(app, ctx);
    painted(app, ctx)
}

/// Every string in a JSON value.
fn strings(v: &Value, out: &mut BTreeSet<String>) {
    match v {
        Value::String(s) => {
            out.insert(s.clone());
        }
        Value::Array(a) => a.iter().for_each(|v| strings(v, out)),
        Value::Object(o) => o.values().for_each(|v| strings(v, out)),
        _ => {}
    }
}

/// Text the window shows as it is, not as interface text: names and values from the scene
/// collection, the profile and the settings, the app's name, the license texts, the contributors,
/// shortcuts and the configured ffmpeg's own version text.
fn data(app: &StudioApp) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    strings(&serde_json::to_value(app.studio().collection()).unwrap_or_default(), &mut out);
    strings(&serde_json::to_value(app.studio().profile()).unwrap_or_default(), &mut out);
    out.extend(app.collections.iter().cloned());
    out.extend(app.profiles.iter().cloned());
    out.insert(crate::APP_NAME.to_string());
    for l in crate::credits::License::ALL {
        out.insert(l.text().to_string());
    }
    for (shown, login) in crate::credits::sorted(studiocraft_engine::credits::CONTRIBUTORS, app.credit_names) {
        out.insert(shown.to_string());
        out.insert(login.to_string());
    }
    if let Some(Ok(v)) = app.settings.as_ref().map(|w| &w.ffmpeg_version) {
        out.insert(v.clone());
    }
    for f in studiocraft_model::RecordingFormat::ALL {
        out.insert(f.extension().to_uppercase());
    }
    if let Some(list) = &app.encoders {
        out.extend(list.iter().map(|(_, codec, _)| codec.clone()));
    }
    // The streaming services' names are their own.
    out.extend(studiocraft_model::SERVICES.iter().map(|s| s.name.to_string()));
    out
}

/// The painted texts that did not go through translation: no pseudo marks, a letter in them, and
/// not data, a shortcut or a link.
fn untranslated(texts: &[String], data: &BTreeSet<String>) -> Vec<String> {
    texts
        .iter()
        .filter(|t| !t.contains('\u{27e6}'))
        .filter(|t| t.chars().any(char::is_alphabetic))
        .filter(|t| !data.contains(t.as_str()))
        .filter(|t| !t.contains("://"))
        .filter(|t| !crate::settings::split_bindings(t).iter().all(|k| Binding::parse(k).is_ok()))
        .cloned()
        .collect()
}

#[test]
fn the_embedded_catalogs_have_no_problems() {
    assert!(I18N.check_all().is_empty(), "{:?}", I18N.check_all());
    let offered = languages();
    assert_eq!(offered.first(), Some(&"en"));
    for code in &offered {
        assert!(I18N.from_code(code).is_some(), "{code} is offered but not embedded");
    }
    assert_eq!(I18N.languages().len(), offered.len());
}

#[test]
fn every_label_drawn_in_the_pseudo_locale_went_through_translation() {
    let mut a = StudioApp::boot(Store::memory(), None, Vec::new());
    // The framework's own words come from a second copy of the catalogs, so they are drawn
    // marked and their lookups stay apart from StudioCraft's: craft-i18n's catalogs list them,
    // not `studiocraft.tsv`.
    let (framework, problems) = craft_i18n::I18n::new(CRAFT_I18N, &languages());
    assert!(problems.is_empty(), "{problems:?}");
    let framework = std::sync::Arc::new(framework);
    set_lang(Lang::PSEUDO);
    a.use_catalogs(std::sync::Arc::clone(&framework));
    let ctx = crate::test_context();
    a.perform(&ctx, &CommandRef::with_params("source.add", json!({"kind": "color", "name": "Backdrop"}))).unwrap();
    a.perform(&ctx, &CommandRef::with_params("source.add", json!({"kind": "text", "name": "Caption"}))).unwrap();
    let mut seen = Vec::new();
    seen.extend(shown(&mut a, &ctx));
    a.perform(&ctx, &CommandRef::new("studio.toggle")).unwrap();
    seen.extend(shown(&mut a, &ctx));
    // The configured ffmpeg's version text is its own, shown as it reports it.
    let mut ffmpeg_versions = BTreeSet::new();
    for (tab, _) in TABS {
        a.settings = None;
        a.open_settings(tab);
        seen.extend(shown(&mut a, &ctx));
        if let Some(Ok(v)) = a.settings.as_ref().map(|w| &w.ffmpeg_version) {
            ffmpeg_versions.insert(v.clone());
        }
    }
    a.settings = None;
    let source = a.selection().map(|(_, s, _)| s).unwrap();
    a.open_properties(source);
    a.open_filters(source);
    a.perform(&ctx, &CommandRef::with_params("filter.add", json!({"source": source, "kind": "color_correction"}))).unwrap();
    a.perform(&ctx, &CommandRef::new("view.stats")).unwrap();
    a.perform(&ctx, &CommandRef::new("view.scopes")).unwrap();
    a.perform(&ctx, &CommandRef::new("app.about")).unwrap();
    a.perform(&ctx, &CommandRef::new("app.licenses")).unwrap();
    a.encoders = Some(vec![("H.264".into(), "libx264".into(), true), ("AV1".into(), "libsvtav1".into(), false)]);
    seen.extend(shown(&mut a, &ctx));
    a.properties.clear();
    a.filters.clear();
    a.ask_name(&ctx, crate::Named::NewProfile, "");
    seen.extend(shown(&mut a, &ctx));
    a.answer_dialog(&ctx, false, None).unwrap();
    a.confirm_exit = true;
    a.notify(tmsg!("Exported the scene collection to {path}", path = "/tmp/show.json"), false);
    seen.extend(shown(&mut a, &ctx));

    let mut known = data(&a);
    known.extend(ffmpeg_versions);
    let missed = untranslated(&seen, &known);
    assert!(missed.is_empty(), "drawn without translation: {missed:?}");
    assert!(seen.iter().any(|t| t == "\u{27e6}Exported the scene collection to /tmp/show.json\u{27e7}"), "{seen:?}");

    // The framework's words were drawn in the pseudo locale too: the Language menu's own labels.
    let words = framework.looked_up();
    for word in ["Language", "Automatic"] {
        assert!(words.contains(word), "the framework word `{word}` was not looked up: {words:?}");
    }

    // Every menu item and every entry of the shortcut list was looked up.
    let looked = I18N.looked_up();
    // Every text looked up has an English source row, in some context. A framework word another
    // test drew in the pseudo locale through the embedded catalogs is not StudioCraft's.
    let sources: BTreeSet<String> = english().into_iter().map(|(_, s)| s).collect();
    let unlisted: Vec<String> =
        looked.iter().filter(|t| !sources.contains(*t) && !words.contains(*t)).map(|t| format!("\t{}\t{}", escape(t), escape(t))).collect();
    assert!(unlisted.is_empty(), "looked up with no row in i18n/locales/en/studiocraft.tsv:\n{}", unlisted.join("\n"));
    set_lang(Lang::EN);
    let english = a.laid_menu();
    fn labels(nodes: &[Node], out: &mut Vec<String>) {
        for n in nodes {
            match n {
                // The Language menu's labels are the framework's words and the languages' own names.
                Node::Submenu { role: Some(craft_menu::ItemRole::Language), .. } => {}
                Node::Item(it) => out.push(it.label.clone()),
                Node::Submenu { label, children, .. } => {
                    out.push(label.clone());
                    labels(children, out);
                }
                _ => {}
            }
        }
    }
    let mut menu = Vec::new();
    for m in &english.menus {
        menu.push(m.title.clone());
        labels(&m.children, &mut menu);
    }
    let names: BTreeSet<&String> = a.profiles.iter().chain(a.collections.iter()).collect();
    for label in menu.iter().filter(|l| !names.contains(l)) {
        assert!(looked.contains(label), "the menu label `{label}` was not looked up");
    }
    for b in crate::commands::catalog().iter().filter(|b| b.command.params.is_null()) {
        assert!(looked.contains(&b.label), "the shortcut label `{}` was not looked up", b.label);
    }
    for (_, label) in TABS {
        assert!(looked.contains(label), "{label}");
    }
}

#[test]
fn a_status_message_translates_its_template_and_keeps_the_value() {
    static CATALOGS: &craft_i18n::Embedded = &[("de", "", "@msg\tExported the scene collection to {path}\tSzenensammlung nach {path} exportiert\n")];
    let (i18n, problems) = craft_i18n::I18n::new(CATALOGS, &["en", "de"]);
    assert!(problems.is_empty(), "{problems:?}");
    let de = i18n.from_code("de").unwrap();
    let msg = tmsg!("Exported the scene collection to {path}", path = "/tmp/show.json");
    assert_eq!(msg.english(), "Exported the scene collection to /tmp/show.json");
    assert_eq!(msg.shown_in(&i18n, de), "Szenensammlung nach /tmp/show.json exportiert");
    assert_eq!(msg.shown_in(&i18n, Lang::EN), "Exported the scene collection to /tmp/show.json");
    // A value is never read as a template.
    let odd = tmsg!("Exported the scene collection to {path}", path = "{path}.json");
    assert_eq!(odd.shown_in(&i18n, de), "Szenensammlung nach {path}.json exportiert");
    // A message from another crate, without placeholders, is looked up whole.
    assert_eq!(Msg::plain("select a source first").shown_in(&i18n, de), "select a source first");
}

#[test]
fn studio_notices_are_translated_with_their_values() {
    let de = I18N.from_code("de").unwrap();
    let saved = studiocraft_studio::Notice::new("Recording saved to {path}", vec![("path", "/v/a.mkv".into())], false);
    assert_eq!(saved.text(), "Recording saved to /v/a.mkv");
    assert_eq!(Msg::new(saved.template, saved.args).shown_in(&I18N, de), "Aufnahme gespeichert unter /v/a.mkv");
    // A plain notice, such as an error from another crate, is shown as it is.
    let plain = studiocraft_studio::Notice::plain("{path} x", true);
    assert_eq!(plain.text(), "{path} x");
    assert_eq!(Msg::new(plain.template, plain.args).shown_in(&I18N, de), "{path} x");
}

#[test]
fn notices_keep_english_and_their_cards_translate_them() {
    let mut a = StudioApp::boot(Store::memory(), None, Vec::new());
    set_lang(Lang::PSEUDO);
    let ctx = crate::test_context();
    a.notify(tmsg!("Imported the scene collection `{name}`", name = "Late Show"), false);
    let texts = shown(&mut a, &ctx);
    assert!(texts.iter().any(|t| t == "\u{27e6}Imported the scene collection `Late Show`\u{27e7}"), "{texts:?}");
    let hud = a.hud();
    let last = hud["ui"]["notices"].as_array().and_then(|n| n.last()).cloned().unwrap_or_default();
    assert_eq!(last["text"], json!("Imported the scene collection `Late Show`"));
}

#[test]
fn the_framework_words_follow_the_interface_language() {
    let cancel = craft_i18n::Word::keyed("dialog.cancel", "Cancel");
    let mut a = StudioApp::boot(Store::memory(), None, Vec::new());
    let ctx = crate::test_context();
    a.set_language("en");
    assert_eq!(a.words.get(cancel), "Cancel");
    a.set_language("de");
    assert_eq!(a.words.get(cancel), "Abbrechen");
    // A language set on the thread alone reaches the host with the next frame.
    set_lang(Lang::PSEUDO);
    a.frame_input(&ctx, craft_ui::runtime::frame::FrameOutput::default());
    assert_eq!(a.words.get(cancel), "\u{27e6}Cancel\u{27e7}");
    a.set_language("en");
    assert_eq!(a.words.get(cancel), "Cancel");
}

#[test]
fn the_root_direction_follows_the_language() {
    use craft_ui::runtime::env::Direction;
    use craft_ui::runtime::frame::FrameOutput;
    let mut a = StudioApp::boot(Store::memory(), None, Vec::new());
    let ctx = crate::test_context();
    for (code, direction) in [("en", Direction::Ltr), ("ar", Direction::Rtl), ("de", Direction::Ltr), ("he", Direction::Rtl), ("en", Direction::Ltr)]
    {
        a.set_language(code);
        let input = a.frame_input(&ctx, FrameOutput::default());
        let Some(host) = input.host else { panic!("{code}: the frame has no host") };
        assert_eq!(host.i18n().map(|i| i.code(host.lang())), Some(code));
        assert_eq!(input.root.direction, direction, "{code}");
    }
}

/// The Language submenu of the laid-out menu bar, wherever the platform puts it.
fn language_submenu(bar: &craft_menu::MenuBar) -> Option<(String, Vec<Node>)> {
    fn find(nodes: &[Node]) -> Option<(String, Vec<Node>)> {
        nodes.iter().find_map(|n| match n {
            Node::Submenu { label, children, role: Some(craft_menu::ItemRole::Language), .. } => Some((label.clone(), children.clone())),
            Node::Submenu { children, .. } => find(children),
            _ => None,
        })
    }
    bar.menus.iter().find_map(|m| find(&m.children))
}

/// Each Language row: its label, its `lang` parameter and whether it is chosen.
fn language_rows(rows: &[Node]) -> Vec<(String, String, bool)> {
    rows.iter()
        .filter_map(|n| match n {
            Node::Item(it) => {
                assert_eq!(it.command.id, "app.language");
                let code = it.command.params["lang"].as_str().unwrap_or_default().to_string();
                Some((it.label.clone(), code, it.kind == craft_menu::ItemKind::Radio(true)))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn the_language_menu_lists_the_catalogs() {
    let mut a = StudioApp::boot(Store::memory(), None, Vec::new());
    let ctx = crate::test_context();
    a.set_language("auto");
    let Some((label, rows)) = language_submenu(&a.laid_menu()) else { panic!("no Language submenu") };
    assert_eq!(label, I18N.tc(lang(), "menu", "Language"));
    // Automatic first and chosen, then English and every built-in language by its own name.
    let mut expected = vec![(I18N.t(lang(), "Automatic").to_string(), AUTO.to_string(), true)];
    expected.extend(I18N.languages().into_iter().map(|l| (I18N.name(l).to_string(), I18N.code(l).to_string(), false)));
    assert_eq!(language_rows(&rows), expected);
    assert_eq!(expected.len(), languages().len() + 1);

    // Choosing a row saves the setting and shows the menu, chosen row included, in that language.
    let r = a.perform(&ctx, &CommandRef::with_params("app.language", json!({"lang": "de"}))).unwrap();
    assert_eq!(r, json!({"language": "de"}));
    assert_eq!(lang(), I18N.from_code("de").unwrap());
    assert_eq!(a.store.prefs().language(), "de");
    let Some((label, rows)) = language_submenu(&a.laid_menu()) else { panic!("no Language submenu") };
    assert_eq!(label, "Sprache");
    let rows = language_rows(&rows);
    assert_eq!(rows.first().map(|r| r.0.as_str()), Some("Automatisch"));
    let chosen: Vec<String> = rows.into_iter().filter(|r| r.2).map(|r| r.1).collect();
    assert_eq!(chosen, ["de"]);

    // A code that is not built in, an empty code or no code is refused with an error that lists
    // every code, and changes nothing.
    let codes = std::iter::once(AUTO).chain(languages()).collect::<Vec<_>>().join(", ");
    for cmd in [
        CommandRef::with_params("app.language", json!({"lang": "tlh"})),
        CommandRef::with_params("app.language", json!({"lang": ""})),
        CommandRef::new("app.language"),
    ] {
        let e = a.perform(&ctx, &cmd).unwrap_err();
        assert!(e.contains(&format!("(one of {codes})")), "{e}");
    }
    assert_eq!(a.store.prefs().language(), "de");
    let r = a.perform(&ctx, &CommandRef::with_params("app.language", json!({"lang": "auto"}))).unwrap();
    assert_eq!(r, json!({"language": AUTO}));
    assert_eq!(lang(), I18N.system_lang());
    a.set_language("en");
}

#[test]
fn the_language_submenu_title_has_an_english_label_for_its_mnemonic() {
    let mut a = StudioApp::boot(Store::memory(), None, Vec::new());
    for code in ["en", "ja", "zh-hans", "ko"] {
        a.set_language(code);
        let Some((label, _)) = language_submenu(&a.laid_menu_for(craft_menu::Target::Windows)) else { panic!("no Language submenu in {code}") };
        assert_eq!(crate::commands::english_label(craft_menu::Labeled::Title(&label)), Some("Language".to_string()), "{code}: `{label}`");
    }
    a.set_language("en");
}

#[test]
fn a_language_chosen_from_the_menu_installs_its_fonts() {
    let mut a = StudioApp::boot(Store::memory(), None, Vec::new());
    a.set_language("en");
    let ctx = crate::test_context();
    a.install_fonts(&ctx);
    a.perform(&ctx, &CommandRef::with_params("app.language", json!({"lang": "ja"}))).unwrap();
    shown(&mut a, &ctx);
    let ja = I18N.from_code("ja").unwrap();
    assert_eq!(a.fonts_for, Some(ja));
    assert_eq!(a.host.lang(), ja);
    a.perform(&ctx, &CommandRef::with_params("app.language", json!({"lang": "en"}))).unwrap();
    shown(&mut a, &ctx);
    assert_eq!(a.fonts_for, Some(Lang::EN));
    set_lang(Lang::EN);
}

#[test]
fn the_language_setting_round_trips_and_an_unknown_code_reads_as_auto() {
    let dir = std::env::temp_dir().join(format!("studiocraft-language-{}", std::process::id()));
    let (mut s, problems) = Store::open(Location::Dir(dir.clone()));
    assert!(problems.is_empty(), "{problems:?}");
    assert_eq!(s.prefs().language(), AUTO);
    s.update(|p| p.language = "en".into());
    s.flush().unwrap();
    let (again, _) = Store::open(Location::Dir(dir.clone()));
    assert_eq!(again.prefs().language(), "en");
    drop(again);

    // A code this build does not offer, written by hand or by a build with more languages.
    s.update(|p| p.language = "tlh".into());
    s.flush().unwrap();
    let (again, problems) = Store::open(Location::Dir(dir.clone()));
    assert!(problems.is_empty(), "{problems:?}");
    assert_eq!(again.prefs().language, "tlh");
    assert_eq!(again.prefs().language(), AUTO);
    let _ = std::fs::remove_dir_all(&dir);

    let mut a = StudioApp::boot(Store::memory(), None, Vec::new());
    a.set_language("en");
    assert_eq!(a.hud()["ui"]["language"], json!("en"));
    assert_eq!(lang(), Lang::EN);
    a.set_language("tlh");
    assert_eq!(a.hud()["ui"]["language"], json!(AUTO));
    assert_eq!(lang(), I18N.system_lang());
    assert_eq!(resolve("tlh"), I18N.system_lang());
    assert_eq!(resolve("en"), Lang::EN);
}

#[test]
fn the_settings_window_starts_from_the_saved_language() {
    let mut a = StudioApp::boot(Store::memory(), None, Vec::new());
    a.set_language("en");
    a.open_settings(Tab::General);
    assert_eq!(a.settings.as_ref().map(|w| w.language.as_str()), Some("en"));
    a.settings = None;
    a.store.update(|p| p.language = "tlh".into());
    a.open_settings(Tab::General);
    assert_eq!(a.settings.as_ref().map(|w| w.language.as_str()), Some(AUTO));
}

/// The folder of StudioCraft's own catalogs.
fn tree_locales() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("i18n/locales")
}

/// One catalog row, unescaped.
#[derive(Debug)]
struct Row {
    line: usize,
    context: String,
    source: String,
    translation: String,
}

fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

fn escape(text: &str) -> String {
    text.replace('\\', "\\\\").replace('\n', "\\n").replace('\t', "\\t")
}

/// The rows of the catalog at `path`; a row without three columns fails the test.
fn read_rows(path: &std::path::Path) -> Vec<Row> {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let mut rows = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let columns: Vec<&str> = line.split('\t').collect();
        let [context, source, translation] = columns.as_slice() else {
            panic!("{}:{}: {} columns; expected 3", path.display(), i + 1, columns.len());
        };
        rows.push(Row { line: i + 1, context: unescape(context), source: unescape(source), translation: unescape(translation) });
    }
    rows
}

/// The English source rows: (context, source text).
fn english() -> BTreeSet<(String, String)> {
    read_rows(&tree_locales().join("en/studiocraft.tsv")).into_iter().map(|r| (r.context, r.source)).collect()
}

/// The Rust files whose literals are interface text: this crate's sources and the app's.
fn interface_sources() -> Vec<std::path::PathBuf> {
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") && p.file_name().is_some_and(|n| n != "tests.rs") {
                out.push(p);
            }
        }
    }
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut out = Vec::new();
    walk(&manifest.join("src"), &mut out);
    walk(&manifest.join("../../src"), &mut out);
    out.sort();
    out
}

/// A Rust string literal at the start of `text` (after whitespace): its value and the rest of
/// the text. `None` when `text` starts with anything else.
fn literal(text: &str) -> Option<(String, &str)> {
    let text = text.trim_start();
    if let Some(raw) = text.strip_prefix('r') {
        let hashes = raw.len() - raw.trim_start_matches('#').len();
        let body = raw.get(hashes..)?.strip_prefix('"')?;
        let close = format!("\"{}", "#".repeat(hashes));
        let end = body.find(&close)?;
        return Some((body.get(..end)?.to_string(), body.get(end + close.len()..)?));
    }
    let body = text.strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = body.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '"' => return Some((out, body.get(i + 1..)?)),
            '\\' => match chars.next()?.1 {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                '0' => out.push('\0'),
                'u' => {
                    let hex: String = chars.by_ref().map(|(_, c)| c).skip(1).take_while(|c| *c != '}').collect();
                    out.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
                }
                '\n' => {
                    // A line continuation: the newline and the next line's indent are not part of it.
                    let rest = body.get(i + 2..)?;
                    let indent = rest.len() - rest.trim_start().len();
                    for _ in 0..rest.get(..indent)?.chars().count() {
                        chars.next();
                    }
                }
                other => out.push(other),
            },
            c => out.push(c),
        }
    }
    None
}

/// A `tl!`, `tlc!`, `tid!` or `tmsg!` call with literal text: where it is, and the English rows
/// that serve it (any one of them).
#[derive(Debug)]
struct Use {
    at: String,
    rows: Vec<(String, String)>,
}

/// Every literal looked up by the translation macros in [`interface_sources`]. Comment lines and
/// the `#[cfg(test)]` part of a file are left out, and so is a call whose text is not a literal.
fn literal_uses() -> Vec<Use> {
    let mut uses = Vec::new();
    for path in interface_sources() {
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let code: String = text
            .lines()
            .take_while(|l| *l != "#[cfg(test)]")
            .map(|l| if l.trim_start().starts_with("//") { "" } else { l })
            .collect::<Vec<_>>()
            .join("\n");
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        for (at, _) in code.match_indices("!(") {
            let head = code.get(..at).unwrap_or("");
            let mac = head.rsplit(|c: char| !(c.is_alphanumeric() || c == '_')).next().unwrap_or("");
            let Some(args) = code.get(at + 2..) else { continue };
            let line = head.matches('\n').count() + 1;
            let first = literal(args);
            let second = || first.as_ref().and_then(|(_, rest)| rest.trim_start().strip_prefix(',')).and_then(literal).map(|(s, _)| s);
            let rows = match (mac, &first) {
                ("tl", Some((s, _))) => vec![(String::new(), s.clone())],
                ("tmsg", Some((s, _))) => vec![("@msg".into(), s.clone()), (String::new(), s.clone())],
                ("tlc", Some((c, _))) => match second() {
                    Some(s) => vec![(c.clone(), s.clone()), (String::new(), s)],
                    None => continue,
                },
                ("tid", Some((id, _))) => match second() {
                    Some(label) => vec![("@id".into(), id.clone()), (String::new(), label)],
                    None => continue,
                },
                _ => continue,
            };
            uses.push(Use { at: format!("{name}:{line}"), rows });
        }
    }
    uses
}

#[test]
fn every_literal_the_macros_look_up_has_an_english_row() {
    let uses = literal_uses();
    assert!(uses.len() > 100, "found only {} literals; the scan is broken", uses.len());
    assert!(uses.iter().any(|u| u.rows.first().is_some_and(|r| r.0 == "@msg")), "no tmsg! literal found");
    assert!(uses.iter().any(|u| u.rows.first().is_some_and(|r| r.0 == "menu")), "no tlc! literal found");
    let en = english();
    let missing: Vec<String> = uses
        .iter()
        .filter(|u| !u.rows.iter().any(|r| en.contains(r)))
        .map(|u| {
            // The row to add: the context row, or for a command label its English label.
            let row = if u.rows.first().is_some_and(|r| r.0 == "@id") { u.rows.last() } else { u.rows.first() };
            let (c, s) = row.cloned().unwrap_or_default();
            format!("{}: {c}\t{}\t{}", u.at, escape(&s), escape(&s))
        })
        .collect();
    assert!(missing.is_empty(), "literals with no row in i18n/locales/en/studiocraft.tsv:\n{}", missing.join("\n"));
}

/// Labels that reach `tl!` through a variable: the move items of the Scenes and Sources menus, the
/// recording qualities, the titles of the name prompts, and the other states of the buttons in
/// the Sources and Audio Mixer docks, the routing labels of the Application Audio dock, the titles
/// of the docks' own windows, and the built-in dock layouts' names.
#[test]
fn every_label_chosen_at_run_time_has_an_english_row() {
    use crate::Named;
    use studiocraft_model::RecordingQuality;
    let mut labels: Vec<&str> = crate::docks::MOVES.iter().map(|(_, l)| *l).collect();
    labels.extend(RecordingQuality::ALL.map(|q| crate::settings::quality_card(q).0));
    labels.extend(RecordingQuality::ALL.map(|q| crate::settings::quality_card(q).1));
    labels.extend(studiocraft_model::AudioTracksWhen::ALL.map(crate::settings::when_label));
    labels.extend(studiocraft_model::AfterStop::ALL.map(crate::settings::after_stop_label));
    // The Pause screen choices of the Stream tab.
    labels.extend(studiocraft_model::PauseAtZero::ALL.map(crate::settings::at_zero_label));
    labels.extend(studiocraft_model::PauseAnchor::ALL.map(crate::settings::anchor_label));
    labels.extend(
        [studiocraft_model::FitMode::Fit, studiocraft_model::FitMode::Fill, studiocraft_model::FitMode::Stretch].map(crate::settings::fit_label),
    );
    // The entries of the Pause button's menu.
    labels.extend(crate::output_bar::pause_menu().into_iter().map(|(l, _)| l));
    // The Stream key cards of the Streaming window.
    labels.extend(crate::streaming::KeyChoice::ALL.map(crate::streaming::KeyChoice::label));
    labels.extend(crate::streaming::KeyChoice::ALL.map(crate::streaming::KeyChoice::description));
    labels.extend(
        [
            Named::NewProfile,
            Named::DuplicateProfile,
            Named::RenameProfile,
            Named::NewCollection,
            Named::DuplicateCollection,
            Named::RenameCollection,
            Named::SaveDockLayout,
            Named::RenameSource(studiocraft_model::SourceId(0)),
            Named::RenameScene(studiocraft_model::SceneId(0)),
        ]
        .map(Named::title),
    );
    labels.extend(["Hide", "Show", "Lock (it can't be moved in the preview)", "Unlock", "Mute", "Unmute"]);
    labels.extend(crate::docks::ROUTE_ITEMS);
    labels.extend(crate::docks::ROUTE_TIPS);
    labels.extend(crate::control::DOCK_TITLES);
    // The built-in dock layouts' names in Docks › Layouts.
    labels.extend(crate::store::BUILT_IN_LAYOUTS);
    // The tooltips of the Scenes and Sources docks' header buttons.
    labels.extend(crate::commands::SCENE_HEADER.iter().chain(&crate::commands::SOURCE_HEADER).map(|(_, _, _, tip)| *tip));
    labels.push(crate::commands::SCENE_LIST_VIEW.1);
    let en = english();
    let missing: Vec<String> = labels.iter().filter(|l| !en.contains(&(String::new(), l.to_string()))).map(|l| format!("\t{l}\t{l}")).collect();
    assert!(missing.is_empty(), "labels with no row in i18n/locales/en/studiocraft.tsv:\n{}", missing.join("\n"));
}

/// The settings that list what the system has (devices, a camera's formats, displays, windows,
/// the slots of a Staged windows source, running applications): their option labels are names
/// from the system, apart from the fixed first entries, which have their own rows.
const SYSTEM_LISTS: &[&str] = &["device", "format", "display", "window", "window1", "window2", "window3", "window4", "window5", "window6", "app"];

#[test]
fn every_plug_in_name_and_setting_label_has_an_english_row() {
    use studiocraft_model::FieldKind;
    use studiocraft_plugin::Plugin;
    let a = StudioApp::boot(Store::memory(), None, Vec::new());
    let reg = a.studio().registry();
    let mut plugins: Vec<&dyn Plugin> = Vec::new();
    plugins.extend(reg.sources().iter().map(|p| p.as_ref() as &dyn Plugin));
    plugins.extend(reg.filters().iter().map(|p| p.as_ref() as &dyn Plugin));
    plugins.extend(reg.transitions().iter().map(|p| p.as_ref() as &dyn Plugin));
    plugins.extend(reg.encoders().iter().map(|p| p.as_ref() as &dyn Plugin));
    plugins.extend(reg.outputs().iter().map(|p| p.as_ref() as &dyn Plugin));
    assert!(plugins.len() > 10, "{} plug-ins", plugins.len());
    let mut texts = BTreeSet::new();
    for p in plugins {
        texts.insert(p.name().to_string());
        for f in p.schema().fields {
            texts.insert(f.label.clone());
            match &f.kind {
                FieldKind::Choice { options } if !SYSTEM_LISTS.contains(&f.key.as_str()) => texts.extend(options.iter().map(|o| o.label.clone())),
                FieldKind::Path { filter_name, .. } if !filter_name.is_empty() => {
                    texts.insert(filter_name.clone());
                }
                _ => {}
            }
        }
    }
    let sources: BTreeSet<String> = english().into_iter().map(|(_, s)| s).collect();
    let missing: Vec<String> = texts.iter().filter(|t| !sources.contains(*t)).map(|t| format!("\t{}\t{}", escape(t), escape(t))).collect();
    assert!(missing.is_empty(), "plug-in text with no row in i18n/locales/en/studiocraft.tsv:\n{}", missing.join("\n"));
}

#[test]
fn the_literal_scan_reads_rust_string_syntax() {
    assert_eq!(literal(r#"  "a \"b\" {n}\u{2026}", x"#), Some(("a \"b\" {n}\u{2026}".to_string(), ", x")));
    assert_eq!(literal(r##"r#"raw "x""#)"##), Some(("raw \"x\"".to_string(), ")")));
    assert_eq!(literal("\"one \\\n        two\")"), Some(("one two".to_string(), ")")));
    assert_eq!(literal("label)"), None);
    assert_eq!(literal("\"unclosed"), None);
}

/// `{name}` placeholders, sorted.
fn placeholders(text: &str) -> Vec<&str> {
    let mut names: Vec<&str> = text.split('{').skip(1).filter_map(|p| p.split_once('}').map(|(n, _)| n)).collect();
    names.sort_unstable();
    names
}

#[test]
fn every_locale_keeps_the_english_placeholders_and_rows() {
    let en_rows = read_rows(&tree_locales().join("en/studiocraft.tsv"));
    let mut problems = Vec::new();
    for r in &en_rows {
        if r.context != "@id" && r.translation != r.source {
            problems.push(format!("en:{}: the English row's text differs from its source", r.line));
        }
    }
    let en: std::collections::BTreeMap<(String, String), &Row> = en_rows.iter().map(|r| ((r.context.clone(), r.source.clone()), r)).collect();
    let mut locales = 0;
    for dir in std::fs::read_dir(tree_locales()).unwrap().flatten().map(|e| e.path()).filter(|p| p.is_dir()) {
        let code = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if code == "en" {
            continue;
        }
        locales += 1;
        for r in read_rows(&dir.join("studiocraft.tsv")) {
            let at = format!("{code}:{}", r.line);
            if r.context == "@id" && r.source == "language.name" {
                continue;
            }
            let Some(source) = en.get(&(r.context.clone(), r.source.clone())).or_else(|| en.get(&(String::new(), r.source.clone()))) else {
                problems.push(format!("{at}: `{}` has no English row", r.source));
                continue;
            };
            let forms: Vec<&str> = if r.context == "@plural" { r.translation.split('|').collect() } else { vec![r.translation.as_str()] };
            let wanted = placeholders(source.translation.split('|').next_back().unwrap_or(""));
            for form in forms {
                if placeholders(form) != wanted {
                    problems.push(format!("{at}: placeholders {:?}, English has {wanted:?}", placeholders(form)));
                }
            }
            if source.translation.ends_with('…') != r.translation.ends_with('…') {
                problems.push(format!("{at}: the trailing … differs from the English"));
            }
        }
    }
    assert!(locales >= 1, "no locale besides en in {}", tree_locales().display());
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn the_placeholder_check_catches_a_renamed_or_dropped_value() {
    assert_eq!(placeholders("{b} and {a}"), ["a", "b"]);
    assert_ne!(placeholders("Szene {nr}"), placeholders("Scene {n}"));
    assert_ne!(placeholders("Szene"), placeholders("Scene {n}"));
}

#[test]
fn the_catalogs_in_the_tree_are_offered_and_switch_the_menus() {
    for dir in std::fs::read_dir(tree_locales()).unwrap().flatten().map(|e| e.path()).filter(|p| p.join("studiocraft.tsv").is_file()) {
        let code = dir.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
        if code != "en" {
            assert!(languages().contains(&code.as_str()), "{code} is in the tree but not offered: {:?}", languages());
        }
    }
    let mut a = StudioApp::boot(Store::memory(), None, Vec::new());
    for (code, name, edit) in [
        ("ar", "العربية", "تحرير"),
        ("cs", "Čeština", "Úpravy"),
        ("de", "Deutsch", "Bearbeiten"),
        ("es", "Español", "Editar"),
        ("fr", "Français", "Édition"),
        ("he", "עברית", "עריכה"),
        ("it", "Italiano", "Modifica"),
        ("ja", "日本語", "編集"),
        ("ko", "한국어", "편집"),
        ("nl", "Nederlands", "Bewerken"),
        ("pl", "Polski", "Edycja"),
        ("pt-br", "Português (Brasil)", "Editar"),
        ("ru", "Русский", "Правка"),
        ("sv", "Svenska", "Redigera"),
        ("tr", "Türkçe", "Düzen"),
        ("uk", "Українська", "Редагування"),
        ("zh-hans", "简体中文", "编辑"),
        ("zh-hant", "繁體中文", "編輯"),
    ] {
        let l = I18N.from_code(code).unwrap_or_else(|| panic!("{code} is embedded"));
        assert_eq!(I18N.name(l), name);
        a.set_language(code);
        assert_eq!(lang(), l);
        let titles: Vec<String> = a.laid_menu().menus.iter().map(|m| m.title.clone()).collect();
        assert!(titles.iter().any(|t| t == edit), "{code}: {titles:?}");
        assert!(!titles.iter().any(|t| t == "Edit"), "{code}: {titles:?}");
    }
    a.set_language("en");
    let titles: Vec<String> = a.laid_menu().menus.iter().map(|m| m.title.clone()).collect();
    assert!(titles.iter().any(|t| t == "Edit"), "{titles:?}");
}

#[test]
fn command_labels_use_the_command_id_only_for_the_commands_own_label() {
    set_lang(Lang::PSEUDO);
    // Own labels and short menu labels both come back marked, so both were looked up.
    assert_eq!(command_label("app.about", "About StudioCraft"), "\u{27e6}About StudioCraft\u{27e7}");
    assert_eq!(command_label("profile.new", "New…"), "\u{27e6}New…\u{27e7}");
    set_lang(Lang::EN);
    assert_eq!(command_label("edit.undo", "Undo"), "Undo");
    assert_eq!(command_label("profile.new", "New…"), "New…");
}
