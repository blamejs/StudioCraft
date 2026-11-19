//! `take.journal`: one JSON object per line, appended by the disk thread, read back by repair.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

/// The journal's file name in a take's folder.
pub const FILE_NAME: &str = "take.journal";

/// The largest journal that is read back whole. A sync line lists only the tracks whose files
/// grew, by track number, so a take of [`crate::MAX_TRACKS`] tracks writes about 1 KiB every
/// 5 seconds: about 17 MiB a day, so the limit covers a take of about two weeks.
pub const MAX_BYTES: u64 = 256 << 20;

/// Bytes from the end of the journal that [`ended`] reads.
const TAIL_BYTES: u64 = 64 << 10;

/// One line of the journal.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Line {
    /// The take began; the files are 32-bit float WAV at this rate and channel count.
    ///
    /// `anchor` names the timeline the files are placed on: `recording`, `stream`, or `none` when
    /// the take follows no output. Take frame 0 is `offset_frames` frames into that timeline, at
    /// `origin_tick` on the studio's clock (none when frame 0 is the first block the take got).
    /// Each file's `bext` time reference is `offset_frames` plus the file's `first_frame`.
    /// `started_unix_ms` is the wall-clock time of take frame 0, when known.
    Start {
        version: u32,
        sample_rate: u32,
        channels: u16,
        anchor: String,
        origin_tick: Option<i64>,
        offset_frames: u64,
        started_unix_ms: Option<i64>,
    },
    /// A track's file was opened. `first_frame` is the take frame its first sample is at.
    File { track: usize, name: String, file: String, segment: u32, first_frame: u64 },
    /// After a sync: frames each track's current file held on disk, for the tracks whose files
    /// grew since the last sync.
    Sync { tracks: Vec<(usize, u64)> },
    /// Audio lost before the disk; the track's file holds silence there.
    Dropout { track: usize, at: u64, frames: u64 },
    /// Frames of lost audio that have no `Dropout` line, because the list of dropouts waiting for
    /// the disk thread was full.
    Unlisted { frames: u64 },
    /// The track's files are cut back to take frame `frames`, the take's end time: a part that
    /// starts there or later is removed and the part holding it is shortened. Written before the
    /// files are changed, so repair can finish a cut that was cut short.
    Cut { track: usize, frames: u64 },
    /// The take stopped early.
    Stop { reason: String },
    /// Files repaired after the take ended without its last line, with the frames each holds.
    Repaired { files: Vec<(String, u64)> },
    /// The take ended and every file is complete.
    End { frames: u64 },
}

/// An open journal.
pub struct Journal {
    file: File,
    /// The length of the lines written whole: a write that fails is cut back to it.
    len: u64,
    /// Bytes after which the next write fails, for the tests of a write cut short.
    #[cfg(test)]
    fail_after: Option<usize>,
}

impl Journal {
    fn new(file: File, len: u64) -> Journal {
        Journal {
            file,
            len,
            #[cfg(test)]
            fail_after: None,
        }
    }

    /// Create the journal in `dir`, locked for as long as the journal is open. [`Journal::open_idle`]
    /// skips a locked journal, so repair never touches a take that is still being written.
    pub fn create(dir: &Path) -> std::io::Result<Journal> {
        // Write access, not append: cutting back a failed write sets the length, which needs it
        // on Windows.
        let file = OpenOptions::new().create_new(true).read(true).write(true).open(dir.join(FILE_NAME))?;
        file.lock()?;
        Ok(Journal::new(file, 0))
    }

    /// Open the journal in `dir` to append to it, with its lines, locked while it is open. `None`
    /// when another journal handle holds the lock: the take is still being written or repaired.
    /// A last line cut short by a crash is removed, so the lines appended next read back. A
    /// journal that is a link, or that has more than one name (hard link), is refused.
    pub fn open_idle(dir: &Path) -> std::io::Result<Option<(Journal, Vec<Line>)>> {
        let path = dir.join(FILE_NAME);
        let mut file = crate::open::no_follow(OpenOptions::new().read(true).write(true), &path)?;
        crate::open::one_name(&file, &path)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Ok(None),
            Err(std::fs::TryLockError::Error(e)) => return Err(e),
        }
        // Read through the locked handle: on Windows another handle cannot read a locked file.
        let bytes = read_bounded(&mut file)?;
        let (lines, cut) = parse(&bytes)?;
        let whole = whole_len(&bytes);
        if cut {
            file.set_len(whole as u64)?;
        }
        let mut len = file.seek(SeekFrom::End(0))?;
        if !cut && whole < bytes.len() {
            // The last line read but has no newline: end it so the next line starts on its own.
            file.write_all(b"\n")?;
            len += 1;
        }
        Ok(Some((Journal::new(file, len), lines)))
    }

    /// Append `line`. It reaches the disk with the next [`Journal::sync`]. A write that fails
    /// part way, as on a full disk, is cut back, so the journal holds only whole lines and the
    /// next line starts on its own.
    pub fn write(&mut self, line: &Line) -> std::io::Result<()> {
        let mut text = serde_json::to_string(line).map_err(std::io::Error::other)?;
        text.push('\n');
        match self.write_bytes(text.as_bytes()) {
            Ok(()) => {
                self.len += text.len() as u64;
                Ok(())
            }
            Err(e) => {
                // The cut is best effort: the error that stopped the write is the one reported.
                if self.file.set_len(self.len).is_ok() {
                    let _ = self.file.seek(SeekFrom::Start(self.len));
                }
                Err(e)
            }
        }
    }

    fn write_bytes(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        #[cfg(test)]
        if let Some(n) = self.fail_after.take() {
            self.file.write_all(bytes.get(..n.min(bytes.len())).unwrap_or_default())?;
            return Err(std::io::Error::new(std::io::ErrorKind::StorageFull, "no space left on the disk"));
        }
        self.file.write_all(bytes)
    }

    pub fn sync(&mut self) -> std::io::Result<()> {
        self.file.sync_data()
    }
}

/// The lines of the journal in `dir`. A last line cut short by a crash is left out; other lines
/// that do not read are an error, and so is a journal that is a link.
pub fn read(dir: &Path) -> std::io::Result<Vec<Line>> {
    parse(&read_bounded(&mut crate::open::no_follow(OpenOptions::new().read(true), &dir.join(FILE_NAME))?)?).map(|(lines, _)| lines)
}

/// Whether the journal in `dir` ends with its `End` line, read from its last 64 KiB only, so a
/// journal of any length is answered without reading it whole.
pub fn ended(dir: &Path) -> std::io::Result<bool> {
    let mut file = crate::open::no_follow(OpenOptions::new().read(true), &dir.join(FILE_NAME))?;
    let len = file.metadata()?.len();
    let from = len.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(from))?;
    let mut tail = Vec::with_capacity((len - from) as usize);
    (&mut file).take(TAIL_BYTES).read_to_end(&mut tail)?;
    let last = tail.split(|&b| b == b'\n').rev().find(|l| !l.iter().all(u8::is_ascii_whitespace));
    Ok(last.and_then(|l| serde_json::from_slice::<Line>(l).ok()).is_some_and(|l| matches!(l, Line::End { .. })))
}

/// The bytes of `file` from its start, or an error when it holds more than [`MAX_BYTES`]. The size
/// is checked before anything is read, and the read stops at the limit if the file grows.
fn read_bounded(file: &mut File) -> std::io::Result<Vec<u8>> {
    let too_big = || std::io::Error::new(std::io::ErrorKind::InvalidData, format!("journal is larger than {MAX_BYTES} bytes"));
    let len = file.metadata()?.len();
    if len > MAX_BYTES {
        return Err(too_big());
    }
    let mut bytes = Vec::with_capacity(len as usize);
    (&mut *file).take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(too_big());
    }
    Ok(bytes)
}

/// The length of the newline-terminated part of `bytes`.
fn whole_len(bytes: &[u8]) -> usize {
    bytes.iter().rposition(|&b| b == b'\n').map_or(0, |n| n + 1)
}

/// The lines in `bytes`, and whether the part after the last newline was left out because it
/// does not read, as when a crash cuts a line short, inside a character or not.
fn parse(bytes: &[u8]) -> std::io::Result<(Vec<Line>, bool)> {
    let invalid = |e: String| std::io::Error::new(std::io::ErrorKind::InvalidData, e);
    let whole = whole_len(bytes);
    let text = std::str::from_utf8(&bytes[..whole]).map_err(|e| invalid(format!("journal: {e}")))?;
    let mut lines = Vec::new();
    for (n, l) in text.lines().enumerate() {
        if l.trim().is_empty() {
            continue;
        }
        lines.push(serde_json::from_str::<Line>(l).map_err(|e| invalid(format!("journal line {}: {e}", n + 1)))?);
    }
    let tail = &bytes[whole..];
    if tail.iter().all(u8::is_ascii_whitespace) {
        return Ok((lines, false));
    }
    match std::str::from_utf8(tail).ok().and_then(|t| serde_json::from_str::<Line>(t).ok()) {
        Some(line) => {
            lines.push(line);
            Ok((lines, false))
        }
        None => Ok((lines, true)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("studiocraft-journal-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_write_cut_short_by_a_full_disk_leaves_only_whole_lines() {
        let dir = scratch("cut-write");
        let mut j = Journal::create(&dir).unwrap();
        j.write(&Line::Dropout { track: 0, at: 0, frames: 1 }).unwrap();
        // The disk takes 10 bytes of the next line, then is full.
        j.fail_after = Some(10);
        assert!(j.write(&Line::Sync { tracks: vec![(0, 1024)] }).is_err());
        j.write(&Line::Stop { reason: "the disk is full".into() }).unwrap();
        drop(j);
        let lines = read(&dir).unwrap();
        assert_eq!(lines, [Line::Dropout { track: 0, at: 0, frames: 1 }, Line::Stop { reason: "the disk is full".into() }]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn whether_a_journal_ended_is_read_from_its_end_alone() {
        let dir = scratch("ended");
        let mut j = Journal::create(&dir).unwrap();
        // Lines well past the part that is read.
        for n in 0..4_000 {
            j.write(&Line::Sync { tracks: vec![(0, n), (1, n)] }).unwrap();
        }
        assert!(!ended(&dir).unwrap());
        j.write(&Line::End { frames: 4_000 }).unwrap();
        drop(j);
        assert!(std::fs::metadata(dir.join(FILE_NAME)).unwrap().len() > TAIL_BYTES);
        assert!(ended(&dir).unwrap());
        // A journal past the limit for reading it whole still answers.
        let f = OpenOptions::new().write(true).open(dir.join(FILE_NAME)).unwrap();
        f.set_len(MAX_BYTES + 1).unwrap();
        drop(f);
        assert!(!ended(&dir).unwrap(), "a journal that ends in zeros did not end");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_journal_with_another_name_is_not_opened_for_repair() {
        let dir = scratch("hard-link");
        let take = dir.join("take");
        std::fs::create_dir_all(&take).unwrap();
        // A last line cut short, which opening for repair would remove.
        let text = b"{\"stop\":{\"reason\":\"x\"}}\n{\"st";
        std::fs::write(dir.join("elsewhere"), text).unwrap();
        std::fs::hard_link(dir.join("elsewhere"), take.join(FILE_NAME)).unwrap();
        let err = Journal::open_idle(&take).err().unwrap_or_else(|| panic!("a journal with two names was opened"));
        assert!(err.to_string().contains("2 names"), "{err}");
        assert_eq!(std::fs::read(dir.join("elsewhere")).unwrap(), text, "the other name's file is unchanged");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
