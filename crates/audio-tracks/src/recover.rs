//! Repair of takes that ended without their journal's last line (the app or the computer stopped
//! during the take): each file is cut to its whole frames and given a header for them.

use std::path::{Path, PathBuf};

use crate::journal::{self, Journal, Line};

/// What repairing one take did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Repaired {
    /// The take's `Audio Files` folder.
    pub dir: PathBuf,
    /// Each file and the frames it holds now.
    pub files: Vec<(String, u64)>,
    /// Frames the journal's last sync recorded for each file, which a repaired file holds at
    /// least.
    pub synced: Vec<(String, u64)>,
}

/// The `Audio Files` folders under `folder` whose take did not end: `folder` itself, its
/// `Audio Files` folder, or those of the folders in it (a recording folder). A journal that cannot
/// be read is included, so repairing it reports why; a take still being written (its journal is
/// locked) is then left as it is by [`repair`]. Links are not followed below `folder`.
pub fn unfinished(folder: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut check = |dir: PathBuf| {
        if (dir == folder || real_dir(&dir)) && real_file(&dir.join(journal::FILE_NAME)) {
            let open = match journal::read(&dir) {
                Ok(lines) => !lines.iter().any(|l| matches!(l, Line::End { .. })),
                Err(_) => true,
            };
            if open {
                found.push(dir);
            }
        }
    };
    check(folder.to_path_buf());
    check(folder.join("Audio Files"));
    if let Ok(entries) = std::fs::read_dir(folder) {
        for e in entries.flatten() {
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                check(e.path().join("Audio Files"));
            }
        }
    }
    found.sort();
    found.dedup();
    found
}

/// Repair the take in `dir`, its `Audio Files` folder: every file the journal names is cut to
/// whole frames with a header for them, and the journal is ended. A take that ended, and a take
/// still being written (its journal is locked), are left as they are and give `None`. The journal
/// and the files it names must be files in `dir` itself, not links, so repair writes only there.
pub fn repair(dir: &Path) -> std::io::Result<Option<Repaired>> {
    if !real_file(&dir.join(journal::FILE_NAME)) {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "the take's journal is not a file in its folder"));
    }
    let Some((mut j, lines)) = Journal::open_idle(dir)? else { return Ok(None) };
    if lines.iter().any(|l| matches!(l, Line::End { .. })) {
        return Ok(None);
    }
    let mut names: Vec<String> = Vec::new();
    // The take frame each file starts at, by file name.
    let mut starts: Vec<(String, u64)> = Vec::new();
    let mut synced: Vec<(String, u64)> = Vec::new();
    for l in &lines {
        match l {
            Line::File { file, first_frame, .. } if !names.contains(file) => {
                names.push(file.clone());
                starts.push((file.clone(), *first_frame));
            }
            Line::Sync { files } => {
                for (f, n) in files {
                    match synced.iter_mut().find(|(s, _)| s == f) {
                        Some(s) => s.1 = *n,
                        None => synced.push((f.clone(), *n)),
                    }
                }
            }
            _ => {}
        }
    }
    // The writer names only files in this folder, so any other name means the journal was damaged:
    // the take stays unfinished, and no file is touched.
    if let Some(bad) = names.iter().find(|n| Path::new(n.as_str()).file_name() != Some(std::ffi::OsStr::new(n.as_str()))) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("the journal names {bad:?}, which is not a file in the take's folder"),
        ));
    }
    // Every file is checked before any is cut, so a take that stays unfinished is left as it was.
    let head = crate::wav::header_len()?;
    for name in &names {
        let path = dir.join(name);
        // Each file is made before the journal names it, so a missing one was moved or deleted
        // since: the take stays unfinished, to be repaired once the file is back.
        if !real_file(&path) {
            return Err(std::io::Error::new(std::io::ErrorKind::NotFound, format!("{name} is missing from the take's folder")));
        }
        let len = crate::wav::check(&path).map_err(|e| std::io::Error::new(e.kind(), format!("{name}: {e}")))?;
        // A sync put its frames on the disk, so a file holding fewer was cut or replaced since.
        let held = len.saturating_sub(head) / crate::wav::FRAME_BYTES;
        if let Some((_, want)) = synced.iter().find(|(f, _)| f == name)
            && held < *want
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{name} holds {held} frames, fewer than the {want} the journal recorded as written"),
            ));
        }
    }
    let mut files = Vec::new();
    for name in names {
        let frames = crate::wav::repair(&dir.join(&name))?;
        files.push((name, frames));
    }
    // A part after the first starts later in the take, so the take ends after its last frame.
    let start = |name: &str| starts.iter().find(|(f, _)| f == name).map_or(0, |(_, s)| *s);
    let end = files.iter().map(|(f, n)| start(f).saturating_add(*n)).max().unwrap_or(0);
    j.write(&Line::Repaired { files: files.clone() })?;
    j.write(&Line::End { frames: end })?;
    j.sync()?;
    Ok(Some(Repaired { dir: dir.to_path_buf(), files, synced }))
}

/// What repairing the takes under a folder did.
#[derive(Debug, Default)]
pub struct Outcome {
    pub repaired: Vec<Repaired>,
    /// Each take that could not be repaired, with why.
    pub failed: Vec<(PathBuf, std::io::Error)>,
}

/// Repair every unfinished take under `folder` (see [`unfinished`]). A take that cannot be
/// repaired does not stop the others.
pub fn repair_each(folder: &Path) -> Outcome {
    let mut out = Outcome::default();
    for dir in unfinished(folder) {
        match repair(&dir) {
            Ok(Some(r)) => out.repaired.push(r),
            Ok(None) => {}
            Err(e) => out.failed.push((dir, e)),
        }
    }
    out
}

/// Repair every unfinished take under `folder`, as [`repair_each`] does. When a take could not
/// be repaired, the error names each such folder and why, after the others are repaired.
pub fn repair_all(folder: &Path) -> std::io::Result<Vec<Repaired>> {
    let out = repair_each(folder);
    if out.failed.is_empty() {
        return Ok(out.repaired);
    }
    let why: Vec<String> = out.failed.iter().map(|(d, e)| format!("{}: {e}", d.display())).collect();
    Err(std::io::Error::other(format!("{} take(s) could not be repaired ({} repaired): {}", out.failed.len(), out.repaired.len(), why.join("; "))))
}

/// Whether `p` is a folder and not a link to one.
fn real_dir(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir())
}

/// Whether `p` is a file and not a link to one.
fn real_file(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.is_file())
}
