//! `cargo xtask version` prints the workspace version, and `cargo xtask version set <version>`
//! changes it.
//!
//! The version is `[workspace.package] version` in the root `Cargo.toml`. Every crate inherits it
//! with `version.workspace = true`, and the release builds and the packaging scripts read it from
//! there.

use std::path::Path;

use crate::{cargo, root, run};

/// The value of `version = "..."` in `[workspace.package]`.
pub fn read(manifest: &str) -> Result<String, String> {
    let (_, line) = version_line(manifest)?;
    let value = line.split_once('=').map(|(_, v)| v.trim().trim_matches('"').to_string()).unwrap_or_default();
    if value.is_empty() {
        return Err("[workspace.package] version is empty".into());
    }
    Ok(value)
}

/// `manifest` with the workspace version set to `new`. Every other line keeps its bytes.
pub fn replace(manifest: &str, new: &str) -> Result<String, String> {
    validate(new)?;
    let (index, _) = version_line(manifest)?;
    let mut out = String::with_capacity(manifest.len() + new.len());
    for (i, line) in manifest.split_inclusive('\n').enumerate() {
        if i == index {
            let eol = if line.ends_with("\r\n") {
                "\r\n"
            } else if line.ends_with('\n') {
                "\n"
            } else {
                ""
            };
            out.push_str(&format!("version = \"{new}\"{eol}"));
        } else {
            out.push_str(line);
        }
    }
    Ok(out)
}

/// A version is `MAJOR.MINOR.PATCH` with an optional `-pre.release` part and no build metadata,
/// and each number fits in a `u64`, as Cargo requires.
pub fn validate(version: &str) -> Result<(), String> {
    let invalid = || Err(format!("`{version}` is not a version like 1.2.3 or 1.2.3-rc.1"));
    let (core, pre) = match version.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (version, None),
    };
    let numbers: Vec<&str> = core.split('.').collect();
    let bad_number =
        |n: &&str| n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) || (n.len() > 1 && n.starts_with('0')) || n.parse::<u64>().is_err();
    if numbers.len() != 3 || numbers.iter().any(bad_number) {
        return invalid();
    }
    // A numeric prerelease part may not start with 0; a part with a letter in it may.
    let bad_part = |part: &str| {
        part.is_empty()
            || !part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || (part.len() > 1 && part.starts_with('0') && part.bytes().all(|b| b.is_ascii_digit()))
    };
    if let Some(pre) = pre
        && (pre.is_empty() || pre.split('.').any(bad_part))
    {
        return invalid();
    }
    Ok(())
}

/// The index and text of the `version` line in `[workspace.package]`.
fn version_line(manifest: &str) -> Result<(usize, &str), String> {
    let mut in_section = false;
    for (i, line) in manifest.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_section = trimmed == "[workspace.package]";
            continue;
        }
        if in_section
            && let Some(rest) = trimmed.strip_prefix("version")
            && rest.trim_start().starts_with('=')
        {
            return Ok((i, line));
        }
    }
    Err("the root Cargo.toml has no `version = \"...\"` in [workspace.package]".into())
}

/// Writes `text` to `path` through a temporary file in the same folder and a rename, so a
/// failed write leaves the old manifest in place.
fn write_manifest(path: &Path, text: &str) -> Result<(), String> {
    let tmp = path.with_extension("toml.xtask-version");
    std::fs::write(&tmp, text).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("replace {}: {e}", path.display()))
}

/// Sets the version in the manifest at `path` (whose text is `text`) to `new`, then runs
/// `update`. When `update` fails, the old manifest is written back. Returns the old version.
fn set(path: &Path, text: &str, new: &str, update: impl FnOnce() -> Result<(), String>) -> Result<String, String> {
    let old = read(text)?;
    write_manifest(path, &replace(text, new)?)?;
    if let Err(e) = update() {
        return Err(match write_manifest(path, text) {
            Ok(()) => format!("{e}; Cargo.toml is back at {old}"),
            Err(restore) => format!("{e}; Cargo.toml could not be put back: {restore}"),
        });
    }
    Ok(old)
}

/// `cargo update --workspace` rewrites the workspace crates' entries in Cargo.lock and upgrades
/// no dependency. It tries offline first.
fn update_lock() -> Result<(), String> {
    let mut offline = cargo();
    offline.args(["update", "--workspace", "--offline"]);
    run(offline, "cargo update --workspace --offline").or_else(|_| {
        let mut online = cargo();
        online.args(["update", "--workspace"]);
        run(online, "cargo update --workspace")
    })
}

/// `cargo xtask version [set <version>]`; `args` are the words after `version`.
pub fn run_command(args: &[String]) -> Result<(), String> {
    let path = root().join("Cargo.toml");
    let text = std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        [] => {
            println!("{}", read(&text)?);
            Ok(())
        }
        ["set", new] => {
            let old = set(&path, &text, new, update_lock)?;
            println!("version: {old} -> {new}");
            Ok(())
        }
        _ => Err("usage: cargo xtask version [set <version>]".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = "[workspace]\nmembers = [\"a\"]\n\n[workspace.package]\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace.dependencies]\nserde = { version = \"1\" }\n";

    #[test]
    fn reads_the_workspace_version_only() {
        assert_eq!(read(MANIFEST).unwrap(), "0.1.0");
        assert!(read("[package]\nversion = \"1.0.0\"\n").is_err());
    }

    #[test]
    fn replaces_only_the_workspace_version() {
        let out = replace(MANIFEST, "1.2.3-rc.1").unwrap();
        assert_eq!(out, MANIFEST.replace("version = \"0.1.0\"", "version = \"1.2.3-rc.1\""));
        assert!(out.contains("serde = { version = \"1\" }"));
    }

    #[test]
    fn keeps_crlf_line_endings() {
        let crlf = MANIFEST.replace('\n', "\r\n");
        assert_eq!(replace(&crlf, "2.0.0").unwrap(), crlf.replace("\"0.1.0\"", "\"2.0.0\""));
    }

    #[test]
    fn accepts_versions_cargo_accepts_and_rejects_the_rest() {
        for good in ["0.1.0", "1.20.300", "1.0.0-rc.1", "1.0.0-alpha", "1.0.0-x-y.2", "1.0.0-0", "1.0.0-01a", "18446744073709551615.0.0"] {
            assert!(validate(good).is_ok(), "{good}");
        }
        for bad in
            ["", "1.0", "1.0.0.0", "v1.0.0", "01.0.0", "1..0", "1.0.0-", "1.0.0-a..b", "1.0.0+meta", "1.a.0", "1.0.0-01", "18446744073709551616.0.0"]
        {
            assert!(validate(bad).is_err(), "{bad}");
            assert!(replace(MANIFEST, bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_failed_lock_update_puts_the_manifest_back() {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
        let dir = std::env::temp_dir().join(format!("studiocraft-xtask-version-{}-{nanos}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("Cargo.toml");
        std::fs::write(&path, MANIFEST).unwrap();
        let err = set(&path, MANIFEST, "2.0.0", || Err("cargo update failed".into())).unwrap_err();
        assert!(err.contains("back at 0.1.0"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), MANIFEST);
        assert_eq!(set(&path, MANIFEST, "2.0.0", || Ok(())).unwrap(), "0.1.0");
        assert_eq!(read(&std::fs::read_to_string(&path).unwrap()).unwrap(), "2.0.0");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_workspace_version_is_valid() {
        let text = std::fs::read_to_string(root().join("Cargo.toml")).unwrap();
        validate(&read(&text).unwrap()).unwrap();
    }
}
