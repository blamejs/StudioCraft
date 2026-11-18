//! The disk side: chunks written to each track's file, silence for the gaps, the header rewritten
//! as the file grows, the journal, files split at the segment size, and the free-space reserve.
//!
//! [`Writer`] runs on the disk thread and is the only code of a take that touches the filesystem.
//! It owns every open file and the journal; chunks go back to the collector's pool once written.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};

use crossbeam_queue::ArrayQueue;

use crate::collect::{Chunk, Dropout, ToDisk};
use crate::journal::{Journal, Line};
use crate::wav::{self, FRAME_BYTES};

/// Silence written for a gap, a piece at a time.
static ZEROS: [u8; 64 * 1024] = [0; 64 * 1024];

/// Why a take stopped before it was asked to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum StopReason {
    /// The disk had no room for a write.
    DiskFull = 1,
    /// Free space fell to the reserve.
    LowSpace = 2,
    /// A write failed for another reason.
    WriteFailed = 3,
    /// The take's folder or a file could not be made.
    CreateFailed = 4,
    /// The free space on the disk could not be read, so the reserve could not be kept.
    SpaceUnknown = 5,
    /// The memory for the write buffer could not be reserved.
    NoMemory = 6,
}

impl StopReason {
    pub fn from_u8(v: u8) -> Option<StopReason> {
        match v {
            1 => Some(StopReason::DiskFull),
            2 => Some(StopReason::LowSpace),
            3 => Some(StopReason::WriteFailed),
            4 => Some(StopReason::CreateFailed),
            5 => Some(StopReason::SpaceUnknown),
            6 => Some(StopReason::NoMemory),
            _ => None,
        }
    }

    /// The English text the status shows; the UI translates it.
    pub fn text(self) -> &'static str {
        match self {
            StopReason::DiskFull => "the disk is full",
            StopReason::LowSpace => "the disk is almost full",
            StopReason::WriteFailed => "a file could not be written",
            StopReason::CreateFailed => "the files could not be created",
            StopReason::SpaceUnknown => "the free space on the disk could not be read",
            StopReason::NoMemory => "there is not enough memory for the write buffer",
        }
    }

    fn of(e: &std::io::Error) -> StopReason {
        if e.kind() == std::io::ErrorKind::StorageFull { StopReason::DiskFull } else { StopReason::WriteFailed }
    }
}

/// A failure the disk thread stops on.
#[derive(Debug)]
pub struct Failure {
    pub reason: StopReason,
    pub error: String,
}

impl Failure {
    fn io(e: std::io::Error) -> Failure {
        Failure { reason: StopReason::of(&e), error: e.to_string() }
    }
}

/// A count of permits a test or the throttled-disk benchmark hands the disk thread, one per chunk.
#[doc(hidden)]
#[derive(Default)]
pub struct Gate {
    permits: Mutex<Option<u64>>,
    changed: Condvar,
}

#[doc(hidden)]
impl Gate {
    /// A gate that lets nothing through until [`Gate::allow`] or [`Gate::open`].
    pub fn closed() -> Arc<Gate> {
        Arc::new(Gate { permits: Mutex::new(Some(0)), changed: Condvar::new() })
    }

    pub fn allow(&self, n: u64) {
        if let Ok(mut p) = self.permits.lock() {
            if let Some(p) = p.as_mut() {
                *p += n;
            }
            self.changed.notify_all();
        }
    }

    /// Let nothing through until [`Gate::allow`] or [`Gate::open`].
    pub fn close(&self) {
        if let Ok(mut p) = self.permits.lock() {
            *p = Some(0);
        }
    }

    /// Let everything through from now on.
    pub fn open(&self) {
        if let Ok(mut p) = self.permits.lock() {
            *p = None;
            self.changed.notify_all();
        }
    }

    fn wait(&self) {
        let Ok(mut p) = self.permits.lock() else { return };
        loop {
            match p.as_mut() {
                None => return,
                Some(n) if *n > 0 => {
                    *n -= 1;
                    return;
                }
                Some(_) => match self.changed.wait(p) {
                    Ok(g) => p = g,
                    Err(_) => return,
                },
            }
        }
    }
}

/// Faults the tests and the benchmark put into the disk side.
#[doc(hidden)]
#[derive(Clone, Default)]
pub struct Faults {
    /// Writes fail with "storage full" once this many bytes of audio are written.
    pub full_after_bytes: Option<u64>,
    /// Free space reported instead of the disk's.
    pub free_space: Option<u64>,
    /// Reading the free space fails.
    pub space_unknown: bool,
    /// The memory for the write buffer cannot be reserved.
    pub no_memory: bool,
    /// A permit per chunk.
    pub gate: Option<Arc<Gate>>,
}

/// Where and how a take writes.
#[derive(Clone)]
pub struct DiskConfig {
    /// The take's `Audio Files` folder; it is created.
    pub dir: PathBuf,
    /// Bytes at which a file is closed and the track's next part begins.
    pub segment_bytes: u64,
    /// Free space the tracks stop at.
    pub reserve_bytes: u64,
    /// Free space below which the status warns.
    pub warn_bytes: u64,
    #[doc(hidden)]
    pub faults: Faults,
}

struct TrackFile {
    file: File,
    file_name: String,
    /// The take frame of the file's first sample.
    base: u64,
    frames: u64,
    header_frames: u64,
    segment: u32,
}

/// What the disk thread reports after each step.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    pub bytes: u64,
    pub free_bytes: u64,
    pub low_space: bool,
}

/// Writes one take.
pub struct Writer {
    cfg: DiskConfig,
    head: u64,
    names: Vec<Option<String>>,
    files: Vec<Option<TrackFile>>,
    /// File names already used in the folder, without letter case.
    used: std::collections::HashSet<String>,
    journal: Journal,
    free: Arc<ArrayQueue<Vec<u8>>>,
    progress: Progress,
    end: u64,
}

impl Writer {
    /// Make the folder and the journal.
    pub fn create(cfg: DiskConfig, free: Arc<ArrayQueue<Vec<u8>>>) -> Result<Writer, Failure> {
        let fail = |e: std::io::Error| Failure { reason: StopReason::CreateFailed, error: e.to_string() };
        std::fs::create_dir_all(&cfg.dir).map_err(fail)?;
        let mut journal = Journal::create(&cfg.dir).map_err(fail)?;
        journal.write(&Line::Start { version: 1, sample_rate: studiocraft_media::audio::SAMPLE_RATE, channels: 2 }).map_err(fail)?;
        let head = wav::header_len().map_err(fail)?;
        let mut w = Writer {
            cfg,
            head,
            names: Vec::new(),
            files: Vec::new(),
            used: std::collections::HashSet::new(),
            journal,
            free,
            progress: Progress::default(),
            end: 0,
        };
        w.check_space()?;
        Ok(w)
    }

    pub fn progress(&self) -> Progress {
        self.progress
    }

    /// Handle one message from the collector. True at the end of the take.
    pub fn handle(&mut self, msg: ToDisk) -> Result<bool, Failure> {
        match msg {
            ToDisk::Track { track, name } => {
                if self.names.len() <= track {
                    self.names.resize(track + 1, None);
                    self.files.resize_with(track + 1, || None);
                }
                if let Some(n) = self.names.get_mut(track) {
                    *n = Some(name);
                }
                Ok(false)
            }
            ToDisk::Chunk(chunk) => {
                if let Some(g) = &self.cfg.faults.gate {
                    g.wait();
                }
                let r = self.write_chunk(&chunk);
                let mut data = chunk.data;
                data.clear();
                let _ = self.free.push(data);
                r.map(|()| false)
            }
            ToDisk::Finish { dropouts, dropped_frames: _, ends } => {
                self.finish(&dropouts, &ends)?;
                Ok(true)
            }
        }
    }

    /// Write each file's header for the frames it holds now.
    pub fn refresh_headers(&mut self) -> Result<(), Failure> {
        for f in self.files.iter_mut().flatten() {
            if f.frames != f.header_frames {
                wav::rewrite_header(&mut f.file, f.frames).map_err(Failure::io)?;
                f.header_frames = f.frames;
            }
        }
        Ok(())
    }

    /// Put every file on the disk, note what they hold in the journal, and check the free space.
    pub fn sync(&mut self) -> Result<(), Failure> {
        self.refresh_headers()?;
        let mut held = Vec::new();
        for f in self.files.iter_mut().flatten() {
            f.file.sync_data().map_err(Failure::io)?;
            held.push((f.file_name.clone(), f.frames));
        }
        self.journal.write(&Line::Sync { files: held }).map_err(Failure::io)?;
        self.journal.sync().map_err(Failure::io)?;
        self.check_space()
    }

    /// Note the reason in the journal and close the files with their headers, after a failure.
    pub fn abandon(&mut self, reason: StopReason) {
        let _ = self.journal.write(&Line::Stop { reason: reason.text().into() });
        let _ = self.refresh_headers();
        for f in self.files.iter_mut().flatten() {
            let _ = f.file.sync_data();
        }
        let _ = self.journal.sync();
    }

    fn check_space(&mut self) -> Result<(), Failure> {
        let read =
            if self.cfg.faults.space_unknown { Err(std::io::Error::other("free space fault")) } else { crate::space::available(&self.cfg.dir) };
        let free = match (self.cfg.faults.free_space, read) {
            (Some(f), _) => f,
            (None, Ok(f)) => f,
            // Without the free space the reserve cannot be kept, so the take stops.
            (None, Err(e)) => return Err(Failure { reason: StopReason::SpaceUnknown, error: format!("free space: {e}") }),
        };
        self.progress.free_bytes = free;
        self.progress.low_space = free < self.cfg.warn_bytes;
        if free < self.cfg.reserve_bytes {
            return Err(Failure { reason: StopReason::LowSpace, error: format!("{free} bytes free") });
        }
        Ok(())
    }

    fn finish(&mut self, dropouts: &[Dropout], ends: &[u64]) -> Result<(), Failure> {
        // Audio lost at the end of a track has no later chunk to write silence before, so each
        // file is padded to where its track ended.
        for (track, &end) in ends.iter().enumerate() {
            self.write_chunk(&Chunk { track, first_frame: end, data: Vec::new() })?;
        }
        for d in dropouts {
            self.journal.write(&Line::Dropout { track: d.track, at: d.at, frames: d.frames }).map_err(Failure::io)?;
        }
        self.refresh_headers()?;
        for f in self.files.iter_mut().flatten() {
            f.file.sync_all().map_err(Failure::io)?;
        }
        self.journal.write(&Line::End { frames: self.end }).map_err(Failure::io)?;
        self.journal.sync().map_err(Failure::io)
    }

    fn write_chunk(&mut self, chunk: &Chunk) -> Result<(), Failure> {
        let mut at = chunk.first_frame;
        let mut data: &[u8] = &chunk.data;
        let end = |f: &Option<TrackFile>| f.as_ref().map(|f| f.base + f.frames);
        let written = end(self.files.get(chunk.track).unwrap_or(&None)).unwrap_or(0);
        if at < written {
            // Already on disk; only the part after it is new.
            let skip = ((written - at) * FRAME_BYTES).min(data.len() as u64) as usize;
            data = data.get(skip..).unwrap_or_default();
            at = written;
        }
        let mut pos = written;
        while pos < at {
            let n = self.room(chunk.track, pos)?.min(at - pos).min(ZEROS.len() as u64 / FRAME_BYTES);
            self.append(chunk.track, &ZEROS[..(n * FRAME_BYTES) as usize])?;
            pos += n;
        }
        while !data.is_empty() {
            let n = self.room(chunk.track, pos)?.min(data.len() as u64 / FRAME_BYTES);
            let (now, rest) = data.split_at((n * FRAME_BYTES) as usize);
            self.append(chunk.track, now)?;
            data = rest;
            pos += n;
            if n == 0 {
                break;
            }
        }
        self.end = self.end.max(pos);
        Ok(())
    }

    /// Frames the track's current file still takes, opening the file or its next part first.
    fn room(&mut self, track: usize, pos: u64) -> Result<u64, Failure> {
        let cap = (self.cfg.segment_bytes.saturating_sub(self.head) / FRAME_BYTES).max(1);
        let full = self.files.get(track).and_then(|f| f.as_ref()).map(|f| f.frames >= cap);
        match full {
            Some(false) => {}
            Some(true) => {
                let next = self.files.get(track).and_then(|f| f.as_ref()).map(|f| f.segment + 1).unwrap_or(1);
                if let Some(Some(mut old)) = self.files.get_mut(track).map(Option::take) {
                    wav::rewrite_header(&mut old.file, old.frames).map_err(Failure::io)?;
                    old.file.sync_all().map_err(Failure::io)?;
                }
                self.open(track, next, pos)?;
            }
            None => self.open(track, 1, 0)?,
        }
        Ok(self.files.get(track).and_then(|f| f.as_ref()).map(|f| cap - f.frames).unwrap_or(0))
    }

    fn open(&mut self, track: usize, segment: u32, base: u64) -> Result<(), Failure> {
        let name = self.names.get(track).cloned().flatten().unwrap_or_else(|| format!("Track {}", track + 1));
        let stem = if segment == 1 { name.clone() } else { format!("{name} part {segment}") };
        let file_name = crate::names::unique(&stem, &mut self.used) + ".wav";
        let path = self.cfg.dir.join(&file_name);
        let fail = |e: std::io::Error| Failure { reason: StopReason::CreateFailed, error: e.to_string() };
        let mut file = OpenOptions::new().create_new(true).write(true).read(true).open(&path).map_err(fail)?;
        file.write_all(&wav::header(0).map_err(fail)?).map_err(Failure::io)?;
        self.journal.write(&Line::File { track, name, file: file_name.clone(), segment, first_frame: base }).map_err(Failure::io)?;
        if self.files.len() <= track {
            self.files.resize_with(track + 1, || None);
        }
        if let Some(slot) = self.files.get_mut(track) {
            *slot = Some(TrackFile { file, file_name, base, frames: 0, header_frames: 0, segment });
        }
        Ok(())
    }

    fn append(&mut self, track: usize, bytes: &[u8]) -> Result<(), Failure> {
        // A disk that fills takes what fits, in whole frames, and fails the rest.
        let (fits, full) = match self.cfg.faults.full_after_bytes {
            Some(limit) if self.progress.bytes + bytes.len() as u64 > limit => {
                let room = limit.saturating_sub(self.progress.bytes) / FRAME_BYTES * FRAME_BYTES;
                (bytes.get(..room as usize).unwrap_or_default(), true)
            }
            _ => (bytes, false),
        };
        let Some(Some(f)) = self.files.get_mut(track) else { return Ok(()) };
        f.file.write_all(fits).map_err(Failure::io)?;
        f.frames += fits.len() as u64 / FRAME_BYTES;
        self.progress.bytes += fits.len() as u64;
        if full {
            return Err(Failure::io(std::io::Error::new(std::io::ErrorKind::StorageFull, "no space left on the disk")));
        }
        Ok(())
    }
}
