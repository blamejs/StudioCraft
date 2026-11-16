//! The folders the in-process server may read and write.
//!
//! `studiocraft-cli mcp --read-root <dir> --write-root <dir>` keeps the
//! files a client's calls reach inside the folders given. [`Roots`] holds them. Each flag may be
//! given more than once; a path inside any read root may be read, and a path inside any write root
//! may be written. A kind of access with no root of its own is not granted at all.
//!
//! A path is checked like this:
//!
//! - It is made absolute against the working directory, then its links are followed. For a path
//!   that does not exist yet, the links of its nearest existing folder are followed and the rest
//!   must be plain names: `..` below a folder that does not exist is refused, and so is a link
//!   that leads nowhere.
//! - The result must be a root or lie inside one, compared folder name by folder name, so `/work2`
//!   is not inside `/work`. On Windows the comparison ignores case, and device names (`CON`,
//!   `NUL`, `COM1` and the like) and a `:` after the drive are refused.
//!
//! A path is checked and then opened. Another program that swaps a folder for a link between the
//! two steps can still reach outside the roots; the roots confine the client, not the computer.

use std::ffi::OsString;
use std::fmt;
use std::path::{Component, Path, PathBuf};

/// Reading a file or writing one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
}

impl fmt::Display for Access {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Access::Read => "read",
            Access::Write => "write",
        })
    }
}

impl Access {
    fn flag(self) -> &'static str {
        match self {
            Access::Read => "--read-root",
            Access::Write => "--write-root",
        }
    }
}

/// Why a path may not be used, or a root could not be set up.
#[derive(Debug, thiserror::Error)]
pub enum AccessError {
    #[error("the {access} root `{}` can't be used: {reason}", .path.display())]
    BadRoot { access: Access, path: PathBuf, reason: String },
    #[error("files can't be {}: the server was started without {}: {}", verb(.access), .access.flag(), .path.display())]
    NotGranted { access: Access, path: PathBuf },
    #[error("{} is outside the {access} roots ({roots})", .path.display())]
    Outside { access: Access, path: PathBuf, roots: String },
    #[error("an empty path names no file")]
    Empty,
    #[error("{} names a device, not a file", .path.display())]
    Device { path: PathBuf },
    #[error("{} has a `:` in a file or folder name", .path.display())]
    Stream { path: PathBuf },
    #[error("{} goes up with `..` below a folder that does not exist", .path.display())]
    ParentOfMissing { path: PathBuf },
    #[error("{} is a link to something that does not exist", .path.display())]
    BrokenLink { path: PathBuf },
    #[error("{} could not be resolved: {reason}", .path.display())]
    Unresolved { path: PathBuf, reason: String },
}

fn verb(access: &Access) -> &'static str {
    match access {
        Access::Read => "read",
        Access::Write => "written",
    }
}

/// The read and write roots, each made absolute with its links followed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Roots {
    read: Vec<PathBuf>,
    write: Vec<PathBuf>,
}

impl Roots {
    /// Roots from the folders given, which must exist. A folder given twice counts once.
    pub fn new(read: &[PathBuf], write: &[PathBuf]) -> Result<Roots, AccessError> {
        Ok(Roots { read: open_all(read, Access::Read)?, write: open_all(write, Access::Write)? })
    }

    /// The read roots, absolute with their links followed.
    pub fn read(&self) -> &[PathBuf] {
        &self.read
    }

    /// The write roots, absolute with their links followed.
    pub fn write(&self) -> &[PathBuf] {
        &self.write
    }

    /// Whether neither kind of root was given.
    pub fn is_empty(&self) -> bool {
        self.read.is_empty() && self.write.is_empty()
    }

    /// `path` resolved, when it may be read or written; otherwise why not.
    pub fn check(&self, path: &Path, access: Access) -> Result<PathBuf, AccessError> {
        let roots = match access {
            Access::Read => &self.read,
            Access::Write => &self.write,
        };
        if roots.is_empty() {
            return Err(AccessError::NotGranted { access, path: path.to_path_buf() });
        }
        let resolved = resolve(path)?;
        if roots.iter().any(|r| inside(&resolved, r)) {
            return Ok(resolved);
        }
        let shown: Vec<String> = roots.iter().map(|r| shown(r).display().to_string()).collect();
        Err(AccessError::Outside { access, path: path.to_path_buf(), roots: shown.join(", ") })
    }

    /// One line describing the roots, for the server instructions and the log.
    pub fn describe(&self) -> String {
        let list = |roots: &[PathBuf]| {
            if roots.is_empty() { "none".to_owned() } else { roots.iter().map(|r| shown(r).display().to_string()).collect::<Vec<_>>().join(", ") }
        };
        format!("read roots {}; write roots {}", list(&self.read), list(&self.write))
    }
}

fn open_all(paths: &[PathBuf], access: Access) -> Result<Vec<PathBuf>, AccessError> {
    let mut out: Vec<PathBuf> = Vec::with_capacity(paths.len());
    for p in paths {
        let bad = |reason: String| AccessError::BadRoot { access, path: p.clone(), reason };
        if p.as_os_str().is_empty() {
            return Err(bad("an empty path names no folder".into()));
        }
        let resolved = std::fs::canonicalize(p).map_err(|e| bad(e.to_string()))?;
        if !resolved.is_dir() {
            return Err(bad("it is not a folder".into()));
        }
        if !out.iter().any(|r| same_path(r, &resolved)) {
            out.push(resolved);
        }
    }
    Ok(out)
}

/// `path` without the `\\?\` prefix Windows' canonical paths carry, for people to read.
fn shown(path: &Path) -> PathBuf {
    if !cfg!(windows) {
        return path.to_path_buf();
    }
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    text.strip_prefix(r"\\?\").map_or_else(|| path.to_path_buf(), PathBuf::from)
}

/// `path` made absolute with its links followed; for a path that does not exist, its nearest
/// existing folder's links followed and the remaining names added.
fn resolve(path: &Path) -> Result<PathBuf, AccessError> {
    if path.as_os_str().is_empty() {
        return Err(AccessError::Empty);
    }
    if cfg!(windows) {
        for c in path.components() {
            if let Component::Normal(name) = c {
                let name = name.to_string_lossy();
                if is_device_name(&name) {
                    return Err(AccessError::Device { path: path.to_path_buf() });
                }
                if name.contains(':') {
                    return Err(AccessError::Stream { path: path.to_path_buf() });
                }
            }
        }
    }
    // On Windows, `std::path::absolute` removes each `..` and the name before it from the text, so
    // a `..` that follows a missing folder is refused here, before that happens.
    let mut before = PathBuf::new();
    for c in path.components() {
        if c == Component::ParentDir
            && !before.as_os_str().is_empty()
            && matches!(std::fs::symlink_metadata(&before), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
        {
            return Err(AccessError::ParentOfMissing { path: path.to_path_buf() });
        }
        before.push(c);
    }
    let unresolved = |reason: String| AccessError::Unresolved { path: path.to_path_buf(), reason };
    let absolute = std::path::absolute(path).map_err(|e| unresolved(e.to_string()))?;
    // Names below the nearest existing folder, the last one first.
    let mut missing: Vec<OsString> = Vec::new();
    let mut at = absolute.as_path();
    loop {
        match std::fs::canonicalize(at) {
            Ok(mut found) => {
                found.extend(missing.iter().rev());
                return Ok(found);
            }
            Err(e) => match std::fs::symlink_metadata(at) {
                Ok(m) if m.file_type().is_symlink() => return Err(AccessError::BrokenLink { path: path.to_path_buf() }),
                Ok(_) => return Err(unresolved(e.to_string())),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(unresolved(e.to_string())),
            },
        }
        match (at.components().next_back(), at.parent()) {
            (Some(Component::Normal(name)), Some(parent)) => {
                missing.push(name.to_os_string());
                at = parent;
            }
            (Some(Component::ParentDir), _) => return Err(AccessError::ParentOfMissing { path: path.to_path_buf() }),
            _ => return Err(unresolved("no part of it exists".into())),
        }
    }
}

/// Whether the resolved `path` is `root` or lies inside it, folder name by folder name.
fn inside(path: &Path, root: &Path) -> bool {
    let mut parts = path.components();
    root.components().all(|r| parts.next().is_some_and(|p| same_component(p, r)))
}

fn same_path(a: &Path, b: &Path) -> bool {
    a.components().count() == b.components().count() && inside(a, b)
}

fn same_component(a: Component, b: Component) -> bool {
    if a == b {
        return true;
    }
    if !cfg!(windows) {
        return false;
    }
    match (a.as_os_str().to_str(), b.as_os_str().to_str()) {
        (Some(a), Some(b)) => a.to_lowercase() == b.to_lowercase(),
        _ => false,
    }
}

/// A Windows device name (`CON`, `NUL`, `COM1` and the like, also with an extension).
fn is_device_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).trim_end_matches(' ').to_ascii_uppercase();
    let numbered = |prefix: &str| stem.strip_prefix(prefix).is_some_and(|n| n.len() == 1 && n.bytes().all(|b| (b'1'..=b'9').contains(&b)));
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$") || numbered("COM") || numbered("LPT")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh folder holding `in` (the read root), `out` (the write root) and `outside`.
    fn folders(tag: &str) -> (PathBuf, Roots) {
        let base = std::env::temp_dir().join(format!("studiocraft-roots-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        for d in ["in/sub", "out", "outside", "in2"] {
            std::fs::create_dir_all(base.join(d)).unwrap();
        }
        std::fs::write(base.join("in/show.json"), "{}").unwrap();
        std::fs::write(base.join("outside/secret.json"), "{}").unwrap();
        let roots = Roots::new(&[base.join("in")], &[base.join("out")]).unwrap();
        (base, roots)
    }

    #[test]
    fn paths_inside_a_root_pass_and_others_are_refused() {
        let (base, roots) = folders("inside");
        assert!(roots.check(&base.join("in/show.json"), Access::Read).is_ok());
        assert!(roots.check(&base.join("in"), Access::Read).is_ok(), "the root itself");
        assert!(roots.check(&base.join("in/sub/new.json"), Access::Read).is_ok(), "a missing file is checked by its folder");
        assert!(roots.check(&base.join("out/new/deeper/x.mkv"), Access::Write).is_ok(), "and by its nearest existing folder");
        let e = roots.check(&base.join("outside/secret.json"), Access::Read).unwrap_err();
        assert!(matches!(e, AccessError::Outside { access: Access::Read, .. }), "{e}");
        assert!(e.to_string().contains("outside the read roots"), "{e}");
        // Read and write access are separate.
        assert!(matches!(roots.check(&base.join("in/show.json"), Access::Write), Err(AccessError::Outside { .. })));
        assert!(matches!(roots.check(&base.join("out/x.json"), Access::Read), Err(AccessError::Outside { .. })));
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn a_sibling_with_the_same_prefix_is_outside() {
        let (base, roots) = folders("prefix");
        assert!(roots.check(&base.join("in2/x.json"), Access::Read).is_err());
        let text = format!("{}2{}x.json", base.join("in").display(), std::path::MAIN_SEPARATOR);
        assert!(roots.check(Path::new(&text), Access::Read).is_err());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn parent_steps_are_resolved_or_refused() {
        let (base, roots) = folders("dotdot");
        let up = base.join("in").join("..").join("outside").join("secret.json");
        assert!(matches!(roots.check(&up, Access::Read), Err(AccessError::Outside { .. })), "an existing path is resolved first");
        let back_in = base.join("in").join("sub").join("..").join("show.json");
        assert!(roots.check(&back_in, Access::Read).is_ok(), "`..` that stays inside is fine");
        let below_missing = base.join("in").join("nothing").join("..").join("..").join("outside").join("secret.json");
        assert!(matches!(roots.check(&below_missing, Access::Read), Err(AccessError::ParentOfMissing { .. })));
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn a_kind_of_access_without_a_root_is_not_granted() {
        let (base, _) = folders("one-kind");
        let roots = Roots::new(&[base.join("in")], &[]).unwrap();
        let e = roots.check(&base.join("in/show.json"), Access::Write).unwrap_err();
        assert!(matches!(e, AccessError::NotGranted { access: Access::Write, .. }), "{e}");
        assert!(e.to_string().contains("--write-root"), "{e}");
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn several_roots_of_one_kind_each_grant_access_and_repeats_count_once() {
        let (base, _) = folders("several");
        let roots = Roots::new(&[base.join("in"), base.join("in2"), base.join("in")], &[]).unwrap();
        assert_eq!(roots.read().len(), 2);
        assert!(roots.check(&base.join("in2/x.json"), Access::Read).is_ok());
        assert!(roots.check(&base.join("in/show.json"), Access::Read).is_ok());
        assert!(roots.describe().contains("write roots none"), "{}", roots.describe());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn a_root_must_be_an_existing_folder() {
        let (base, _) = folders("bad-root");
        assert!(matches!(Roots::new(&[base.join("missing")], &[]), Err(AccessError::BadRoot { .. })));
        assert!(matches!(Roots::new(&[], &[base.join("in/show.json")]), Err(AccessError::BadRoot { .. })));
        assert!(matches!(Roots::new(&[PathBuf::new()], &[]), Err(AccessError::BadRoot { .. })));
        assert!(matches!(Roots::new(&[base.join("in")], &[]).map(|r| r.check(Path::new(""), Access::Read)), Ok(Err(AccessError::Empty))));
        let _ = std::fs::remove_dir_all(base);
    }

    #[cfg(unix)]
    #[test]
    fn links_that_lead_out_of_a_root_are_outside() {
        let (base, roots) = folders("links");
        std::os::unix::fs::symlink(base.join("outside"), base.join("in/escape")).unwrap();
        std::os::unix::fs::symlink(base.join("outside/none"), base.join("in/dangling")).unwrap();
        assert!(matches!(roots.check(&base.join("in/escape/secret.json"), Access::Read), Err(AccessError::Outside { .. })));
        assert!(matches!(roots.check(&base.join("in/dangling"), Access::Read), Err(AccessError::BrokenLink { .. })));
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn device_names_are_recognized() {
        for name in ["CON", "nul", "COM1", "lpt9.txt", "AUX "] {
            assert!(is_device_name(name), "{name}");
        }
        for name in ["COM0", "COM10", "console", "show.json"] {
            assert!(!is_device_name(name), "{name}");
        }
    }
}
