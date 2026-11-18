//! The disk side: chunks written to each track's file, silence for the gaps, the header rewritten
//! as the file grows, the journal, files split at the segment size, and the free-space reserve.
//!
//! [`Writer`] runs on the disk thread and is the only code of a take that touches the filesystem.
//! It owns every open file and the journal; chunks go back to the collector's pool once written.

use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
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
    /// The disk thread panics on its first chunk.
    pub panic_on_chunk: bool,
}

/// Where a take's files sit on the timeline of the output it follows, for the journal and each
/// file's `bext` chunk.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Timing {
    /// `recording`, `stream`, or empty for a take that follows no output (journaled as `none`).
    pub anchor: String,
    /// Take frame 0 on the studio's clock, when the take has a start time.
    pub origin_tick: Option<i64>,
    /// Frames from the start of the output to take frame 0.
    pub offset_frames: u64,
    /// The wall-clock time of take frame 0, in milliseconds since 1970.
    pub started_unix_ms: Option<i64>,
    /// The local date and time of take frame 0, `yyyy-mm-dd` and `hh:mm:ss`.
    pub date: String,
    pub time: String,
}

/// Where and how a take writes.
#[derive(Clone)]
pub struct DiskConfig {
    /// The take's `Audio Files` folder, which is created. With [`DiskConfig::folder_stem`], the
    /// folder the take's own folder is made in instead.
    pub dir: PathBuf,
    /// Make the take's folder in `dir` as `<stem> Audio`, or `<stem> (n) Audio` when that name is
    /// taken, with its `Audio Files` folder inside. The name is taken by creating the folder, so
    /// two takes never share one.
    pub folder_stem: Option<String>,
    pub timing: Timing,
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
    /// The take frame of the file's first sample.
    base: u64,
    frames: u64,
    header_frames: u64,
    /// The frames the journal's last sync line recorded.
    synced_frames: u64,
    segment: u32,
    stamp: wav::Stamp,
    file_name: String,
}

/// A part closed at the size limit, with its header written.
struct Closed {
    file_name: String,
    base: u64,
    frames: u64,
    segment: u32,
    stamp: wav::Stamp,
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
    dropq: Arc<ArrayQueue<Dropout>>,
    progress: Progress,
    end: u64,
    /// The take's `Audio Files` folder.
    dir: PathBuf,
    /// Every file made, for a take that is thrown away.
    made: Vec<String>,
    /// Each track's parts closed at the size limit, oldest first, for a cut to an earlier part.
    closed: Vec<Vec<Closed>>,
    /// The take ended and is to be thrown away.
    discard: bool,
}

/// Folder names tried for a take's folder before the take stops.
const FOLDER_TRIES: u32 = 999;

/// Make the take's `Audio Files` folder as `cfg` says. Its path.
fn make_folder(cfg: &DiskConfig) -> std::io::Result<PathBuf> {
    let Some(stem) = &cfg.folder_stem else {
        std::fs::create_dir_all(&cfg.dir)?;
        return Ok(cfg.dir.clone());
    };
    std::fs::create_dir_all(&cfg.dir)?;
    for n in 1..=FOLDER_TRIES {
        let name = if n == 1 { format!("{stem} Audio") } else { format!("{stem} ({n}) Audio") };
        let take = cfg.dir.join(name);
        match std::fs::create_dir(&take) {
            Ok(()) => {
                let files = take.join("Audio Files");
                std::fs::create_dir(&files)?;
                return Ok(files);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, "no free folder name"))
}

impl Writer {
    /// Make the folder and the journal. Dropouts come in through `dropq`.
    pub fn create(cfg: DiskConfig, free: Arc<ArrayQueue<Vec<u8>>>, dropq: Arc<ArrayQueue<Dropout>>) -> Result<Writer, Failure> {
        let fail = |e: std::io::Error| Failure { reason: StopReason::CreateFailed, error: e.to_string() };
        let dir = make_folder(&cfg).map_err(fail)?;
        let mut journal = Journal::create(&dir).map_err(fail)?;
        let t = &cfg.timing;
        let anchor = if t.anchor.is_empty() { "none".to_string() } else { t.anchor.clone() };
        let start = Line::Start {
            version: 1,
            sample_rate: studiocraft_media::audio::SAMPLE_RATE,
            channels: 2,
            anchor,
            origin_tick: t.origin_tick,
            offset_frames: t.offset_frames,
            started_unix_ms: t.started_unix_ms,
        };
        journal.write(&start).map_err(fail)?;
        let head = wav::header_len().map_err(fail)?;
        let mut w = Writer {
            cfg,
            head,
            names: Vec::new(),
            files: Vec::new(),
            used: std::collections::HashSet::new(),
            journal,
            free,
            dropq,
            progress: Progress::default(),
            end: 0,
            dir,
            made: Vec::new(),
            closed: Vec::new(),
            discard: false,
        };
        w.check_space()?;
        Ok(w)
    }

    pub fn progress(&self) -> Progress {
        self.progress
    }

    /// The take's `Audio Files` folder.
    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }

    /// Whether the take ended to be thrown away ([`Writer::discard_files`]).
    pub fn discarded(&self) -> bool {
        self.discard
    }

    /// Remove the take's files, its journal and its folders, after a take thrown away. The folder
    /// a take was made in is left alone, and so is a folder holding anything else.
    pub fn discard_files(self) {
        let Writer { dir, made, journal, files, cfg, .. } = self;
        drop(files);
        drop(journal);
        for name in made.iter().map(String::as_str).chain([crate::journal::FILE_NAME]) {
            let _ = std::fs::remove_file(dir.join(name));
        }
        let _ = std::fs::remove_dir(&dir);
        if cfg.folder_stem.is_some()
            && let Some(take) = dir.parent()
        {
            let _ = std::fs::remove_dir(take);
        }
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
                if self.cfg.faults.panic_on_chunk {
                    #[allow(clippy::panic)]
                    {
                        panic!("disk thread fault");
                    }
                }
                let r = self.write_chunk(&chunk);
                let mut data = chunk.data;
                data.clear();
                let _ = self.free.push(data);
                r.map(|()| false)
            }
            ToDisk::Finish { dropped_frames: _, unlisted, ends, discard } => {
                if discard {
                    self.discard = true;
                    return Ok(true);
                }
                self.finish(unlisted, &ends)?;
                Ok(true)
            }
        }
    }

    /// Write each file's header for the frames it holds now.
    pub fn refresh_headers(&mut self) -> Result<(), Failure> {
        for f in self.files.iter_mut().flatten() {
            if f.frames != f.header_frames {
                wav::rewrite_header(&mut f.file, f.frames, &f.stamp).map_err(Failure::io)?;
                f.header_frames = f.frames;
            }
        }
        Ok(())
    }

    /// Put the files that grew on the disk, note what they hold and the dropouts that ended in
    /// the journal, and check the free space.
    pub fn sync(&mut self) -> Result<(), Failure> {
        self.refresh_headers()?;
        let mut grew = Vec::new();
        for (track, f) in self.files.iter_mut().enumerate() {
            let Some(f) = f else { continue };
            if f.frames != f.synced_frames {
                f.file.sync_data().map_err(Failure::io)?;
                f.synced_frames = f.frames;
                grew.push((track, f.frames));
            }
        }
        self.journal_dropouts()?;
        if !grew.is_empty() {
            self.journal.write(&Line::Sync { tracks: grew }).map_err(Failure::io)?;
        }
        self.journal.sync().map_err(Failure::io)?;
        self.check_space()
    }

    /// Journal the dropouts the collector has sent.
    fn journal_dropouts(&mut self) -> Result<(), Failure> {
        while let Some(d) = self.dropq.pop() {
            self.journal.write(&Line::Dropout { track: d.track, at: d.at, frames: d.frames }).map_err(Failure::io)?;
        }
        Ok(())
    }

    /// Note the reason in the journal and close the files with their headers, after a failure.
    pub fn abandon(&mut self, reason: StopReason) {
        let _ = self.journal_dropouts();
        let _ = self.journal.write(&Line::Stop { reason: reason.text().into() });
        let _ = self.refresh_headers();
        for f in self.files.iter_mut().flatten() {
            let _ = f.file.sync_data();
        }
        let _ = self.journal.sync();
    }

    fn check_space(&mut self) -> Result<(), Failure> {
        let read = if self.cfg.faults.space_unknown { Err(std::io::Error::other("free space fault")) } else { crate::space::available(&self.dir) };
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

    fn finish(&mut self, unlisted: u64, ends: &[u64]) -> Result<(), Failure> {
        // A chunk sent before the take's end time was known can hold audio past it, so each file
        // is cut back to where its track ended. Audio lost at the end of a track has no later
        // chunk to write silence before, so each file is padded to that point too. The cuts are in
        // the journal on the disk before any file changes.
        let cuts: Vec<(usize, u64)> = ends
            .iter()
            .enumerate()
            .filter(|&(track, &end)| self.files.get(track).and_then(Option::as_ref).is_some_and(|f| f.base + f.frames > end))
            .map(|(track, &end)| (track, end))
            .collect();
        if !cuts.is_empty() {
            for &(track, frames) in &cuts {
                self.journal.write(&Line::Cut { track, frames }).map_err(Failure::io)?;
            }
            self.journal.sync().map_err(Failure::io)?;
        }
        for (track, &end) in ends.iter().enumerate() {
            self.cut_to(track, end)?;
            self.write_chunk(&Chunk { track, first_frame: end, data: Vec::new() })?;
        }
        self.end = self.files.iter().flatten().map(|f| f.base + f.frames).max().unwrap_or(0);
        // A track that never got audio, as in a take that ended before its first block, still
        // gets its file, empty.
        for track in 0..self.names.len() {
            let named = self.names.get(track).is_some_and(Option::is_some);
            if named && self.files.get(track).is_none_or(Option::is_none) {
                self.room(track, 0)?;
            }
        }
        self.journal_dropouts()?;
        if unlisted > 0 {
            self.journal.write(&Line::Unlisted { frames: unlisted }).map_err(Failure::io)?;
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
                    wav::rewrite_header(&mut old.file, old.frames, &old.stamp).map_err(Failure::io)?;
                    old.file.sync_all().map_err(Failure::io)?;
                    if self.closed.len() <= track {
                        self.closed.resize_with(track + 1, Vec::new);
                    }
                    if let Some(parts) = self.closed.get_mut(track) {
                        let TrackFile { file_name, base, frames, segment, stamp, .. } = old;
                        parts.push(Closed { file_name, base, frames, segment, stamp });
                    }
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
        let path = self.dir.join(&file_name);
        let fail = |e: std::io::Error| Failure { reason: StopReason::CreateFailed, error: e.to_string() };
        let mut file = OpenOptions::new().create_new(true).write(true).read(true).open(&path).map_err(fail)?;
        self.made.push(file_name.clone());
        let t = &self.cfg.timing;
        let stamp = wav::Stamp { time_reference: t.offset_frames.saturating_add(base), date: t.date.clone(), time: t.time.clone() };
        file.write_all(&wav::header(0, &stamp).map_err(fail)?).map_err(Failure::io)?;
        self.journal.write(&Line::File { track, name, file: file_name.clone(), segment, first_frame: base }).map_err(Failure::io)?;
        if self.files.len() <= track {
            self.files.resize_with(track + 1, || None);
        }
        if let Some(slot) = self.files.get_mut(track) {
            *slot = Some(TrackFile { file, base, frames: 0, header_frames: 0, synced_frames: 0, segment, stamp, file_name });
        }
        Ok(())
    }

    /// Cut the files of `track` back to take frame `end` when they run past it. A part that starts
    /// at or after `end` is removed, and the part before it becomes the track's file again.
    fn cut_to(&mut self, track: usize, end: u64) -> Result<(), Failure> {
        while self.files.get(track).and_then(Option::as_ref).is_some_and(|f| f.base >= end)
            && let Some(prev) = self.closed.get_mut(track).and_then(Vec::pop)
        {
            let Some(Some(cur)) = self.files.get_mut(track).map(Option::take) else { break };
            self.progress.bytes = self.progress.bytes.saturating_sub(cur.frames * FRAME_BYTES);
            drop(cur.file);
            std::fs::remove_file(self.dir.join(&cur.file_name)).map_err(Failure::io)?;
            self.made.retain(|n| *n != cur.file_name);
            let mut file = OpenOptions::new().read(true).write(true).open(self.dir.join(&prev.file_name)).map_err(Failure::io)?;
            file.seek(SeekFrom::End(0)).map_err(Failure::io)?;
            let Closed { file_name, base, frames, segment, stamp } = prev;
            if let Some(slot) = self.files.get_mut(track) {
                *slot = Some(TrackFile { file, base, frames, header_frames: frames, synced_frames: frames, segment, stamp, file_name });
            }
        }
        let head = self.head;
        let Some(Some(f)) = self.files.get_mut(track) else { return Ok(()) };
        if f.base + f.frames <= end {
            return Ok(());
        }
        let keep = end.saturating_sub(f.base);
        f.file.set_len(head + keep * FRAME_BYTES).map_err(Failure::io)?;
        f.file.seek(SeekFrom::End(0)).map_err(Failure::io)?;
        self.progress.bytes = self.progress.bytes.saturating_sub((f.frames - keep) * FRAME_BYTES);
        f.frames = keep;
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
