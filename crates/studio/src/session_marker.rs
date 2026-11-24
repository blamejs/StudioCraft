//! Running-session markers: while StudioCraft runs, it keeps a file of its own in the `sessions`
//! folder of the settings folder, and removes it when it closes properly. A marker that an
//! earlier session left behind, with nothing holding it, means that session ended without
//! closing: it crashed, was killed, or the computer lost power.
//!
//! Each running copy of the app holds a lock on its own marker for as long as it runs, so a copy
//! that starts while another runs from the same folder does not take the running one's marker
//! for one left by a crash. The operating system releases the lock when the process ends,
//! however it ends.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// The folder of the markers in the settings folder.
pub const DIR_NAME: &str = "sessions";

/// The start of every marker's file name.
const PREFIX: &str = "session-";

/// The start of the name a marker has while it is being made. The scan skips these names.
const PENDING: &str = "pending-";

/// How much of a marker is read: enough to tell an empty one from one a session wrote.
const READ_BYTES: u64 = 256;

/// The most markers looked at in one start; the rest wait for a later start.
const MAX_MARKERS: usize = 256;

/// How the sessions before this one ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Previous {
    /// They closed properly, or there were none.
    Clean,
    /// At least one ended without closing (its marker was still there and nothing held it), or
    /// how they ended could not be checked.
    Unclean,
}

/// The marker of the running session. [`SessionMarker::finish`] removes it when the session
/// closes properly. Dropping it without that leaves it in place, as a crash does, so the next
/// start finds an unclean shutdown.
#[derive(Debug)]
pub struct SessionMarker {
    file: File,
    path: PathBuf,
}

/// What a marker left in the folder says: `Some(true)` for one left by a session that did not
/// close, `Some(false)` for an empty one, and `None` for one a running session holds. A marker
/// left behind is removed.
fn take_left(path: &Path) -> std::io::Result<Option<bool>> {
    // Read and write access: what the session wrote is read through the locked handle. On
    // Windows another handle cannot read a locked file.
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(None),
        Err(TryLockError::Error(e)) => return Err(e),
    }
    let mut head = Vec::new();
    (&file).take(READ_BYTES).read_to_end(&mut head)?;
    // On Windows an open file cannot be removed. What the marker holds is kept until it is
    // removed, so a marker that can't be removed still counts at the next start.
    drop(file);
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    Ok(Some(!head.is_empty()))
}

/// Whether an entry of the folder, with this type, is a marker to look at: a file is, and a
/// folder or a link is not. An entry that is gone gives `false`; a type that can't be read is an
/// error.
fn look_at(file_type: std::io::Result<std::fs::FileType>) -> std::io::Result<bool> {
    match file_type {
        Ok(t) => Ok(t.is_file()),
        // Another copy that started at the same time removed it.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Look at the earlier markers among the entries of the folder `dir`, each a path and its type,
/// skipping `own`, this session's marker, and looking at no more than `max_markers`. Returns how
/// the sessions before ended and the problems, each with its path and the reason.
fn scan(
    dir: &Path,
    own: &Path,
    entries: impl Iterator<Item = std::io::Result<(PathBuf, std::io::Result<std::fs::FileType>)>>,
    max_markers: usize,
) -> (Previous, Vec<String>) {
    let mut previous = Previous::Clean;
    let mut problems = Vec::new();
    let mut looked = 0;
    for entry in entries {
        let (entry, file_type) = match entry {
            Ok(entry) => entry,
            Err(e) => {
                previous = Previous::Unclean;
                problems.push(format!("{}: {e}", dir.display()));
                break;
            }
        };
        let named = entry.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with(PREFIX));
        if !named || entry == own {
            continue;
        }
        // An entry whose type can't be read counts toward the limit like a marker.
        let file_type = match look_at(file_type) {
            Ok(false) => continue,
            other => other,
        };
        looked += 1;
        if looked > max_markers {
            // The markers not looked at may include one left by a crash.
            previous = Previous::Unclean;
            problems.push(format!("{}: more than {max_markers} session markers; the rest are looked at next time", dir.display()));
            break;
        }
        // An entry whose type can't be read may be a marker left by a crash: the user decides.
        if let Err(e) = file_type {
            previous = Previous::Unclean;
            problems.push(format!("{}: {e}", entry.display()));
            continue;
        }
        match take_left(&entry) {
            Ok(Some(true)) => previous = Previous::Unclean,
            Ok(_) => {}
            // Another copy that started at the same time removed it.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            // A marker that can't be looked at may be one left by a crash: the user decides.
            Err(e) => {
                previous = Previous::Unclean;
                problems.push(format!("{}: {e}", entry.display()));
            }
        }
    }
    (previous, problems)
}

impl SessionMarker {
    /// Report how the sessions before this one ended, removing the markers they left, and put
    /// this session's marker in `settings/sessions`. The problems are the earlier markers that
    /// could not be read or removed, each with its path and the reason.
    pub fn begin(settings: &Path) -> std::io::Result<(SessionMarker, Previous, Vec<String>)> {
        SessionMarker::begin_looking_at(settings, MAX_MARKERS)
    }

    /// [`SessionMarker::begin`] at the app's start, with the problems worded for the status bar.
    /// When this session's marker can't be written there is no marker, and the sessions before
    /// count as unclean: a marker one of them left may still be in the folder.
    pub fn start(settings: &Path) -> (Option<SessionMarker>, Previous, Vec<String>) {
        match SessionMarker::begin(settings) {
            Ok((marker, previous, unread)) => {
                let problems = unread.into_iter().map(|p| format!("a session marker could not be read or removed: {p}")).collect();
                (Some(marker), previous, problems)
            }
            Err(e) => {
                let problem = format!("a crash in this session will not be noticed at the next start: the session marker could not be written: {e}");
                (None, Previous::Unclean, vec![problem])
            }
        }
    }

    /// [`SessionMarker::begin`], looking at no more than `max_markers` earlier markers.
    fn begin_looking_at(settings: &Path, max_markers: usize) -> std::io::Result<(SessionMarker, Previous, Vec<String>)> {
        let dir = settings.join(DIR_NAME);
        std::fs::create_dir_all(&dir)?;
        // This session's marker comes first: when it can't be made, the earlier markers stay for
        // a later start. It is made under a name the scan skips, locked and written, and only
        // then given its marker name, so another copy never finds it unlocked.
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let name = format!("{}-{stamp}", std::process::id());
        let pending = dir.join(format!("{PENDING}{name}"));
        let path = dir.join(format!("{PREFIX}{name}"));
        let mut file = OpenOptions::new().read(true).write(true).create_new(true).open(&pending)?;
        let made = file
            .lock()
            .and_then(|()| file.write_all(format!("pid {}\n", std::process::id()).as_bytes()))
            .and_then(|()| file.sync_data())
            .and_then(|()| std::fs::rename(&pending, &path));
        if let Err(e) = made {
            drop(file);
            let _ = std::fs::remove_file(&pending);
            return Err(e);
        }
        // A folder that can't be listed may hold a marker left by a crash: the user decides.
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) => {
                let problems = vec![format!("{}: {e}", dir.display())];
                return Ok((SessionMarker { file, path }, Previous::Unclean, problems));
            }
        };
        let entries = entries.map(|entry| entry.map(|entry| (entry.path(), entry.file_type())));
        let (previous, problems) = scan(&dir, &path, entries, max_markers);
        Ok((SessionMarker { file, path }, previous, problems))
    }

    /// Where the marker is.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The session closed properly: empty the marker while it is still locked, let the lock go
    /// and remove the file.
    pub fn finish(self) -> std::io::Result<()> {
        let SessionMarker { file, path } = self;
        file.set_len(0)?;
        // Closing the file releases the lock; on Windows an open file cannot be removed. A copy
        // that opens the marker meanwhile finds it empty.
        drop(file);
        match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn begin(settings: &Path) -> (SessionMarker, Previous) {
        let (marker, previous, problems) = SessionMarker::begin(settings).unwrap();
        assert!(problems.is_empty(), "{problems:?}");
        (marker, previous)
    }

    fn markers(settings: &Path) -> usize {
        std::fs::read_dir(settings.join(DIR_NAME)).map_or(0, |d| d.count())
    }

    #[test]
    fn a_first_start_is_clean_and_a_clean_exit_removes_the_marker() {
        let dir = tempfile::tempdir().unwrap();
        let (marker, previous) = begin(dir.path());
        assert_eq!(previous, Previous::Clean);
        assert!(marker.path().exists());
        marker.finish().unwrap();
        assert_eq!(markers(dir.path()), 0);
        let (again, previous) = begin(dir.path());
        assert_eq!(previous, Previous::Clean);
        again.finish().unwrap();
    }

    #[test]
    fn a_session_that_ends_without_finishing_is_found_at_the_next_start() {
        let dir = tempfile::tempdir().unwrap();
        let (marker, _) = begin(dir.path());
        // A crash: the process ends and the lock goes, but the marker stays.
        drop(marker);
        let (next, previous) = begin(dir.path());
        assert_eq!(previous, Previous::Unclean);
        assert_eq!(markers(dir.path()), 1, "the marker left behind is removed");
        next.finish().unwrap();
        assert_eq!(begin(dir.path()).1, Previous::Clean);
    }

    #[test]
    fn copies_of_the_app_running_at_once_each_keep_a_marker() {
        let dir = tempfile::tempdir().unwrap();
        let (first, _) = begin(dir.path());
        let (second, previous) = begin(dir.path());
        assert_eq!(previous, Previous::Clean, "a running copy's marker is not a crash");
        assert!(first.path().exists() && second.path().exists());
        // The first closes properly and the second crashes: the next start finds the second's.
        first.finish().unwrap();
        drop(second);
        let (third, previous) = begin(dir.path());
        assert_eq!(previous, Previous::Unclean);
        third.finish().unwrap();
    }

    #[test]
    fn empty_markers_other_files_and_a_missing_folder_are_handled() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join(DIR_NAME);
        std::fs::create_dir_all(&sessions).unwrap();
        // An empty marker: a clean exit that could not remove the file.
        std::fs::write(sessions.join("session-1-1"), b"").unwrap();
        // Files and folders that are not markers stay as they are.
        std::fs::write(sessions.join("notes.txt"), b"x").unwrap();
        std::fs::create_dir(sessions.join("session-folder")).unwrap();
        let (m, previous) = begin(dir.path());
        assert_eq!(previous, Previous::Clean);
        assert!(!sessions.join("session-1-1").exists());
        assert!(sessions.join("notes.txt").exists() && sessions.join("session-folder").exists());
        m.finish().unwrap();
        // A large marker is read only in part.
        std::fs::write(sessions.join("session-2-2"), vec![b'x'; 1 << 20]).unwrap();
        let (m, previous) = begin(dir.path());
        assert_eq!(previous, Previous::Unclean);
        m.finish().unwrap();
        // A settings folder that does not exist yet is made.
        let nested = dir.path().join("a").join("b");
        let (m, previous) = begin(&nested);
        assert_eq!(previous, Previous::Clean);
        m.finish().unwrap();
    }

    #[test]
    fn many_other_files_do_not_hide_a_marker_left_behind() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join(DIR_NAME);
        std::fs::create_dir_all(&sessions).unwrap();
        for i in 0..300 {
            std::fs::write(sessions.join(format!("other-{i}")), b"x").unwrap();
        }
        std::fs::write(sessions.join("session-9-9"), b"pid 9\n").unwrap();
        // Only markers count toward the limit, so a limit of one still finds the one marker.
        let (m, previous, problems) = SessionMarker::begin_looking_at(dir.path(), 1).unwrap();
        assert!(problems.is_empty());
        assert_eq!(previous, Previous::Unclean);
        m.finish().unwrap();
        // More markers than the limit: the ones not looked at may be from a crash.
        for i in 0..3 {
            std::fs::write(sessions.join(format!("session-1-{i}")), b"").unwrap();
        }
        let (m, previous, problems) = SessionMarker::begin_looking_at(dir.path(), 2).unwrap();
        assert_eq!(previous, Previous::Unclean);
        assert_eq!(problems.len(), 1);
        m.finish().unwrap();
    }

    #[test]
    fn a_markers_folder_that_is_a_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(DIR_NAME), b"x").unwrap();
        assert!(SessionMarker::begin(dir.path()).is_err());
    }

    #[test]
    fn a_marker_that_cannot_be_written_asks_at_start() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(DIR_NAME), b"x").unwrap();
        let (marker, previous, problems) = SessionMarker::start(dir.path());
        assert!(marker.is_none());
        assert_eq!(previous, Previous::Unclean, "a marker left by a crash may still be there");
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("could not be written"), "{problems:?}");
        // A folder that works gives a marker and a clean start.
        let ok = tempfile::tempdir().unwrap();
        let (marker, previous, problems) = SessionMarker::start(ok.path());
        assert_eq!(previous, Previous::Clean);
        assert!(problems.is_empty(), "{problems:?}");
        marker.unwrap().finish().unwrap();
    }

    #[test]
    fn an_entry_whose_type_cannot_be_read_is_not_skipped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file"), b"x").unwrap();
        std::fs::create_dir(dir.path().join("folder")).unwrap();
        assert!(look_at(std::fs::symlink_metadata(dir.path().join("file")).map(|m| m.file_type())).unwrap());
        assert!(!look_at(std::fs::symlink_metadata(dir.path().join("folder")).map(|m| m.file_type())).unwrap());
        assert!(!look_at(Err(std::io::ErrorKind::NotFound.into())).unwrap());
        assert!(look_at(Err(std::io::Error::other("denied"))).is_err());
    }

    #[test]
    fn entries_whose_type_cannot_be_read_count_toward_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let own = dir.path().join("session-own");
        let entries = (0..1000).map(|i| Ok((dir.path().join(format!("session-{i}")), Err(std::io::Error::other("denied")))));
        let (previous, problems) = scan(dir.path(), &own, entries, 3);
        assert_eq!(previous, Previous::Unclean);
        // Three entries and the line saying the rest wait for a later start.
        assert_eq!(problems.len(), 4, "{problems:?}");
        assert!(problems[3].contains("more than 3 session markers"), "{problems:?}");
    }
}
