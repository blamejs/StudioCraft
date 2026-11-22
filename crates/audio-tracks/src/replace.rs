//! Writing a file again only when nobody else wrote it since it was read.
//!
//! [`replace_unchanged`] stages the new bytes beside the file as [`craft_atomic::atomic_write`]
//! does, then reads the file once more just before the rename that puts the new bytes in place.
//! When the file no longer holds the bytes it was read with, as after another app saved it, the
//! rename does not run, the staged file is removed and the call is an error.

use std::fs::{File, OpenOptions};
use std::io::Read;
use std::path::Path;

use craft_atomic::{AtomicFs, NewFilePermissions, RealFs, RenameRetry};

/// Replace the file `path`, read as `was`, with `bytes` in one step, unless it no longer holds
/// `was`. A changed file is left as it is, and the error says it changed. Another writer can
/// still save between that last read and the rename.
pub fn replace_unchanged(path: &Path, was: &[u8], bytes: &[u8]) -> std::io::Result<()> {
    craft_atomic::atomic_write_custom(&Unchanged { path, was }, RenameRetry::platform(), path, bytes)
}

/// Whether the file at `path` holds exactly `was`. A link in its last name, or anything that is
/// not a file, does not.
fn holds(path: &Path, was: &[u8]) -> std::io::Result<bool> {
    let file = crate::open::no_follow(OpenOptions::new().read(true), path)?;
    let mut now = Vec::with_capacity(was.len());
    // One byte past `was` is enough to see the file grew.
    file.take(u64::try_from(was.len()).unwrap_or(u64::MAX).saturating_add(1)).read_to_end(&mut now)?;
    Ok(now == was)
}

/// [`RealFs`], except that the rename over `path` first checks that it still holds `was`.
struct Unchanged<'a> {
    path: &'a Path,
    was: &'a [u8],
}

impl AtomicFs for Unchanged<'_> {
    fn create_new(&self, path: &Path, permissions: NewFilePermissions) -> std::io::Result<File> {
        RealFs.create_new(path, permissions)
    }

    fn sync(&self, file: &File) -> std::io::Result<()> {
        RealFs.sync(file)
    }

    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        if !holds(self.path, self.was)? {
            return Err(std::io::Error::other(format!("{} changed after it was read", self.path.display())));
        }
        RealFs.rename(from, to)
    }

    fn hard_link(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        RealFs.hard_link(from, to)
    }

    fn remove(&self, path: &Path) -> std::io::Result<()> {
        RealFs.remove(path)
    }

    fn sync_dir(&self, dir: &Path) -> std::io::Result<()> {
        RealFs.sync_dir(dir)
    }

    fn set_permissions(&self, file: &File, perms: std::fs::Permissions) -> std::io::Result<()> {
        RealFs.set_permissions(file, perms)
    }

    #[cfg(unix)]
    fn set_group(&self, file: &File, gid: u32) -> std::io::Result<()> {
        RealFs.set_group(file, gid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_that_changed_after_it_was_read_is_not_replaced() {
        let dir = std::env::temp_dir().join(format!("studiocraft-replace-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.txt");
        std::fs::write(&path, b"old").unwrap();
        replace_unchanged(&path, b"old", b"new").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        // Another writer saved the file since it was read as "new": shorter, longer, the same length.
        for saved in [&b"ne"[..], b"newer", b"wen"] {
            std::fs::write(&path, saved).unwrap();
            let e = replace_unchanged(&path, b"new", b"mine").unwrap_err();
            assert!(e.to_string().contains("changed after it was read"), "{e}");
            assert_eq!(std::fs::read(&path).unwrap(), saved);
        }
        assert!(replace_unchanged(&dir.join("none.txt"), b"", b"mine").is_err(), "a file that is gone");
        let left: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(left, ["a.txt"], "no staged file is left behind");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
