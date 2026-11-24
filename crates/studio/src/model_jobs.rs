//! The model folders that downloads and installs write to, held across every studio in the
//! process.
//!
//! A download stopped when its studio closes is not waited for, and it deletes its part file when
//! it reaches its next read. Until then its folder stays held, so a download or install that a
//! later studio starts in the same folder is refused instead of having its part file deleted.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

static HELD: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// A model folder held by one download or install until this is dropped.
pub(crate) struct Held(PathBuf);

impl Held {
    /// Hold `dir`, or `None` while another download or install, of this studio or of one that has
    /// closed, still writes to it.
    pub(crate) fn take(dir: &Path) -> Option<Held> {
        // The job writes into the folder anyway; once it exists, every name of it resolves to one
        // path. A folder that cannot be made is compared by its deepest existing part, and the job
        // then fails when it tries to write there.
        let _ = std::fs::create_dir_all(dir);
        let dir = resolved(dir);
        let mut held = HELD.lock().unwrap_or_else(|e| e.into_inner());
        if held.contains(&dir) {
            return None;
        }
        held.push(dir.clone());
        Some(Held(dir))
    }
}

/// `dir` with its deepest existing folder resolved (links and relative parts followed), and the
/// parts that do not exist yet after it, so two names of one folder compare equal.
fn resolved(dir: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut at = dir;
    loop {
        if let Ok(real) = std::fs::canonicalize(at) {
            return rest.iter().rev().fold(real, |p, part| p.join(part));
        }
        match (at.parent(), at.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                at = parent;
            }
            _ => return dir.to_path_buf(),
        }
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        HELD.lock().unwrap_or_else(|e| e.into_inner()).retain(|d| d != &self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_is_held_once_until_it_is_let_go() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let first = Held::take(&a).unwrap();
        assert!(Held::take(&a).is_none());
        let other = Held::take(&dir.path().join("b"));
        assert!(other.is_some());
        drop(first);
        assert!(Held::take(&a).is_some());
    }

    #[test]
    fn two_names_of_one_folder_are_one_folder() {
        let dir = tempfile::tempdir().unwrap();
        let first = Held::take(&dir.path().join("models").join("face"));
        assert!(first.is_some());
        assert!(Held::take(&dir.path().join(".").join("models").join("face")).is_none());
        assert!(Held::take(&dir.path().join("new").join("..").join("models").join("face")).is_none());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.path(), dir.path().join("link")).unwrap();
            assert!(Held::take(&dir.path().join("link").join("models").join("face")).is_none());
        }
    }
}
