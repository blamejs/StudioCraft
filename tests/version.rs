//! `studiocraft --version` prints the line the Craft launcher reads: the name and the version.
// Unwrapping and panicking on failure is fine here, unlike in shipped code (AGENTS.md › Never crash).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::process::Command;

#[test]
fn version_prints_the_name_and_the_package_version() {
    for flag in ["--version", "-V"] {
        let out = Command::new(env!("CARGO_BIN_EXE_studiocraft")).arg(flag).output().unwrap();
        assert!(out.status.success(), "{flag}: {out:?}");
        assert_eq!(String::from_utf8(out.stdout).unwrap(), format!("StudioCraft {}\n", env!("CARGO_PKG_VERSION")), "{flag}");
    }
}
