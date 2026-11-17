//! Dependency licenses: every package must be usable under a permissive license.

use serde_json::Value;

/// Licenses accepted on their own.
pub const ALLOWED: &[&str] = &[
    "MIT",
    "MIT-0",
    "Apache-2.0",
    "Apache-2.0 WITH LLVM-exception",
    "BSD-1-Clause",
    "BSD-2-Clause",
    "BSD-3-Clause",
    "0BSD",
    "Zlib",
    "ISC",
    "BSL-1.0",
    "CC0-1.0",
    "Unicode-3.0",
    "Unicode-DFS-2016",
    "Unlicense",
    "CDLA-Permissive-2.0",
    // Font licenses: egui's built-in fonts.
    "OFL-1.1",
    "Ubuntu-font-1.0",
    // The Independent JPEG Group license, a BSD-style license.
    "IJG",
];

/// Packages accepted under a license that is not in [`ALLOWED`], each with the license and the
/// package that needs it. Each one is used unmodified, as a separate crate.
pub const EXCEPTIONS: &[(&str, &str, &str)] = &[
    // MPL-2.0 is a file-level license; `dirs-sys` (through `craft-settings`) depends on it.
    ("option-ext", "MPL-2.0", "dirs-sys"),
    // NCSA is a BSD-style license. `rav1e` depends on `libfuzzer-sys` only in fuzzing builds, and
    // `rav1e` is an optional dependency of `craft-video` that no StudioCraft crate enables;
    // `cargo metadata` lists both because `Cargo.lock` does.
    ("libfuzzer-sys", "(MIT OR Apache-2.0) AND NCSA", "rav1e"),
];

fn excepted(name: &str, license: &str) -> bool {
    EXCEPTIONS.iter().any(|(n, l, _)| *n == name && *l == license)
}

fn term_ok(term: &str) -> bool {
    let t = term.trim().trim_matches(|c| c == '(' || c == ')').trim();
    ALLOWED.iter().any(|a| a.eq_ignore_ascii_case(t))
}

/// Whether an SPDX expression (`MIT OR Apache-2.0`, `(MIT OR Apache-2.0) AND Unicode-3.0`, the old
/// `MIT/Apache-2.0`) offers a permissive choice.
pub fn permissive(expr: &str) -> bool {
    let e = expr.replace('/', " OR ");
    // Each AND part must have a permissive alternative.
    e.split(" AND ").all(|part| part.split(" OR ").any(term_ok))
}

pub fn run() -> Result<(), String> {
    eprintln!("$ licenses");
    let out = crate::cargo().args(["metadata", "--format-version", "1"]).output().map_err(|e| format!("cargo metadata: {e}"))?;
    if !out.status.success() {
        return Err(format!("cargo metadata failed: {}", String::from_utf8_lossy(&out.stderr)));
    }
    let meta: Value = serde_json::from_slice(&out.stdout).map_err(|e| format!("cargo metadata: {e}"))?;
    let mut problems = Vec::new();
    for p in meta["packages"].as_array().into_iter().flatten() {
        let name = p["name"].as_str().unwrap_or("?");
        let version = p["version"].as_str().unwrap_or("?");
        match p["license"].as_str() {
            Some(l) if permissive(l) || excepted(name, l) => {}
            Some(l) => problems.push(format!("{name} {version}: {l}")),
            None => problems.push(format!("{name} {version}: no license field")),
        }
    }
    if problems.is_empty() { Ok(()) } else { Err(format!("dependencies without a permissive license:\n  {}", problems.join("\n  "))) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_spdx_expressions() {
        assert!(permissive("MIT"));
        assert!(permissive("MIT OR Apache-2.0"));
        assert!(permissive("MIT/Apache-2.0"));
        assert!(permissive("(MIT OR Apache-2.0) AND Unicode-3.0"));
        assert!(permissive("Apache-2.0 OR LGPL-2.1-or-later OR MIT"));
        assert!(!permissive("GPL-3.0-only"));
        assert!(!permissive("LGPL-2.1"));
        assert!(!permissive("MIT AND GPL-2.0"));
        assert!(!permissive("MPL-2.0"));
        assert!(permissive("(MIT OR Apache-2.0) AND OFL-1.1 AND Ubuntu-font-1.0"));
        assert!(!permissive("(MIT OR Apache-2.0) AND NCSA"));
    }

    #[test]
    fn exceptions_name_one_package_and_license() {
        assert!(excepted("option-ext", "MPL-2.0"));
        assert!(!excepted("option-ext", "GPL-3.0-only"));
        assert!(!excepted("other-crate", "MPL-2.0"));
        assert!(excepted("libfuzzer-sys", "(MIT OR Apache-2.0) AND NCSA"));
        assert!(!excepted("other-crate", "(MIT OR Apache-2.0) AND NCSA"), "NCSA is accepted for libfuzzer-sys alone");
    }
}
