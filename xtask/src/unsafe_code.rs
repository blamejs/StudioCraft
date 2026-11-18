//! The `unsafe` allowlist (AGENTS.md › Never crash, rule 3): the workspace forbids `unsafe_code`,
//! and only the crates in [`ALLOWED`] may set their own lints instead. Each of those must deny
//! `unsafe_code`, `unsafe_op_in_unsafe_fn` and `clippy::undocumented_unsafe_blocks`, copy every
//! workspace clippy lint, and deny `unsafe_op_in_unsafe_fn` at the top of its `src/lib.rs`. Its
//! `allow(unsafe_code)` attributes must sit on `mod` items, and it must have no public `unsafe fn`
//! or `unsafe trait`. Every other crate, and an allowed crate that keeps the workspace lints, must
//! use the workspace lints, and its Rust files must hold no `unsafe` keyword and no `allow`,
//! `warn` or `expect` of `unsafe_code`.

use std::collections::BTreeMap;
use std::path::Path;

/// The only crates that may call native hardware APIs (hardware video encoding, GPU texture
/// sharing for capture, and hardware video decoding), each with the `Cargo.toml` it must have.
pub const ALLOWED: &[(&str, &str)] = &[
    ("craft-hwenc-native", "crates/craft-hwenc-native/Cargo.toml"),
    ("craft-gpu-interop", "crates/craft-gpu-interop/Cargo.toml"),
    ("craft-hwdec", "crates/craft-hwdec/Cargo.toml"),
];

fn allowed(k: &Crate) -> bool {
    ALLOWED.iter().any(|&(name, manifest)| k.name == name && k.manifest_path == manifest)
}

fn allowed_names() -> String {
    ALLOWED.iter().map(|&(name, _)| name).collect::<Vec<_>>().join(", ")
}

/// The `[lints.rust]` entries an allowed crate must have.
const ALLOWED_RUST_LINTS: &[(&str, &str)] = &[("unsafe_code", "\"deny\""), ("unsafe_op_in_unsafe_fn", "\"deny\"")];

/// The crate attribute an allowed crate's `src/lib.rs` must carry.
const LIB_ATTRIBUTE: &str = "#![deny(unsafe_op_in_unsafe_fn)]";

/// A TOML file read as `section -> key -> value`, enough for `Cargo.toml` lint tables: each
/// `[section]` header starts a table, and each `key = value` line inside it is one entry.
fn tables(toml: &str) -> BTreeMap<String, BTreeMap<String, String>> {
    let mut out: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut section = String::new();
    for line in toml.lines() {
        let line = line.split(" #").next().unwrap_or(line).trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = name.trim().to_string();
            out.entry(section.clone()).or_default();
        } else if let Some((key, value)) = line.split_once('=') {
            out.entry(section.clone()).or_default().insert(key.trim().to_string(), value.trim().to_string());
        }
    }
    out
}

/// `source` with comments, string literals and character literals replaced by spaces. Newlines
/// stay, so line numbers do not move.
fn code_only(source: &str) -> String {
    let c: Vec<char> = source.chars().collect();
    let mut out = String::with_capacity(source.len());
    let blank = |out: &mut String, ch: char| out.push(if ch == '\n' { '\n' } else { ' ' });
    let mut i = 0;
    while i < c.len() {
        let next = c.get(i + 1).copied();
        if c[i] == '/' && next == Some('/') {
            while i < c.len() && c[i] != '\n' {
                blank(&mut out, c[i]);
                i += 1;
            }
        } else if c[i] == '/' && next == Some('*') {
            let mut depth = 0usize;
            while i < c.len() {
                if c[i] == '/' && c.get(i + 1) == Some(&'*') {
                    depth += 1;
                    out.push_str("  ");
                    i += 2;
                } else if c[i] == '*' && c.get(i + 1) == Some(&'/') {
                    depth = depth.saturating_sub(1);
                    out.push_str("  ");
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    blank(&mut out, c[i]);
                    i += 1;
                }
            }
        } else if let Some(len) = raw_string_len(&c, i) {
            for ch in &c[i..i + len] {
                blank(&mut out, *ch);
            }
            i += len;
        } else if c[i] == '"' {
            blank(&mut out, c[i]);
            i += 1;
            while i < c.len() && c[i] != '"' {
                if c[i] == '\\' && i + 1 < c.len() {
                    blank(&mut out, c[i]);
                    i += 1;
                }
                blank(&mut out, c[i]);
                i += 1;
            }
            if i < c.len() {
                out.push(' ');
                i += 1;
            }
        } else if c[i] == '\'' {
            // A character literal is 'x' or an escape such as '\n' or '\u{1F600}'; anything
            // else is a lifetime or a label, which stays.
            let len = if next == Some('\\') {
                c.get(i + 3..).and_then(|rest| rest.iter().position(|&ch| ch == '\'')).map(|p| p + 4)
            } else if c.get(i + 2) == Some(&'\'') {
                Some(3)
            } else {
                None
            };
            match len {
                Some(len) if i + len <= c.len() => {
                    for ch in &c[i..i + len] {
                        blank(&mut out, *ch);
                    }
                    i += len;
                }
                _ => {
                    out.push('\'');
                    i += 1;
                }
            }
        } else {
            out.push(c[i]);
            i += 1;
        }
    }
    out
}

fn ident_char(c: Option<char>) -> bool {
    c.is_some_and(|c| c.is_alphanumeric() || c == '_')
}

/// The length of a raw string literal (`r"…"`, `r#"…"#`, `br"…"`, `cr"…"`) starting at `i`.
fn raw_string_len(c: &[char], i: usize) -> Option<usize> {
    let start = match (c.get(i), c.get(i + 1)) {
        (Some('r'), _) => i + 1,
        (Some('b' | 'c'), Some('r')) => i + 2,
        _ => return None,
    };
    if ident_char(i.checked_sub(1).and_then(|p| c.get(p)).copied()) {
        return None;
    }
    let hashes = c.get(start..)?.iter().take_while(|&&ch| ch == '#').count();
    if c.get(start + hashes) != Some(&'"') {
        return None;
    }
    let closes = |j: usize| c.get(j) == Some(&'"') && c.get(j + 1..j + 1 + hashes).is_some_and(|h| h.iter().all(|&ch| ch == '#'));
    let end = (start + hashes + 1..c.len()).find(|&j| closes(j)).map_or(c.len(), |j| j + 1 + hashes);
    Some(end - i)
}

/// Each place in `source` that uses `unsafe` or relaxes `unsafe_code`, as `(line, what)`.
pub fn unsafe_uses(source: &str) -> Vec<(usize, &'static str)> {
    let code = code_only(source);
    let line_of = |at: usize| code[..at].matches('\n').count() + 1;
    let mut hits = Vec::new();
    for (at, word) in words(&code) {
        if word == "unsafe" {
            hits.push((line_of(at), "`unsafe`"));
        } else if word == "unsafe_code" && relaxes(&code, at) {
            hits.push((line_of(at), "a relaxed `unsafe_code` lint"));
        }
    }
    hits
}

/// Whether the `unsafe_code` at byte `at` of `code` is inside an `allow`, `warn` or `expect`
/// attribute, which may span several lines.
fn relaxes(code: &str, at: usize) -> bool {
    let before = &code[..at];
    let level = before.rfind('(').map(|p| before[..p].trim_end()).and_then(|b| words(b).last().map(|&(_, w)| w));
    matches!(level, Some("allow" | "warn" | "expect"))
}

/// Whether `name` names an FFI module: `ffi`, or a name ending in `_ffi`.
fn ffi_name(name: &str) -> bool {
    name == "ffi" || name.ends_with("_ffi")
}

/// Whether the Rust file at `path` belongs to an FFI module: its file name or one of its folders
/// below `src/` is an FFI module name.
pub fn ffi_file(path: &str) -> bool {
    let below_src = path.split_once("/src/").map_or(path, |(_, rest)| rest);
    below_src.split('/').any(|part| ffi_name(part.strip_suffix(".rs").unwrap_or(part)))
}

/// Each place in an allowed crate's file at `path` that breaks the FFI boundary, as
/// `(line, what)`. An `allow(unsafe_code)` must be an outer attribute of a private
/// `mod <ffi name>;` declaration, so the unsafe code is in that module's own file. A file outside
/// the FFI modules holds no `unsafe` keyword and no raw pointer type, so the crate's public API is
/// safe.
pub fn boundary_problems(path: &str, source: &str) -> Vec<(usize, &'static str)> {
    let code = code_only(source);
    let line_of = |at: usize| code[..at].matches('\n').count() + 1;
    let ffi = ffi_file(path);
    let mut hits = Vec::new();
    for (at, word) in words(&code) {
        if word == "unsafe" && !ffi {
            hits.push((line_of(at), "`unsafe` outside an FFI module (`ffi` or `<name>_ffi`)"));
        } else if word == "unsafe_code" {
            if relaxes(&code, at) && !on_an_ffi_mod(&code, at) {
                hits.push((line_of(at), "`allow(unsafe_code)` that is not on a private `mod ffi;` or `mod <name>_ffi;` declaration"));
            }
        } else if matches!(word, "unsafe_op_in_unsafe_fn" | "undocumented_unsafe_blocks") && relaxes(&code, at) {
            hits.push((line_of(at), "a relaxed `unsafe_op_in_unsafe_fn` or `undocumented_unsafe_blocks` lint"));
        } else if word == "use" && !ffi && reexports_ffi(&code, at) {
            hits.push((line_of(at), "a public re-export from an FFI module"));
        } else if matches!(word, "const" | "mut") && !ffi && code[..at].trim_end().ends_with('*') {
            hits.push((line_of(at), "a raw pointer outside an FFI module (`ffi` or `<name>_ffi`)"));
        }
    }
    hits
}

/// Whether the `use` at byte `at` of `code` is a bare `pub use` whose path names an FFI module.
fn reexports_ffi(code: &str, at: usize) -> bool {
    let before = code[..at].trim_end();
    if !before.ends_with("pub") || ident_char(before[..before.len() - 3].chars().next_back()) {
        return false;
    }
    let tree = code[at..].split(';').next().unwrap_or("");
    words(tree).iter().any(|&(_, w)| ffi_name(w))
}

/// Whether the lint attribute around byte `at` of `code` is an outer attribute (`#[…]`) of a
/// private `mod <ffi name>;` declaration.
fn on_an_ffi_mod(code: &str, at: usize) -> bool {
    let Some(hash) = code[..at].rfind('#') else { return false };
    let after_hash = code[hash + 1..].trim_start();
    if !after_hash.starts_with('[') {
        return false;
    }
    let mut rest = after_hash;
    // Skip this attribute and any that follow it.
    while let Some(body) = rest.strip_prefix('[') {
        let mut depth = 1usize;
        let mut end = None;
        for (i, ch) in body.char_indices() {
            match ch {
                '[' => depth += 1,
                ']' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(end) = end else { return false };
        rest = body[end + 1..].trim_start();
        match rest.strip_prefix('#') {
            Some(r) if r.trim_start().starts_with('[') => rest = r.trim_start(),
            _ => break,
        }
    }
    // `pub(crate)` and `pub(super)` keep the module inside the crate; a bare `pub` does not.
    if let Some(after_pub) = rest.strip_prefix("pub") {
        let Some(inner) = after_pub.trim_start().strip_prefix('(') else { return false };
        let Some(close) = inner.find(')') else { return false };
        rest = inner[close + 1..].trim_start();
    }
    let Some(after_mod) = rest.strip_prefix("mod") else { return false };
    let Some(&(start, name)) = words(after_mod).first() else { return false };
    after_mod[..start].trim().is_empty() && ffi_name(name) && after_mod[start + name.len()..].trim_start().starts_with(';')
}

/// The identifiers in `line`, with their byte offsets.
fn words(line: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, ch) in line.char_indices() {
        if ch.is_alphanumeric() || ch == '_' {
            start.get_or_insert(i);
        } else if let Some(s) = start.take() {
            out.push((s, &line[s..i]));
        }
    }
    if let Some(s) = start {
        out.push((s, &line[s..]));
    }
    out
}

/// One crate as the check sees it: its package name, its `Cargo.toml`, and its Rust files
/// (path and text) relative to the workspace root.
pub struct Crate {
    pub name: String,
    pub manifest_path: String,
    pub manifest: String,
    pub sources: Vec<(String, String)>,
}

/// The rule violations among `crates`, given the workspace root's `Cargo.toml`.
pub fn check(workspace_manifest: &str, crates: &[Crate]) -> Vec<String> {
    let mut problems = Vec::new();
    let workspace = tables(workspace_manifest);
    let rust = workspace.get("workspace.lints.rust");
    if rust.and_then(|t| t.get("unsafe_code")).map(String::as_str) != Some("\"forbid\"") {
        problems.push("Cargo.toml: [workspace.lints.rust] must set unsafe_code = \"forbid\"".to_string());
    }
    let clippy = workspace.get("workspace.lints.clippy").cloned().unwrap_or_default();
    for k in crates {
        let t = tables(&k.manifest);
        let uses_workspace = t.get("lints").and_then(|l| l.get("workspace")).map(String::as_str) == Some("true");
        // An allowed crate that keeps the workspace lints is checked like any other crate.
        if allowed(k) && !uses_workspace {
            let own_rust = t.get("lints.rust").cloned().unwrap_or_default();
            for (lint, level) in ALLOWED_RUST_LINTS {
                if own_rust.get(*lint).map(String::as_str) != Some(*level) {
                    problems.push(format!("{}: [lints.rust] must set {lint} = {level}", k.manifest_path));
                }
            }
            for (lint, level) in rust.into_iter().flatten() {
                if !ALLOWED_RUST_LINTS.iter().any(|(l, _)| l == lint) && own_rust.get(lint) != Some(level) {
                    problems.push(format!("{}: [lints.rust] must copy the workspace's {lint} = {level}", k.manifest_path));
                }
            }
            let own_clippy = t.get("lints.clippy").cloned().unwrap_or_default();
            if own_clippy.get("undocumented_unsafe_blocks").map(String::as_str) != Some("\"deny\"") {
                problems.push(format!("{}: [lints.clippy] must set undocumented_unsafe_blocks = \"deny\"", k.manifest_path));
            }
            for (lint, level) in &clippy {
                if own_clippy.get(lint) != Some(level) {
                    problems.push(format!("{}: [lints.clippy] must copy the workspace's {lint} = {level}", k.manifest_path));
                }
            }
            let lib = k.sources.iter().find(|(p, _)| p.ends_with("src/lib.rs"));
            if !lib.is_some_and(|(_, text)| text.lines().any(|l| l.trim() == LIB_ATTRIBUTE)) {
                problems.push(format!("{}: src/lib.rs must start with {LIB_ATTRIBUTE}", k.name));
            }
            for (path, text) in &k.sources {
                for (line, what) in boundary_problems(path, text) {
                    problems.push(format!("{path}:{line}: {what}"));
                }
            }
            continue;
        }
        if !uses_workspace {
            problems.push(format!(
                "{}: must use the workspace lints ([lints] workspace = true); only {} may set their own",
                k.manifest_path,
                allowed_names()
            ));
        }
        for (path, text) in &k.sources {
            for (line, what) in unsafe_uses(text) {
                problems.push(format!("{path}:{line}: {what} outside the allowed crates ({})", allowed_names()));
            }
        }
    }
    problems
}

/// The package name in a `Cargo.toml`, if it has a `[package]` table.
fn package_name(manifest: &str) -> Option<String> {
    tables(manifest).get("package")?.get("name").map(|n| n.trim_matches('"').to_string())
}

/// Every crate under `root`: each Rust file belongs to the crate whose `Cargo.toml` is in the
/// nearest folder above it.
pub fn crates(root: &Path) -> Result<Vec<Crate>, String> {
    let files = crate::scan::try_files(root)?;
    let mut out: Vec<Crate> = Vec::new();
    for f in files.iter().filter(|f| *f == "Cargo.toml" || f.ends_with("/Cargo.toml")) {
        let manifest = std::fs::read_to_string(root.join(f)).map_err(|e| format!("{f}: {e}"))?;
        if let Some(name) = package_name(&manifest) {
            out.push(Crate { name, manifest_path: f.clone(), manifest, sources: Vec::new() });
        }
    }
    let dir = |manifest_path: &str| manifest_path.strip_suffix("Cargo.toml").unwrap_or("").to_string();
    for f in files.iter().filter(|f| f.ends_with(".rs")) {
        let owner = out.iter_mut().filter(|k| f.starts_with(&dir(&k.manifest_path))).max_by_key(|k| k.manifest_path.len());
        if let Some(k) = owner {
            let text = std::fs::read_to_string(root.join(f)).map_err(|e| format!("{f}: {e}"))?;
            k.sources.push((f.clone(), text));
        }
    }
    Ok(out)
}

pub fn run(root: &Path) -> Result<(), String> {
    eprintln!("$ unsafe");
    let workspace = std::fs::read_to_string(root.join("Cargo.toml")).map_err(|e| format!("Cargo.toml: {e}"))?;
    let problems = check(&workspace, &crates(root)?);
    if problems.is_empty() { Ok(()) } else { Err(format!("unsafe code:\n  {}", problems.join("\n  "))) }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORKSPACE: &str =
        "[workspace.lints.rust]\nunsafe_code = \"forbid\"\n\n[workspace.lints.clippy]\nunwrap_used = \"deny\"\npanic = \"deny\"\n";

    fn krate(name: &str, manifest: &str, lib: &str) -> Crate {
        Crate {
            name: name.into(),
            manifest_path: format!("crates/{name}/Cargo.toml"),
            manifest: format!("[package]\nname = \"{name}\"\n\n{manifest}"),
            sources: vec![(format!("crates/{name}/src/lib.rs"), lib.into())],
        }
    }

    const WORKSPACE_LINTS: &str = "[lints]\nworkspace = true\n";

    const OPTED_IN: &str = "[lints.rust]\nunsafe_code = \"deny\"\nunsafe_op_in_unsafe_fn = \"deny\"\n\n[lints.clippy]\nunwrap_used = \"deny\"\npanic = \"deny\"\nundocumented_unsafe_blocks = \"deny\"\n";

    const OPTED_IN_LIB: &str = "#![deny(unsafe_op_in_unsafe_fn)]\n\n#[allow(unsafe_code)]\nmod ffi;\n\npub fn f() {\n    ffi::f()\n}\n";

    const OPTED_IN_FFI: &str = "pub unsafe extern \"C\" fn g(p: *mut u8) {}\n\npub fn f() {\n    // SAFETY: a null pointer is never read.\n    unsafe { g(std::ptr::null_mut()) }\n}\n";

    /// An allowed crate with its own lints, `src/lib.rs` = `lib` and `src/ffi.rs` = `ffi`.
    fn opted_in(lib: &str, ffi: &str) -> Crate {
        let mut k = krate("craft-hwenc-native", OPTED_IN, lib);
        k.sources.push(("crates/craft-hwenc-native/src/ffi.rs".into(), ffi.into()));
        k
    }

    #[test]
    fn a_crate_outside_the_list_may_not_use_unsafe() {
        let problems = check(WORKSPACE, &[krate("studiocraft-render", WORKSPACE_LINTS, "fn f() {\n    unsafe { g() }\n}\n")]);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].starts_with("crates/studiocraft-render/src/lib.rs:2: `unsafe`"), "{problems:?}");
    }

    #[test]
    fn a_crate_outside_the_list_may_not_relax_the_lint() {
        for lib in [
            "#[allow(unsafe_code)]\nmod m {}\n",
            "#![cfg_attr(test, warn(dead_code, unsafe_code))]\n",
            "#[expect(unsafe_code)]\nfn f() {}\n",
            "#[allow(\n    unsafe_code\n)]\nmod m {}\n",
        ] {
            assert_eq!(check(WORKSPACE, &[krate("studiocraft-render", WORKSPACE_LINTS, lib)]).len(), 1, "{lib}");
        }
        let own = krate("studiocraft-render", "[lints.rust]\nunsafe_code = \"deny\"\n", "");
        let problems = check(WORKSPACE, &[own]);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("must use the workspace lints"), "{problems:?}");
    }

    #[test]
    fn forbidding_and_mentioning_unsafe_is_not_a_use() {
        let lib = "#![forbid(unsafe_code)]\n#![deny(unsafe_code)]\n// unsafe { here }\n/* unsafe /* nested */ unsafe */\nconst A: &str = \"unsafe { x }\";\nconst B: &str = r#\"allow(unsafe_code) \"unsafe\" \"#;\nconst C: char = '\"';\nfn not_unsafe<'a>(x: &'a str) -> &'a str { x }\nfn unsafe_free() {}\n";
        assert_eq!(check(WORKSPACE, &[krate("studiocraft-model", WORKSPACE_LINTS, lib)]), Vec::<String>::new());
    }

    #[test]
    fn code_after_a_literal_is_still_checked() {
        let lib = "const A: &str = \"a \\\" b\";\nconst C: char = '\\'';\nconst R: &str = r#\"x\"#;\nfn f() { unsafe {} }\n";
        assert_eq!(unsafe_uses(lib), [(4, "`unsafe`")]);
    }

    #[test]
    fn an_allowed_crate_may_opt_in_with_the_required_lints() {
        assert_eq!(check(WORKSPACE, &[opted_in(OPTED_IN_LIB, OPTED_IN_FFI)]), Vec::<String>::new());
        // Keeping the workspace lints is allowed too, and the crate is then checked like any other.
        assert_eq!(check(WORKSPACE, &[krate("craft-hwdec", WORKSPACE_LINTS, "")]), Vec::<String>::new());
        let hidden = "#[cfg(target_os = \"macos\")]\nfn f() { unsafe {} }\n";
        assert_eq!(check(WORKSPACE, &[krate("craft-hwdec", WORKSPACE_LINTS, hidden)]).len(), 1);
        // The exception is for the listed path too: the same name elsewhere is an ordinary crate.
        let mut elsewhere = opted_in(OPTED_IN_LIB, OPTED_IN_FFI);
        elsewhere.manifest_path = "crates/other/Cargo.toml".into();
        let problems = check(WORKSPACE, &[elsewhere]);
        assert!(problems.iter().any(|p| p.contains("must use the workspace lints")), "{problems:?}");
    }

    #[test]
    fn an_allowed_crate_keeps_unsafe_behind_its_ffi_modules() {
        let lib = "crates/x/src/lib.rs";
        let ok = "#![deny(unsafe_op_in_unsafe_fn)]\n#[cfg(windows)]\n#[allow(unsafe_code)]\npub(crate) mod ffi;\n#[allow(unsafe_code)]\n#[cfg(unix)]\nmod va_ffi;\nfn f(x: u8, y: u8) -> u8 { x * y }\n";
        assert_eq!(boundary_problems(lib, ok), Vec::<(usize, &str)>::new());
        let bad = [
            "#![allow(unsafe_code)]\n",
            "#[allow(unsafe_code)]\nfn f() {}\n",
            "#[allow(unsafe_code)]\nimpl A {}\n",
            "#[allow(unsafe_code)]\nmod helpers;\n",
            "#[allow(unsafe_code)]\nmod ffi {}\n",
            "#[allow(unsafe_code)]\npub mod ffi;\n",
            "pub unsafe fn f() {}\n",
            "pub unsafe extern \"C\" fn f() {}\n",
            "pub unsafe trait T {}\n",
            "pub fn f(p: *mut u8) {}\n",
            "pub struct S {\n    pub p: *const u8,\n}\n",
            "pub use ffi::g;\n",
            "pub use self::{nvenc_ffi::open, other::x};\n",
        ];
        assert_eq!(boundary_problems(lib, "pub(crate) use ffi::g;\npub use other::Safe;\n"), Vec::<(usize, &str)>::new());
        for text in bad {
            assert_eq!(boundary_problems(lib, text).len(), 1, "{text}");
            let text = format!("{LIB_ATTRIBUTE}\n{text}");
            assert_eq!(check(WORKSPACE, &[opted_in(&text, "")]).len(), 1, "{text}");
        }
        // The required lints may not be relaxed anywhere, FFI modules included.
        for text in ["#[allow(unsafe_op_in_unsafe_fn)]\nfn f() {}\n", "#[allow(clippy::undocumented_unsafe_blocks)]\nfn f() {}\n"] {
            assert_eq!(boundary_problems("crates/x/src/ffi.rs", text).len(), 1, "{text}");
        }
        // Inside an FFI module's files, raw pointers and unsafe functions are allowed.
        for path in ["crates/x/src/ffi.rs", "crates/x/src/nvenc_ffi.rs", "crates/x/src/ffi/session.rs", "crates/x/src/va_ffi/mod.rs"] {
            assert!(ffi_file(path), "{path}");
            assert_eq!(boundary_problems(path, OPTED_IN_FFI), Vec::<(usize, &str)>::new(), "{path}");
        }
        assert!(!ffi_file("crates/x/src/buffer.rs"));
        assert!(!ffi_file("crates/ffi/src/lib.rs"));
    }

    #[test]
    fn an_allowed_crate_that_opts_in_needs_every_required_lint() {
        let loose = "[lints.rust]\nunsafe_code = \"allow\"\n\n[lints.clippy]\nunwrap_used = \"deny\"\n";
        let problems = check(WORKSPACE, &[krate("craft-gpu-interop", loose, "mod ffi {}\n")]);
        let text = problems.join("\n");
        for needed in ["unsafe_code = \"deny\"", "unsafe_op_in_unsafe_fn = \"deny\"", "undocumented_unsafe_blocks", "panic = \"deny\"", LIB_ATTRIBUTE]
        {
            assert!(text.contains(needed), "{needed} missing from {text}");
        }
        assert!(!text.contains("unwrap_used"), "{text}");
    }

    #[test]
    fn an_allowed_crate_copies_the_other_workspace_rust_lints() {
        let workspace = WORKSPACE.replace("unsafe_code = \"forbid\"\n", "unsafe_code = \"forbid\"\nmissing_docs = \"warn\"\n");
        let problems = check(&workspace, &[opted_in(OPTED_IN_LIB, OPTED_IN_FFI)]);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("missing_docs"), "{problems:?}");
        let copied = OPTED_IN.replace("[lints.rust]\n", "[lints.rust]\nmissing_docs = \"warn\"\n");
        let mut k = opted_in(OPTED_IN_LIB, OPTED_IN_FFI);
        k.manifest = format!("[package]\nname = \"craft-hwenc-native\"\n\n{copied}");
        assert_eq!(check(&workspace, &[k]), Vec::<String>::new());
    }

    #[test]
    fn the_workspace_must_forbid_unsafe_code() {
        let problems = check("[workspace.lints.rust]\nunsafe_code = \"deny\"\n", &[]);
        assert_eq!(problems.len(), 1, "{problems:?}");
    }

    #[test]
    fn a_folder_that_cannot_be_read_fails_the_check() {
        let missing = std::env::temp_dir().join(format!("studiocraft-xtask-unsafe-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&missing);
        assert!(crates(&missing).is_err());
        assert!(run(&missing).is_err());
    }

    #[test]
    fn rust_files_belong_to_the_nearest_crate() {
        let root = std::env::temp_dir().join(format!("studiocraft-xtask-unsafe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let files = [
            ("Cargo.toml", "[package]\nname = \"app\"\n"),
            ("src/main.rs", "fn main() {}\n"),
            ("crates/a/Cargo.toml", "[package]\nname = \"a\"\n"),
            ("crates/a/src/lib.rs", "fn f() { unsafe {} }\n"),
        ];
        for (f, text) in files {
            let p = root.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, text).unwrap();
        }
        let found = crates(&root);
        let _ = std::fs::remove_dir_all(&root);
        let found = found.unwrap();
        let by_name: BTreeMap<&str, Vec<&str>> =
            found.iter().map(|k| (k.name.as_str(), k.sources.iter().map(|(p, _)| p.as_str()).collect())).collect();
        assert_eq!(by_name.get("app"), Some(&vec!["src/main.rs"]));
        assert_eq!(by_name.get("a"), Some(&vec!["crates/a/src/lib.rs"]));
    }

    /// The repository itself passes the check.
    #[test]
    fn the_workspace_passes() {
        let root = crate::root();
        let workspace = std::fs::read_to_string(root.join("Cargo.toml")).unwrap();
        assert_eq!(check(&workspace, &crates(&root).unwrap()), Vec::<String>::new());
    }
}
