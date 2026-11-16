//! Turns `contributors/people.toml` and the optional `contributors/contributors.json` into the
//! Rust source of the contributor table.
//!
//! `build.rs` includes this file and writes the result to `OUT_DIR/credits.rs`. The crate also
//! includes it in test builds, where the tests at the end check it.
//!
//! - Each person is listed once, by GitHub username, whether the username comes from
//!   `contributors.json` (`contributors[].login`) or from a `[people.<username>]` entry.
//! - Names come only from a person's own `[people.<username>]` entry: `real_name`,
//!   `display_name`, or with `sync_github_name = true` the `github_name` that
//!   `contributors.json` records for that username.
//! - Unknown keys are ignored. A file that cannot be parsed gives a warning and is skipped.

use std::collections::BTreeMap;
use std::fmt::Write as _;

/// The longest name kept, in characters.
pub const MAX_NAME: usize = 100;
/// The most people listed. Further entries are dropped with a warning.
pub const MAX_ROWS: usize = 10_000;

/// One person in the generated table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub login: String,
    pub real_name: Option<String>,
    pub display_name: Option<String>,
}

/// Whether `login` has the shape of a GitHub username: 1 to 39 ASCII letters, digits and
/// hyphens.
fn valid_login(login: &str) -> bool {
    (1..=39).contains(&login.len()) && login.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// A name field: trimmed, non-empty, at most [`MAX_NAME`] characters.
fn clean_name(v: Option<&str>) -> Option<String> {
    let s = v?.trim();
    if s.is_empty() { None } else { Some(s.chars().take(MAX_NAME).collect::<String>().trim_end().to_string()) }
}

/// The people listed in the two files, sorted by username without case.
pub fn rows(people: Option<&str>, json: Option<&str>, warnings: &mut Vec<String>) -> Vec<Row> {
    let table = match people.map(str::parse::<toml::Table>) {
        None => toml::Table::new(),
        Some(Ok(t)) => t,
        Some(Err(e)) => {
            warnings.push(format!("contributors/people.toml could not be read: {e}; contributors are listed without names"));
            toml::Table::new()
        }
    };
    let entries = match table.get("people") {
        None => toml::Table::new(),
        Some(toml::Value::Table(t)) => t.clone(),
        Some(_) => {
            warnings.push("contributors/people.toml: `people` is not a table; contributors are listed without names".into());
            toml::Table::new()
        }
    };
    let stats = match json.map(serde_json::from_str::<serde_json::Value>) {
        None => serde_json::Value::Null,
        Some(Ok(v)) => v,
        Some(Err(e)) => {
            warnings.push(format!("contributors/contributors.json could not be read: {e}"));
            serde_json::Value::Null
        }
    };

    // Lowercase username -> (username as first written, name on the GitHub profile).
    let mut people: BTreeMap<String, (String, Option<String>)> = BTreeMap::new();
    let mut add = |login: &str, github_name: Option<&str>, warnings: &mut Vec<String>| {
        if !valid_login(login) {
            warnings.push(format!("contributors: `{}` is not a GitHub username; skipped", login.escape_debug()));
            return;
        }
        let key = login.to_ascii_lowercase();
        if !people.contains_key(&key) && people.len() >= MAX_ROWS {
            warnings.push(format!("contributors: more than {MAX_ROWS} people; `{login}` is not listed"));
            return;
        }
        let slot = people.entry(key).or_insert_with(|| (login.to_string(), None));
        if slot.1.is_none() {
            slot.1 = clean_name(github_name);
        }
    };
    let listed = stats.get("contributors").and_then(serde_json::Value::as_array).map(Vec::as_slice).unwrap_or_default();
    for c in listed {
        if let Some(login) = c.get("login").and_then(serde_json::Value::as_str) {
            add(login, c.get("github_name").and_then(serde_json::Value::as_str), warnings);
        }
    }
    for login in entries.keys() {
        add(login, None, warnings);
    }

    people
        .into_iter()
        .map(|(key, (login, github_name))| {
            let entry = entries.iter().find(|(k, _)| k.to_ascii_lowercase() == key).and_then(|(_, v)| v.as_table());
            let field = |k: &str| entry.and_then(|e| e.get(k)).and_then(toml::Value::as_str);
            let synced = entry.and_then(|e| e.get("sync_github_name")).and_then(toml::Value::as_bool) == Some(true);
            let display_name = clean_name(field("display_name")).or(if synced { github_name } else { None });
            Row { login, real_name: clean_name(field("real_name")), display_name }
        })
        .collect()
}

/// The Rust source of `pub static CONTRIBUTORS: &[Contributor]` for `rows`.
pub fn render(rows: &[Row]) -> String {
    let opt = |v: &Option<String>| match v {
        Some(s) => format!("Some({s:?})"),
        None => "None".to_string(),
    };
    let mut o = String::from("pub static CONTRIBUTORS: &[Contributor] = &[\n");
    for r in rows {
        let _ = writeln!(o, "    Contributor {{ login: {:?}, real_name: {}, display_name: {} }},", r.login, opt(&r.real_name), opt(&r.display_name));
    }
    o.push_str("];\n");
    o
}

/// The generated source and the warnings for the two files' text (`None`: the file is absent).
pub fn generate(people: Option<&str>, json: Option<&str>) -> (String, Vec<String>) {
    let mut warnings = Vec::new();
    let rows = rows(people, json, &mut warnings);
    (render(&rows), warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(login: &str, real: Option<&str>, display: Option<&str>) -> Row {
        Row { login: login.into(), real_name: real.map(Into::into), display_name: display.map(Into::into) }
    }

    #[test]
    fn people_entries_give_names_and_unknown_keys_are_ignored() {
        let people = r#"
            [people]
            [people.zed]
            real_name = "Zed Example"
            favorite_color = "green"
            [people.amy]
            display_name = "  Amy  "
            real_name = ""
            [people.Bob-2]
        "#;
        let mut w = Vec::new();
        let rows = rows(Some(people), None, &mut w);
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(rows, [row("amy", None, Some("Amy")), row("Bob-2", None, None), row("zed", Some("Zed Example"), None)]);
    }

    #[test]
    fn json_logins_are_listed_and_names_come_only_from_an_entry() {
        let json = r#"{"commits": 3, "contributors": [
            {"login": "carol", "github_name": "Carol GitHub"},
            {"login": "dave", "github_name": "Dave GitHub"},
            {"login": "Eve"},
            {"login": "bad name"},
            {"nologin": true}
        ]}"#;
        let people = "[people.carol]\nsync_github_name = true\n[people.eve]\nreal_name = \"Eve R\"\n";
        let mut w = Vec::new();
        let rows = rows(Some(people), Some(json), &mut w);
        assert_eq!(rows, [row("carol", None, Some("Carol GitHub")), row("dave", None, None), row("Eve", Some("Eve R"), None)]);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("bad name"));
    }

    #[test]
    fn missing_or_malformed_files_give_an_empty_table_and_warnings() {
        let (src, w) = generate(None, None);
        assert!(w.is_empty());
        assert_eq!(src, "pub static CONTRIBUTORS: &[Contributor] = &[\n];\n");
        let (src, w) = generate(Some("[people\nbroken"), Some("{"));
        assert_eq!(w.len(), 2, "{w:?}");
        assert_eq!(src, "pub static CONTRIBUTORS: &[Contributor] = &[\n];\n");
        let (_, w) = generate(Some("people = 5"), None);
        assert_eq!(w.len(), 1, "{w:?}");
        // The template file with no entries.
        let (src, w) = generate(Some("# Names for the About window.\n\n[people]\n"), None);
        assert!(w.is_empty());
        assert!(!src.contains("login"));
    }

    #[test]
    fn rendered_names_are_escaped_rust_literals() {
        let src = render(&[row("x", Some("Quote \" and \\ back"), Some("Line\nbreak"))]);
        assert!(src.contains(r#"Contributor { login: "x", real_name: Some("Quote \" and \\ back"), display_name: Some("Line\nbreak") },"#), "{src}");
    }

    #[test]
    fn long_names_are_cut() {
        let people = format!("[people.long]\nreal_name = \"{}\"\n", "a".repeat(500));
        let rows = rows(Some(&people), None, &mut Vec::new());
        assert_eq!(rows[0].real_name.as_deref().map(str::len), Some(MAX_NAME));
    }
}
