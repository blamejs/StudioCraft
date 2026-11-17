//! Workspace tooling: `cargo xtask <command>`.
//!
//! Pure Rust (std and serde_json). `cargo` is run through `std::process::Command`.

mod layers;
mod licenses;
mod scan;
mod version;

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const USAGE: &str = "\
usage: cargo xtask <command>

commands:
  fmt        cargo fmt --all --check
  clippy     cargo clippy --workspace --all-targets -D warnings
  test       cargo test --workspace
  layers     check that each crate depends only on crates in lower layers (docs/architecture.md)
  licenses   check that every dependency has an MIT, Apache-2.0, BSD, Zlib, ISC or similar license
  assets     check that every picture, sound, font or video file is listed in ASSETS.md
  brands     check that the app's text and docs name no other vendor's products
  wasm       cargo check --target wasm32-unknown-unknown for the crates that build for the web
  ci         all of the above, stopping at the first failure
  version    print the workspace version; `version set <version>` changes it and Cargo.lock
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("fmt") => fmt(),
        Some("clippy") => clippy(),
        Some("test") => test(),
        Some("layers") => layers::run(),
        Some("licenses") => licenses::run(),
        Some("assets") => scan::assets(&root()),
        Some("brands") => scan::brands(&root()),
        Some("wasm") => wasm(),
        Some("ci") => ci(),
        Some("version") => version::run_command(args.get(1..).unwrap_or(&[])),
        Some("-h" | "--help" | "help") | None => {
            print!("{USAGE}");
            Ok(())
        }
        Some(other) => Err(format!("unknown command `{other}`\n\n{USAGE}")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The workspace root (the parent of the xtask crate).
pub fn root() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest.parent().unwrap_or(manifest).to_path_buf()
}

/// A `cargo` command (the one running xtask, else `cargo` on the PATH) in the workspace root.
pub fn cargo() -> Command {
    let mut c = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    c.current_dir(root());
    c
}

/// Print `what` and run `cmd`, which must exit successfully.
fn run(mut cmd: Command, what: &str) -> Result<(), String> {
    eprintln!("$ {what}");
    let status = cmd.status().map_err(|e| format!("{what}: could not start: {e}"))?;
    if status.success() { Ok(()) } else { Err(format!("{what}: exited with {status}")) }
}

fn fmt() -> Result<(), String> {
    let mut c = cargo();
    c.args(["fmt", "--all", "--check"]);
    run(c, "cargo fmt --all --check")
}

fn clippy() -> Result<(), String> {
    let mut c = cargo();
    c.args(["clippy", "--workspace", "--all-targets", "--", "-D", "warnings"]);
    run(c, "cargo clippy --workspace --all-targets -- -D warnings")
}

fn test() -> Result<(), String> {
    let mut c = cargo();
    c.args(["test", "--workspace"]);
    run(c, "cargo test --workspace")
}

/// Crates with no operating-system or GPU dependency, which the web build will use.
const WASM_CRATES: &[&str] = &["studiocraft-model", "studiocraft-plugin", "craft-color-transform"];

fn wasm() -> Result<(), String> {
    let mut c = cargo();
    c.args(["check", "--target", "wasm32-unknown-unknown"]);
    for k in WASM_CRATES {
        c.args(["-p", k]);
    }
    run(c, &format!("cargo check --target wasm32-unknown-unknown -p {}", WASM_CRATES.join(" -p ")))
        .map_err(|e| format!("{e} (add the target with `rustup target add wasm32-unknown-unknown`)"))
}

fn ci() -> Result<(), String> {
    fmt()?;
    clippy()?;
    test()?;
    layers::run()?;
    licenses::run()?;
    scan::assets(&root())?;
    scan::brands(&root())?;
    wasm()?;
    eprintln!("ci: every check passed");
    Ok(())
}
