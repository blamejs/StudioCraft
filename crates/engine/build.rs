//! Builds the contributor table (`credits::CONTRIBUTORS`) from `contributors/people.toml` and the
//! optional `contributors/contributors.json` at the repository root.
//!
//! A missing `people.toml` or a file that cannot be parsed gives a warning and fewer names (or an
//! empty table); the build carries on. Nothing is read at run time.

#[path = "src/credits_gen.rs"]
mod credits_gen;

use std::path::{Path, PathBuf};

/// The file's text: `None` when it is absent or unreadable (with a warning when `required`).
fn read(path: &Path, required: bool) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(t) => Some(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !required => None,
        Err(e) => {
            println!("cargo::warning={}: {e}; the About window lists no contributors from it", path.display());
            None
        }
    }
}

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-changed=src/credits_gen.rs");
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contributors");
    let people_path = dir.join("people.toml");
    let json_path = dir.join("contributors.json");
    // The folder too, so that adding `contributors.json` later runs this again.
    println!("cargo::rerun-if-changed={}", dir.display());
    println!("cargo::rerun-if-changed={}", people_path.display());
    if json_path.exists() {
        println!("cargo::rerun-if-changed={}", json_path.display());
    }
    let people = read(&people_path, true);
    let json = read(&json_path, false);
    let (src, warnings) = credits_gen::generate(people.as_deref(), json.as_deref());
    for w in warnings {
        println!("cargo::warning={}", w.replace('\n', " "));
    }
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap_or_default()).join("credits.rs");
    if std::fs::read_to_string(&out).is_ok_and(|old| old == src) {
        return;
    }
    if let Err(e) = std::fs::write(&out, src) {
        println!("cargo::error=writing {}: {e}", out.display());
    }
}
