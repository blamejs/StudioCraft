//! `studiocraft-cli --version` prints the line the Craft launcher reads: the name and the version.

use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_studiocraft-cli");

#[test]
fn version_prints_the_name_and_the_package_version() {
    for flag in ["--version", "-V", "version"] {
        let out = Command::new(BIN).arg(flag).output().unwrap();
        assert!(out.status.success(), "{flag}: {out:?}");
        assert_eq!(String::from_utf8(out.stdout).unwrap(), format!("studiocraft-cli {}\n", env!("CARGO_PKG_VERSION")), "{flag}");
    }
}
