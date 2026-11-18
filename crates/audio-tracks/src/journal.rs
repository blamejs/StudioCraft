//! `take.journal`: one JSON object per line, appended by the disk thread, read back by repair.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

/// The journal's file name in a take's folder.
pub const FILE_NAME: &str = "take.journal";

/// The largest journal that is read back. A day-long take of [`crate::MAX_TAKE_TRACKS`] files
/// writes about 40 MiB of sync lines, so a larger file did not come from the disk thread.
pub const MAX_BYTES: u64 = 256 << 20;

/// One line of the journal.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Line {
    /// The take began; the files are 32-bit float WAV at this rate and channel count.
    Start { version: u32, sample_rate: u32, channels: u16 },
    /// A track's file was opened. `first_frame` is the take frame its first sample is at.
    File { track: usize, name: String, file: String, segment: u32, first_frame: u64 },
    /// After a sync: frames each open file held on disk, by file name.
    Sync { files: Vec<(String, u64)> },
    /// Audio lost before the disk; the track's file holds silence there.
    Dropout { track: usize, at: u64, frames: u64 },
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
}

impl Journal {
    /// Create the journal in `dir`, locked for as long as the journal is open. [`Journal::open_idle`]
    /// skips a locked journal, so repair never touches a take that is still being written.
    pub fn create(dir: &Path) -> std::io::Result<Journal> {
        let file = OpenOptions::new().create_new(true).read(true).append(true).open(dir.join(FILE_NAME))?;
        file.lock()?;
        Ok(Journal { file })
    }

    /// Open the journal in `dir` to append to it, with its lines, locked while it is open. `None`
    /// when another journal handle holds the lock: the take is still being written or repaired.
    /// A last line cut short by a crash is removed, so the lines appended next read back.
    pub fn open_idle(dir: &Path) -> std::io::Result<Option<(Journal, Vec<Line>)>> {
        // Write access, not append: setting the length needs it on Windows.
        let mut file = OpenOptions::new().read(true).write(true).open(dir.join(FILE_NAME))?;
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
        file.seek(SeekFrom::End(0))?;
        if !cut && whole < bytes.len() {
            // The last line read but has no newline: end it so the next line starts on its own.
            file.write_all(b"\n")?;
        }
        Ok(Some((Journal { file }, lines)))
    }

    /// Append `line`. It reaches the disk with the next [`Journal::sync`].
    pub fn write(&mut self, line: &Line) -> std::io::Result<()> {
        let mut text = serde_json::to_string(line).map_err(std::io::Error::other)?;
        text.push('\n');
        self.file.write_all(text.as_bytes())
    }

    pub fn sync(&mut self) -> std::io::Result<()> {
        self.file.sync_data()
    }
}

/// The lines of the journal in `dir`. A last line cut short by a crash is left out; other lines
/// that do not read are an error.
pub fn read(dir: &Path) -> std::io::Result<Vec<Line>> {
    parse(&read_bounded(&mut File::open(dir.join(FILE_NAME))?)?).map(|(lines, _)| lines)
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
