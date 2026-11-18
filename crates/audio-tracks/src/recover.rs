//! Repair of takes that ended without their journal's last line (the app or the computer stopped
//! during the take): each file is cut to its whole frames and given a header for them, with its
//! `bext` time reference kept. Each file keeps the audio it holds, so the files of a repaired take
//! can end at different points: the chunks still in memory at the crash are lost, and their
//! length differs from track to track.

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
/// `Audio Files` folder, or those of the folders in it (a recording folder). Whether a take ended
/// is read from the end of its journal alone, so a long take's journal is not read whole. A
/// journal that cannot be read is included, so repairing it reports why; a take still being
/// written (its journal is locked) is then left as it is by [`repair`]. Links are not followed
/// below `folder`.
pub fn unfinished(folder: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut check = |dir: PathBuf| {
        if (dir == folder || real_dir(&dir)) && real_file(&dir.join(journal::FILE_NAME)) {
            let open = !journal::ended(&dir).unwrap_or(false);
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
/// The files must have names in the writer's format that differ in more than letter case, and each
/// must have one name (no hard links), so a file that is removed is never one that is kept.
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
    // Each track's file at this point of the journal: a sync line names tracks, not files.
    let mut current: std::collections::HashMap<usize, String> = std::collections::HashMap::new();
    // Each track's files, and the take frame a `Cut` line cut them back to.
    let mut of_track: Vec<(usize, String)> = Vec::new();
    let mut cuts: std::collections::HashMap<usize, u64> = std::collections::HashMap::new();
    for l in &lines {
        match l {
            Line::File { track, file, first_frame, .. } => {
                current.insert(*track, file.clone());
                if !names.contains(file) {
                    names.push(file.clone());
                    starts.push((file.clone(), *first_frame));
                    of_track.push((*track, file.clone()));
                }
            }
            Line::Cut { track, frames } => {
                cuts.insert(*track, *frames);
            }
            Line::Sync { tracks } => {
                for (track, n) in tracks {
                    let Some(f) = current.get(track) else { continue };
                    match synced.iter_mut().find(|(s, _)| s == f) {
                        Some(s) => s.1 = *n,
                        None => synced.push((f.clone(), *n)),
                    }
                }
            }
            _ => {}
        }
    }
    // The writer names only track files in this folder, in its own name format and apart in letter
    // case, so any other name means the journal was damaged: the take stays unfinished, and no file
    // is touched.
    if let Some(bad) = names.iter().find(|n| !crate::names::is_track_file(n)) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("the journal names {bad:?}, which is not a track file in the take's folder"),
        ));
    }
    for (i, a) in names.iter().enumerate() {
        if let Some(b) = names[i + 1..].iter().find(|b| b.to_lowercase() == a.to_lowercase()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("the journal names {a:?} and {b:?}, which differ only in letter case"),
            ));
        }
    }
    // A cut the writer journaled is finished here too, since the take may have stopped part way
    // through it: a part that starts at or after the end, after an earlier part of its track, is
    // removed, and the file holding the end keeps only the frames before it. A track's first file
    // is never removed.
    let start_of = |name: &str| starts.iter().find(|(f, _)| f == name).map_or(0, |(_, s)| *s);
    let mut removed: Vec<String> = Vec::new();
    let mut limits: Vec<(String, u64)> = Vec::new();
    for (track, file) in &of_track {
        let Some(&end) = cuts.get(track) else { continue };
        let start = start_of(file);
        let earlier = of_track.iter().any(|(t, f)| t == track && f != file && start_of(f) < start);
        if earlier && start >= end {
            removed.push(file.clone());
        } else {
            limits.push((file.clone(), end.saturating_sub(start)));
        }
    }
    names.retain(|n| !removed.contains(n));
    let limit = |name: &str| limits.iter().find(|(f, _)| f == name).map(|(_, l)| *l);
    // Every file is checked before any is removed or cut, so a take that stays unfinished is left
    // as it was. A part to remove may be gone already, when the take stopped after removing it;
    // one that is there must be a track file the writer made, as the kept files must. Each must
    // have one name, and no part to remove may be a kept file under another name.
    let mut removed_ids = Vec::new();
    let mut kept_ids = Vec::new();
    for name in &removed {
        let path = dir.join(name);
        match std::fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(std::io::Error::new(e.kind(), format!("{name}: {e}"))),
            Ok(m) if !m.is_file() => {
                return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{name} is not a file in the take's folder")));
            }
            Ok(_) => {
                crate::wav::check(&path).map_err(|e| std::io::Error::new(e.kind(), format!("{name}: {e}")))?;
                removed_ids.push((name, file_identity(&path)?));
            }
        }
    }
    let head = crate::wav::header_len()?;
    for name in &names {
        let path = dir.join(name);
        // Each file is made before the journal names it, so a missing one was moved or deleted
        // since: the take stays unfinished, to be repaired once the file is back.
        if !real_file(&path) {
            return Err(std::io::Error::new(std::io::ErrorKind::NotFound, format!("{name} is missing from the take's folder")));
        }
        let len = crate::wav::check(&path).map_err(|e| std::io::Error::new(e.kind(), format!("{name}: {e}")))?;
        kept_ids.push((name, file_identity(&path)?));
        // A sync put its frames on the disk, so a file holding fewer was cut or replaced since.
        let held = len.saturating_sub(head) / crate::wav::FRAME_BYTES;
        if let Some((_, want)) = synced.iter().find(|(f, _)| f == name)
            && held < limit(name).map_or(*want, |l| l.min(*want))
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{name} holds {held} frames, fewer than the {want} the journal recorded as written"),
            ));
        }
    }
    for (r, rid) in &removed_ids {
        if let Some((k, _)) = kept_ids.iter().find(|(_, kid)| kid == rid) {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{r} and {k} are the same file")));
        }
    }
    for name in &removed {
        let path = dir.join(name);
        if real_file(&path) {
            std::fs::remove_file(&path)?;
        }
    }
    let mut files = Vec::new();
    for name in names {
        let path = dir.join(&name);
        if let Some(frames) = limit(&name) {
            crate::wav::cut(&path, frames)?;
        }
        let frames = crate::wav::repair(&path)?;
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

/// Which file `p` names, opened without following a link; an error when it has more than one
/// name (hard link).
fn file_identity(p: &Path) -> std::io::Result<(u64, u64)> {
    let file = crate::open::no_follow(std::fs::OpenOptions::new().read(true), p)?;
    crate::open::one_name(&file, p).map(|i| i.id)
}

/// Whether `p` is a folder and not a link to one.
fn real_dir(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir())
}

/// Whether `p` is a file and not a link to one.
fn real_file(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.is_file())
}
