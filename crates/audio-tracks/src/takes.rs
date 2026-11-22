//! The takes in a recording folder: each `<name> Audio` folder whose `Audio Files` folder holds a
//! `take.journal`, with its start time, length, tracks, size, dropouts and markers, read from the
//! journal and the lengths of the files it names, newest first.
//!
//! The scan reads at most [`MAX_ENTRIES`] entries of the folder and looks only at
//! `<entry>/Audio Files/take.journal` in each, without going further down. Links are not followed:
//! a take folder, an `Audio Files` folder or a journal that is a link is skipped or refused. A
//! journal is read a line at a time, at most [`journal::MAX_BYTES`] bytes and [`MAX_LINE_BYTES`] per
//! line, so a take of any length is listed without holding its journal in memory; a journal that
//! names more than [`MAX_FILES`] files or more tracks than a take makes is not read further. A take
//! whose journal cannot be looked up or read, and a `<name> Audio` folder that cannot be looked
//! into, are counted in [`Listing::unreadable`] with why, and the scan goes on.
//!
//! A take still being written holds a lock on its journal. The scan holds a shared lock on each
//! journal while it reads it, so a take whose writer holds the lock is listed as
//! [`TakeState::Recording`] without its journal being read, and a repair that starts while the scan
//! reads a journal leaves that take for the next repair.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::journal::{self, Line};
use crate::session::FILES_FOLDER;

/// Entries of the folder the scan reads at most.
pub const MAX_ENTRIES: usize = 500;

/// Bytes one journal line may hold. The longest line the writer makes is a sync or repair line
/// for [`crate::MAX_TAKE_TRACKS`] tracks, a few KiB.
pub const MAX_LINE_BYTES: usize = 1 << 20;

/// Files one journal may name: 1,024 parts for each of [`crate::MAX_TAKE_TRACKS`] tracks. A part
/// holds at least 1 GiB, about 46 minutes, so one track reaches the limit after about 23 years, and
/// a take of many tracks fills its journal's [`journal::MAX_BYTES`] first.
pub const MAX_FILES: usize = 1024 * crate::MAX_TAKE_TRACKS;

/// Whether a take finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TakeState {
    /// The journal ends with its `End` line: every file is complete.
    Finished,
    /// The journal has no `End` line and nothing holds its lock: the app or the computer stopped
    /// during the take, and [`crate::recover::repair`] finishes its files.
    NeedsRepair,
    /// The journal is locked: the take is being written or repaired now.
    Recording,
}

/// One take in the folder.
#[derive(Clone, Debug, PartialEq)]
pub struct TakeInfo {
    /// The take's name: its folder's name without ` Audio`.
    pub name: String,
    /// The take's `Audio Files` folder.
    pub dir: PathBuf,
    /// The take's session file, when it is a file beside the `Audio Files` folder.
    pub session: Option<PathBuf>,
    /// The wall-clock time of take frame 0 in milliseconds since 1970, from the journal, or the
    /// journal's last change when the journal does not give it.
    pub started_unix_ms: Option<i64>,
    /// Samples per second of the take's files.
    pub sample_rate: u32,
    /// Frames the take holds: the `End` line's count when it finished, otherwise the end of the
    /// file that ends last.
    pub frames: u64,
    /// Tracks the take made, the program mix included.
    pub tracks: usize,
    /// Bytes of the files the journal names, and of the journal.
    pub bytes: u64,
    /// Files the journal names that are not files in the take's folder, other than parts that the
    /// take's end removed: deleted, moved, replaced by a link, or named outside the folder.
    pub missing_files: usize,
    /// Dropouts the journal lists.
    pub dropouts: u64,
    /// Frames of audio lost, listed as dropouts or not.
    pub lost_frames: u64,
    /// Markers set during the take.
    pub markers: u64,
    pub state: TakeState,
    /// Whether the take was repaired after it ended without its journal's last line.
    pub repaired: bool,
    /// Why the take stopped early, in English, when it did.
    pub stopped: Option<String>,
}

impl TakeInfo {
    /// Seconds of audio the take holds.
    pub fn seconds(&self) -> f64 {
        self.frames as f64 / f64::from(self.sample_rate.max(1))
    }
}

/// What a scan of a folder found.
#[derive(Debug, Default)]
pub struct Listing {
    /// The takes, newest first; takes with no start time come last, by name.
    pub takes: Vec<TakeInfo>,
    /// Each take whose journal could not be read, with why.
    pub unreadable: Vec<(PathBuf, String)>,
    /// Whether the folder holds more than [`MAX_ENTRIES`] entries, so some were not looked at.
    pub more: bool,
}

/// The takes in `folder`, newest first (see the module documentation). A folder that cannot be
/// read gives an empty listing with the folder counted as unreadable.
pub fn list(folder: &Path) -> Listing {
    let mut out = Listing::default();
    let entries = match std::fs::read_dir(folder) {
        Ok(e) => e,
        Err(e) => {
            out.unreadable.push((folder.to_path_buf(), e.to_string()));
            return out;
        }
    };
    for (seen, entry) in entries.enumerate() {
        if seen == MAX_ENTRIES {
            out.more = true;
            break;
        }
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                out.unreadable.push((folder.to_path_buf(), e.to_string()));
                continue;
            }
        };
        // The entry's own type: a link to a folder is not a folder here.
        match entry.file_type() {
            Ok(t) if t.is_dir() => {}
            Ok(_) => continue,
            Err(e) => {
                out.unreadable.push((entry.path(), e.to_string()));
                continue;
            }
        }
        let take = entry.path();
        let dir = take.join(FILES_FOLDER);
        match std::fs::symlink_metadata(&dir) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => continue,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            // Only a folder named as a take folder is counted: other folders in the recording
            // folder are not the app's.
            Err(e) => {
                if entry.file_name().to_string_lossy().ends_with(" Audio") {
                    out.unreadable.push((take, e.to_string()));
                }
                continue;
            }
        }
        match std::fs::symlink_metadata(dir.join(journal::FILE_NAME)) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                out.unreadable.push((dir, e.to_string()));
                continue;
            }
        }
        match read_take(&take, &dir) {
            Ok(info) => out.takes.push(info),
            Err(e) => out.unreadable.push((dir, e.to_string())),
        }
    }
    out.takes.sort_by(|a, b| match (a.started_unix_ms, b.started_unix_ms) {
        (Some(x), Some(y)) => y.cmp(&x).then_with(|| a.name.cmp(&b.name)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.name.cmp(&b.name),
    });
    out.unreadable.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Totals of a journal's lines.
#[derive(Default)]
struct Totals {
    sample_rate: Option<u32>,
    started_unix_ms: Option<i64>,
    /// Each file the journal names, the take frame it starts at, and its track.
    files: Vec<(String, u64, usize)>,
    names: std::collections::HashSet<String>,
    tracks: std::collections::HashSet<usize>,
    /// The take frame each track's first file starts at.
    first_start: std::collections::HashMap<usize, u64>,
    /// The take frame each track's files were cut back to.
    cuts: std::collections::HashMap<usize, u64>,
    dropouts: u64,
    lost_frames: u64,
    markers: u64,
    end: Option<u64>,
    repaired: bool,
    stopped: Option<String>,
}

impl Totals {
    /// Add `line`. An error when the journal names more tracks than a take makes or more than
    /// [`MAX_FILES`] files.
    fn add(&mut self, line: Line) -> Result<(), String> {
        match line {
            Line::Start { sample_rate, started_unix_ms, .. } => {
                self.sample_rate = Some(sample_rate);
                self.started_unix_ms = started_unix_ms;
            }
            Line::File { track, file, first_frame, .. } => {
                if self.tracks.insert(track) && self.tracks.len() > crate::MAX_TAKE_TRACKS {
                    return Err(format!("the journal names more than {} tracks", crate::MAX_TAKE_TRACKS));
                }
                if !self.names.contains(&file) {
                    if self.files.len() == MAX_FILES {
                        return Err(format!("the journal names more than {MAX_FILES} files"));
                    }
                    self.names.insert(file.clone());
                    self.files.push((file, first_frame, track));
                    let first = self.first_start.entry(track).or_insert(first_frame);
                    *first = (*first).min(first_frame);
                }
            }
            // A cut names a track the take made, so the tracks' limit bounds these too.
            Line::Cut { track, frames } => {
                if self.cuts.len() < crate::MAX_TAKE_TRACKS || self.cuts.contains_key(&track) {
                    self.cuts.insert(track, frames);
                }
            }
            Line::Marker { .. } => self.markers = self.markers.saturating_add(1),
            Line::Dropout { frames, .. } => {
                self.dropouts = self.dropouts.saturating_add(1);
                self.lost_frames = self.lost_frames.saturating_add(frames);
            }
            Line::Unlisted { frames } => self.lost_frames = self.lost_frames.saturating_add(frames),
            Line::Stop { reason } => self.stopped = Some(reason),
            Line::Repaired { .. } => self.repaired = true,
            Line::End { frames } => self.end = Some(frames),
            Line::Sync { .. } | Line::Rename { .. } => {}
        }
        Ok(())
    }

    /// Whether the take's end removed the file starting at `start` on `track`: a part after the
    /// track's first that starts at or after the frame the track was cut back to, as
    /// [`crate::recover::repair`] removes it.
    fn cut_away(&self, track: usize, start: u64) -> bool {
        let earlier = self.first_start.get(&track).is_some_and(|f| *f < start);
        earlier && self.cuts.get(&track).is_some_and(|end| start >= *end)
    }
}

/// The take in the folder `take`, whose `Audio Files` folder is `dir`.
fn read_take(take: &Path, dir: &Path) -> std::io::Result<TakeInfo> {
    let path = dir.join(journal::FILE_NAME);
    let mut file = crate::open::no_follow(OpenOptions::new().read(true), &path)?;
    let meta = file.metadata()?;
    let changed_ms = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).and_then(|d| i64::try_from(d.as_millis()).ok());
    let folder = take.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let name = folder.strip_suffix(" Audio").filter(|s| !s.is_empty()).unwrap_or(&folder).to_string();
    let session = crate::session::path_for(dir).filter(|p| std::fs::symlink_metadata(p).is_ok_and(|m| m.is_file()));
    let mut info = TakeInfo {
        name,
        dir: dir.to_path_buf(),
        session,
        started_unix_ms: changed_ms,
        sample_rate: studiocraft_media::audio::SAMPLE_RATE,
        frames: 0,
        tracks: 0,
        bytes: meta.len(),
        missing_files: 0,
        dropouts: 0,
        lost_frames: 0,
        markers: 0,
        state: TakeState::Recording,
        repaired: false,
        stopped: None,
    };
    match file.try_lock_shared() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Ok(info),
        Err(std::fs::TryLockError::Error(e)) => return Err(e),
    }
    // Read through the locked handle: on Windows another handle cannot read a locked file.
    let totals = read_lines(&mut file)?;
    let rate = totals.sample_rate.filter(|r| *r > 0).unwrap_or(studiocraft_media::audio::SAMPLE_RATE);
    let head = crate::wav::header_len()?;
    // On Windows a name with `~` can be another file's short name, which a listing of the folder
    // does not show, so such a name counts only when the folder lists it. When the folder cannot
    // be listed, such names are neither counted nor called missing.
    let listed = listed_tilde_names(dir, totals.files.iter().map(|(n, _, _)| n.as_str()));
    let mut file_end = 0u64;
    let mut missing = 0usize;
    // The files are looked at under the journal's lock, so a repair does not change them meanwhile.
    for (name, first, track) in &totals.files {
        let gone = |missing: &mut usize| {
            if !totals.cut_away(*track, *first) {
                *missing += 1;
            }
        };
        // Only a name in the writer's format is a file in the take's folder.
        // Another name was never one of the take's files, so no cut removed it.
        if !crate::names::is_track_file(name) {
            missing += 1;
            continue;
        }
        if name.contains('~') {
            match &listed {
                Some(l) if l.contains(&name.to_lowercase()) => {}
                Some(_) => {
                    gone(&mut missing);
                    continue;
                }
                None => continue,
            }
        }
        let meta = match std::fs::symlink_metadata(dir.join(name)) {
            Ok(m) if m.is_file() => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                gone(&mut missing);
                continue;
            }
            // A link or a folder in a file's place, or a file that cannot be looked up, was not
            // removed by the take's end.
            _ => {
                missing += 1;
                continue;
            }
        };
        info.bytes = info.bytes.saturating_add(meta.len());
        file_end = file_end.max(first.saturating_add(meta.len().saturating_sub(head) / crate::wav::FRAME_BYTES));
    }
    drop(file);
    info.missing_files = missing;
    info.sample_rate = rate;
    info.started_unix_ms = totals.started_unix_ms.or(changed_ms);
    info.frames = totals.end.unwrap_or(file_end);
    info.tracks = totals.tracks.len();
    info.dropouts = totals.dropouts;
    info.lost_frames = totals.lost_frames;
    info.markers = totals.markers;
    info.state = if totals.end.is_some() { TakeState::Finished } else { TakeState::NeedsRepair };
    info.repaired = totals.repaired;
    info.stopped = totals.stopped;
    Ok(info)
}

/// The names in `files` that hold `~` and that a listing of `dir` shows, without letter case, or
/// `None` when the folder cannot be listed. The whole folder is looked through, keeping only those
/// names.
fn listed_tilde_names<'a>(dir: &Path, files: impl Iterator<Item = &'a str>) -> Option<std::collections::HashSet<String>> {
    let wanted: std::collections::HashSet<String> = files.filter(|n| n.contains('~')).map(str::to_lowercase).collect();
    let mut found = std::collections::HashSet::new();
    if wanted.is_empty() {
        return Some(found);
    }
    // An entry that cannot be read could be any of the names, so then none of them is known.
    for entry in std::fs::read_dir(dir).ok()? {
        let Ok(name) = entry.ok()?.file_name().into_string() else { continue };
        let name = name.to_lowercase();
        if wanted.contains(&name) {
            found.insert(name);
            if found.len() == wanted.len() {
                break;
            }
        }
    }
    Some(found)
}

/// The totals of the journal lines in `file`, read a line at a time. A last line cut short by a
/// crash, which has no newline, is left out when it does not read; any other line that does not
/// read is an error, and so is a journal longer than [`journal::MAX_BYTES`] or a line longer than
/// [`MAX_LINE_BYTES`].
fn read_lines(file: &mut File) -> std::io::Result<Totals> {
    let invalid = |e: String| std::io::Error::new(std::io::ErrorKind::InvalidData, e);
    if file.metadata()?.len() > journal::MAX_BYTES {
        return Err(invalid(format!("journal is larger than {} bytes", journal::MAX_BYTES)));
    }
    let mut reader = BufReader::new(file.take(journal::MAX_BYTES + 1));
    let mut totals = Totals::default();
    let mut buf = Vec::new();
    let mut read = 0u64;
    for n in 1u64.. {
        buf.clear();
        let got = (&mut reader).take(MAX_LINE_BYTES as u64 + 1).read_until(b'\n', &mut buf)?;
        if got == 0 {
            break;
        }
        read += got as u64;
        if read > journal::MAX_BYTES {
            return Err(invalid(format!("journal is larger than {} bytes", journal::MAX_BYTES)));
        }
        let whole = buf.last() == Some(&b'\n');
        if !whole && buf.len() > MAX_LINE_BYTES {
            return Err(invalid(format!("journal line {n} is longer than {MAX_LINE_BYTES} bytes")));
        }
        if buf.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match serde_json::from_slice::<Line>(&buf) {
            Ok(line) => totals.add(line).map_err(invalid)?,
            // The last line, cut short by a crash.
            Err(_) if !whole => break,
            Err(e) => return Err(invalid(format!("journal line {n}: {e}"))),
        }
    }
    Ok(totals)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::Journal;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("studiocraft-takes-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn start(started_unix_ms: Option<i64>) -> Line {
        Line::Start {
            version: 1,
            sample_rate: 48_000,
            channels: 2,
            anchor: "recording".into(),
            origin_tick: Some(0),
            offset_frames: 0,
            started_unix_ms,
            session: true,
        }
    }

    fn file(track: usize, name: &str, first_frame: u64) -> Line {
        Line::File { track, name: name.into(), file: format!("{name}.wav"), segment: 1, first_frame, program: track == 0 }
    }

    /// A take folder `<stem> Audio` in `folder` with a journal of `lines` and, for each `(file,
    /// frames)`, a file of that many frames after a header. Its `Audio Files` folder.
    fn take(folder: &Path, stem: &str, lines: &[Line], files: &[(&str, u64)]) -> PathBuf {
        let dir = folder.join(format!("{stem} Audio")).join(FILES_FOLDER);
        std::fs::create_dir_all(&dir).unwrap();
        let mut j = Journal::create(&dir).unwrap();
        for l in lines {
            j.write(l).unwrap();
        }
        drop(j);
        let head = crate::wav::header_len().unwrap();
        for (name, frames) in files {
            std::fs::write(dir.join(name), vec![0u8; (head + frames * crate::wav::FRAME_BYTES) as usize]).unwrap();
        }
        dir
    }

    #[test]
    fn takes_are_listed_newest_first_with_their_length_tracks_dropouts_and_markers() {
        let folder = scratch("listed");
        let head = crate::wav::header_len().unwrap();
        let old = take(
            &folder,
            "2026-11-13 19-00-00",
            &[
                start(Some(1_000)),
                file(0, "00 Program", 0),
                file(1, "Mic", 0),
                Line::Dropout { track: 1, at: 10, frames: 480 },
                Line::Unlisted { frames: 20 },
                Line::Marker { at: 5, name: "Intro".into(), number: Some(1) },
                Line::Marker { at: 50, name: "Goal".into(), number: Some(2) },
                Line::Rename { number: 2, name: "Kickoff".into() },
                Line::End { frames: 96_000 },
            ],
            &[("00 Program.wav", 96_000), ("Mic.wav", 96_000)],
        );
        std::fs::write(crate::session::path_for(&old).unwrap(), b"{}").unwrap();
        // A crash: no end line, a second part, and a last line cut short.
        let new = take(
            &folder,
            "2026-11-20 19-02-00",
            &[start(Some(2_000)), file(0, "00 Program", 0), file(1, "Mic", 0), file(1, "Mic part 2", 48_000)],
            &[("00 Program.wav", 60_000), ("Mic.wav", 48_000), ("Mic part 2.wav", 24_000)],
        );
        let mut text = std::fs::read(new.join(journal::FILE_NAME)).unwrap();
        text.extend_from_slice(b"{\"sync\":{\"tr");
        std::fs::write(new.join(journal::FILE_NAME), text).unwrap();
        // Not takes: a file, a folder without a journal.
        std::fs::write(folder.join("Show.mkv"), b"video").unwrap();
        std::fs::create_dir_all(folder.join("Empty Audio").join(FILES_FOLDER)).unwrap();

        let got = list(&folder);
        assert!(got.unreadable.is_empty(), "{:?}", got.unreadable);
        assert!(!got.more);
        let names: Vec<&str> = got.takes.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["2026-11-20 19-02-00", "2026-11-13 19-00-00"], "newest first");
        let n = &got.takes[0];
        assert_eq!(n.state, TakeState::NeedsRepair);
        assert_eq!(n.frames, 72_000, "the end of the part that ends last");
        assert_eq!((n.tracks, n.dropouts, n.markers, n.session.is_some(), n.missing_files), (2, 0, 0, false, 0));
        let o = &got.takes[1];
        assert_eq!(o.state, TakeState::Finished);
        assert_eq!(o.started_unix_ms, Some(1_000));
        assert_eq!(o.frames, 96_000);
        assert!((o.seconds() - 2.0).abs() < 1e-9);
        assert_eq!((o.tracks, o.dropouts, o.lost_frames, o.markers), (2, 1, 500, 2));
        assert_eq!(o.session, crate::session::path_for(&old));
        let journal_len = std::fs::metadata(old.join(journal::FILE_NAME)).unwrap().len();
        assert_eq!(o.bytes, journal_len + 2 * (head + 96_000 * crate::wav::FRAME_BYTES));
        assert_eq!(o.dir, old);
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn a_take_whose_journal_does_not_read_is_counted_and_the_others_are_listed() {
        let folder = scratch("unreadable");
        take(&folder, "Good", &[start(Some(5)), file(0, "00 Program", 0), Line::End { frames: 0 }], &[]);
        let bad = folder.join("Bad Audio").join(FILES_FOLDER);
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join(journal::FILE_NAME), b"not json\n{\"end\":{\"frames\":1}}\n").unwrap();
        let long = folder.join("Long Audio").join(FILES_FOLDER);
        std::fs::create_dir_all(&long).unwrap();
        std::fs::write(long.join(journal::FILE_NAME), vec![b' '; MAX_LINE_BYTES + 10]).unwrap();
        let got = list(&folder);
        assert_eq!(got.takes.len(), 1);
        assert_eq!(got.takes[0].name, "Good");
        let dirs: Vec<&Path> = got.unreadable.iter().map(|(d, _)| d.as_path()).collect();
        assert_eq!(dirs, [bad.as_path(), long.as_path()]);
        assert!(got.unreadable[0].1.contains("journal line 1"), "{:?}", got.unreadable);
        assert!(got.unreadable[1].1.contains("longer than"), "{:?}", got.unreadable);
        // A folder that is not there is counted too.
        let none = list(&folder.join("missing"));
        assert!(none.takes.is_empty());
        assert_eq!(none.unreadable.len(), 1);
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn a_journal_naming_more_files_or_tracks_than_a_take_makes_is_unreadable() {
        let folder = scratch("too-many");
        let named = |n: usize, track: usize| Line::File {
            track,
            name: "Mic".into(),
            file: format!("Mic part {n}.wav"),
            segment: 1,
            first_frame: 0,
            program: false,
        };
        let at_limit: Vec<Line> = (0..MAX_FILES).map(|n| named(n, n % crate::MAX_TAKE_TRACKS)).collect();
        take(&folder, "Full", &at_limit, &[]);
        let mut files: Vec<Line> = (0..=MAX_FILES).map(|n| named(n, 0)).collect();
        // Naming a file again adds nothing.
        files.push(named(0, 0));
        let many_files = take(&folder, "Files", &files, &[]);
        let tracks: Vec<Line> = (0..=crate::MAX_TAKE_TRACKS).map(|n| named(0, n)).collect();
        let many_tracks = take(&folder, "Tracks", &tracks, &[]);
        let got = list(&folder);
        let names: Vec<&str> = got.takes.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["Full"], "{:?}", got.unreadable);
        assert_eq!(got.takes[0].tracks, crate::MAX_TAKE_TRACKS);
        let why: Vec<(&Path, &str)> = got.unreadable.iter().map(|(d, e)| (d.as_path(), e.as_str())).collect();
        assert_eq!(why.len(), 2, "{why:?}");
        assert_eq!(why[0].0, many_files.as_path());
        assert!(why[0].1.contains(&format!("more than {MAX_FILES} files")), "{why:?}");
        assert_eq!(why[1].0, many_tracks.as_path());
        assert!(why[1].1.contains("tracks"), "{why:?}");
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[cfg(unix)]
    #[test]
    fn a_journal_that_cannot_be_looked_up_is_counted() {
        use std::os::unix::fs::PermissionsExt;
        let folder = scratch("no-access");
        let dir = take(&folder, "Locked", &[start(Some(1)), Line::End { frames: 0 }], &[]);
        let shut = take(&folder, "Shut", &[start(Some(1)), Line::End { frames: 0 }], &[]);
        let shut = shut.parent().unwrap().to_path_buf();
        // A folder that is not named as a take folder is not counted.
        let other = folder.join("Other");
        std::fs::create_dir_all(other.join(FILES_FOLDER)).unwrap();
        let mode = |p: &Path, m: u32| std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap();
        mode(&dir, 0o000);
        mode(&shut, 0o000);
        mode(&other, 0o000);
        // A user that reads every file anyway, as root does, finds the journal.
        let readable = std::fs::symlink_metadata(dir.join(journal::FILE_NAME)).is_ok();
        let got = list(&folder);
        mode(&dir, 0o755);
        mode(&shut, 0o755);
        mode(&other, 0o755);
        if readable {
            eprintln!("skipped: this user reads a folder without permission");
        } else {
            assert!(got.takes.is_empty());
            let dirs: Vec<&Path> = got.unreadable.iter().map(|(d, _)| d.as_path()).collect();
            assert_eq!(dirs, [dir.as_path(), shut.as_path()]);
        }
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn the_scan_stops_at_its_entry_limit() {
        let folder = scratch("limit");
        for n in 0..MAX_ENTRIES + 3 {
            std::fs::write(folder.join(format!("file {n}")), b"").unwrap();
        }
        let got = list(&folder);
        assert!(got.more);
        assert!(got.takes.is_empty());
        let _ = std::fs::remove_dir_all(&folder);
        let folder = scratch("at-limit");
        for n in 0..MAX_ENTRIES {
            std::fs::write(folder.join(format!("file {n}")), b"").unwrap();
        }
        assert!(!list(&folder).more, "exactly the limit is all of it");
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn a_take_being_written_is_listed_as_recording_without_reading_its_journal() {
        let folder = scratch("recording");
        let dir = folder.join("Live Audio").join(FILES_FOLDER);
        std::fs::create_dir_all(&dir).unwrap();
        let mut j = Journal::create(&dir).unwrap();
        j.write(&start(Some(9))).unwrap();
        j.write(&file(0, "00 Program", 0)).unwrap();
        j.sync().unwrap();
        let got = list(&folder);
        assert_eq!(got.takes.len(), 1, "{:?}", got.unreadable);
        assert_eq!(got.takes[0].state, TakeState::Recording);
        assert_eq!(got.takes[0].tracks, 0, "the journal is not read while it is locked");
        // Once the take ends and releases the lock, the same handle still open, it reads.
        j.write(&Line::End { frames: 0 }).unwrap();
        j.sync().unwrap();
        j.release().unwrap();
        let got = list(&folder);
        assert_eq!(got.takes[0].state, TakeState::Finished);
        assert_eq!(got.takes[0].tracks, 1);
        drop(j);
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn a_take_with_no_start_time_goes_last_and_a_name_without_audio_is_kept() {
        let folder = scratch("no-start");
        take(&folder, "Timed", &[start(Some(i64::MAX)), Line::End { frames: 0 }], &[]);
        let plain = folder.join("Plain").join(FILES_FOLDER);
        std::fs::create_dir_all(&plain).unwrap();
        std::fs::write(plain.join(journal::FILE_NAME), b"{\"end\":{\"frames\":0}}\n").unwrap();
        let got = list(&folder);
        let names: Vec<&str> = got.takes.iter().map(|t| t.name.as_str()).collect();
        // The journal's change time stands in for a missing start time; a journal written now is
        // older than the latest possible start.
        assert_eq!(names, ["Timed", "Plain"]);
        assert!(got.takes[1].started_unix_ms.is_some());
        assert_eq!(got.takes[1].frames, 0);
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn names_outside_the_take_folder_add_no_size() {
        let folder = scratch("outside");
        let dir = take(
            &folder,
            "Show",
            &[start(Some(1)), Line::File { track: 0, name: "x".into(), file: "../../secret.wav".into(), segment: 1, first_frame: 0, program: false }],
            &[],
        );
        std::fs::write(folder.join("secret.wav"), vec![0u8; 4096]).unwrap();
        let got = list(&folder);
        let journal_len = std::fs::metadata(dir.join(journal::FILE_NAME)).unwrap().len();
        assert_eq!(got.takes[0].bytes, journal_len);
        assert_eq!(got.takes[0].frames, 0);
        assert_eq!(got.takes[0].missing_files, 1, "a name outside the folder is not a file of the take");
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn a_deleted_file_is_missing_and_a_part_the_end_removed_is_not() {
        let folder = scratch("missing");
        let lines = [
            start(Some(1)),
            file(1, "Mic", 0),
            file(1, "Mic part 2", 48_000),
            file(1, "Mic part 3", 96_000),
            file(1, "Mic part 4", 120_000),
            Line::File { track: 1, name: "Mic".into(), file: "../Mic part 5.wav".into(), segment: 5, first_frame: 150_000, program: false },
            file(2, "Late", 70_000),
            Line::Cut { track: 1, frames: 60_000 },
            Line::Cut { track: 2, frames: 60_000 },
            Line::End { frames: 60_000 },
        ];
        let dir = take(&folder, "Show", &lines, &[("Mic.wav", 48_000)]);
        std::fs::create_dir(dir.join("Mic part 3.wav")).unwrap();
        let got = list(&folder);
        // Part 2 starts before the end and was deleted; a track's first file is never removed; a
        // folder in part 3's place was not removed by the end; a name outside the folder was never
        // a part. Part 4 starts after the end, so the end removed it.
        assert_eq!(got.takes[0].missing_files, 4);
        assert_eq!(got.takes[0].state, TakeState::Finished);
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn a_name_with_a_tilde_counts_when_the_folder_lists_it() {
        let folder = scratch("tilde");
        let head = crate::wav::header_len().unwrap();
        let lines = [start(Some(1)), file(1, "Old~Mic", 0), file(2, "Gone~1", 0)];
        let dir = take(&folder, "Show", &lines, &[("Old~Mic.wav", 100)]);
        let got = list(&folder);
        let journal_len = std::fs::metadata(dir.join(journal::FILE_NAME)).unwrap().len();
        assert_eq!(got.takes[0].bytes, journal_len + head + 100 * crate::wav::FRAME_BYTES);
        assert_eq!(got.takes[0].frames, 100);
        assert_eq!(got.takes[0].tracks, 2);
        assert_eq!(got.takes[0].missing_files, 1, "the name the folder does not list");
        // A folder whose files open but that cannot be listed still gives the take, without the
        // names it could not check.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o100)).unwrap();
            let lists = std::fs::read_dir(&dir).is_ok();
            let got = list(&folder);
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
            if lists {
                eprintln!("skipped: this user lists a folder without permission");
            } else {
                assert!(got.unreadable.is_empty(), "{:?}", got.unreadable);
                assert_eq!(got.takes[0].bytes, journal_len);
                assert_eq!(got.takes[0].tracks, 2);
                assert_eq!(got.takes[0].missing_files, 0, "names that could not be checked are not called missing");
            }
        }
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[cfg(unix)]
    #[test]
    fn links_are_not_followed() {
        let folder = scratch("links");
        let elsewhere = scratch("links-elsewhere");
        let real = take(&elsewhere, "Real", &[start(Some(1)), Line::End { frames: 0 }], &[]);
        // A take folder that is a link, an Audio Files folder that is a link, and a journal that is one.
        std::os::unix::fs::symlink(real.parent().unwrap(), folder.join("Linked Audio")).unwrap();
        std::fs::create_dir_all(folder.join("Files Audio")).unwrap();
        std::os::unix::fs::symlink(&real, folder.join("Files Audio").join(FILES_FOLDER)).unwrap();
        let j = folder.join("Journal Audio").join(FILES_FOLDER);
        std::fs::create_dir_all(&j).unwrap();
        std::os::unix::fs::symlink(real.join(journal::FILE_NAME), j.join(journal::FILE_NAME)).unwrap();
        let got = list(&folder);
        assert!(got.takes.is_empty(), "{:?}", got.takes);
        let dirs: Vec<&Path> = got.unreadable.iter().map(|(d, _)| d.as_path()).collect();
        assert_eq!(dirs, [j.as_path()], "a journal that is a link is refused");
        let _ = std::fs::remove_dir_all(&folder);
        let _ = std::fs::remove_dir_all(&elsewhere);
    }

    #[cfg(windows)]
    #[test]
    fn links_are_not_followed() {
        let folder = scratch("win-links");
        let elsewhere = scratch("win-links-elsewhere");
        let real = take(&elsewhere, "Real", &[start(Some(1)), Line::End { frames: 0 }], &[]);
        // Making a symbolic link needs Developer Mode or the privilege to make links.
        if let Err(e) = std::os::windows::fs::symlink_dir(real.parent().unwrap(), folder.join("Linked Audio")) {
            eprintln!("skipped: a symbolic link could not be made: {e}");
            let _ = std::fs::remove_dir_all(&folder);
            let _ = std::fs::remove_dir_all(&elsewhere);
            return;
        }
        std::fs::create_dir_all(folder.join("Files Audio")).unwrap();
        std::os::windows::fs::symlink_dir(&real, folder.join("Files Audio").join(FILES_FOLDER)).unwrap();
        let j = folder.join("Journal Audio").join(FILES_FOLDER);
        std::fs::create_dir_all(&j).unwrap();
        std::os::windows::fs::symlink_file(real.join(journal::FILE_NAME), j.join(journal::FILE_NAME)).unwrap();
        let got = list(&folder);
        assert!(got.takes.is_empty(), "{:?}", got.takes);
        let dirs: Vec<&Path> = got.unreadable.iter().map(|(d, _)| d.as_path()).collect();
        assert_eq!(dirs, [j.as_path()], "a journal that is a link is refused");
        let _ = std::fs::remove_dir_all(&folder);
        let _ = std::fs::remove_dir_all(&elsewhere);
    }
}
