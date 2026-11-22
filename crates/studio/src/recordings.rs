//! The recordings in the recording folder, for `recordings.list`: each recording file with its
//! marker timeline and its take of audio tracks, each saved replay, and each take of audio tracks
//! that has no recording file (a stream's take), newest first.
//!
//! A scan reads at most [`MAX_ENTRIES`] entries of the folder for its files and lists the takes
//! with [`takes::list`], which reads at most as many. It looks at no folder below the recording
//! folder other than the takes' own, and follows no link: a file that is a link is not listed. A
//! scan runs on its own thread and stops looking at files once it has run for [`SCAN_LIMIT`];
//! `recordings.list` never waits for it, and returns the last finished scan while the next one
//! runs. One scan runs at a time.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, UNIX_EPOCH};

use chrono::TimeZone;
use serde_json::{Value, json};
use studiocraft_audio_tracks::takes::{self, TakeInfo, TakeState};
use studiocraft_model::RecordingFormat;

use crate::{Studio, StudioError, recording_folder};

/// Entries of the recording folder a scan reads at most for its files, as for its takes.
pub const MAX_ENTRIES: usize = takes::MAX_ENTRIES;

/// How long a scan looks at files before it stops and reports `timed_out`; StudioCraft's own
/// value. Listing the takes is not cut short: it reads at most [`takes::MAX_ENTRIES`] journals.
pub const SCAN_LIMIT: Duration = Duration::from_secs(5);

/// The start of a saved replay's name ([`crate::replay_path`]).
const REPLAY_PREFIX: &str = "Replay ";

/// What one scan of the recording folder found.
#[derive(Debug)]
pub(crate) struct Scan {
    folder: PathBuf,
    entries: Vec<Entry>,
    unreadable: Vec<(PathBuf, String)>,
    /// Whether the folder holds more entries than a scan reads.
    more: bool,
    /// Whether the scan stopped at [`SCAN_LIMIT`] before it looked at every entry.
    timed_out: bool,
    finished_unix_ms: Option<i64>,
}

/// A recording file or saved replay, or a take of audio tracks without one.
#[derive(Debug)]
struct Entry {
    /// The file's name without its extension, or the take's name.
    name: String,
    file: Option<VideoFile>,
    /// The recording's marker timeline (`<name>.otio`).
    timeline: Option<PathBuf>,
    take: Option<TakeInfo>,
}

#[derive(Debug)]
struct VideoFile {
    path: PathBuf,
    extension: String,
    bytes: u64,
    modified_unix_ms: Option<i64>,
    replay: bool,
}

impl Entry {
    /// When the recording or take started: the date and time in the file's name, read as local
    /// time, else the take's start. A take being written, or one whose journal gives no start,
    /// has its journal's last change as its start.
    fn started_unix_ms(&self) -> Option<i64> {
        self.file.as_ref().and_then(|_| stamp_unix_ms(&self.name)).or_else(|| self.take.as_ref().and_then(|t| t.started_unix_ms))
    }

    /// The time the list is sorted by: the start, else the file's last change.
    fn sort_ms(&self) -> Option<i64> {
        self.started_unix_ms().or_else(|| self.file.as_ref().and_then(|f| f.modified_unix_ms))
    }

    fn json(&self) -> Value {
        let kind = match &self.file {
            Some(f) if f.replay => "replay",
            Some(_) => "recording",
            None => "audio_tracks",
        };
        json!({
            "name": self.name,
            "kind": kind,
            "started_unix_ms": self.started_unix_ms(),
            "file": self.file.as_ref().map(|f| f.path.display().to_string()),
            "format": self.file.as_ref().map(|f| f.extension.clone()),
            "bytes": self.file.as_ref().map(|f| f.bytes),
            "modified_unix_ms": self.file.as_ref().and_then(|f| f.modified_unix_ms),
            "timeline": self.timeline.as_ref().map(|p| p.display().to_string()),
            "audio_tracks": self.take.as_ref().map(take_json),
        })
    }
}

fn take_json(t: &TakeInfo) -> Value {
    let state = match t.state {
        TakeState::Finished => "finished",
        TakeState::NeedsRepair => "needs_repair",
        TakeState::Recording => "recording",
    };
    json!({
        "folder": t.dir.display().to_string(),
        "session": t.session.as_ref().map(|p| p.display().to_string()),
        "state": state,
        "started_unix_ms": t.started_unix_ms,
        "seconds": t.seconds(),
        "tracks": t.tracks,
        "bytes": t.bytes,
        "missing_files": t.missing_files,
        "dropouts": t.dropouts,
        "lost_seconds": t.lost_frames as f64 / f64::from(t.sample_rate.max(1)),
        "markers": t.markers,
        "repaired": t.repaired,
        "stopped": t.stopped,
    })
}

impl Scan {
    fn json(&self, scanning: bool) -> Value {
        json!({
            "folder": self.folder.display().to_string(),
            "scanning": scanning,
            "scanned_unix_ms": self.finished_unix_ms,
            "recordings": self.entries.iter().map(Entry::json).collect::<Vec<_>>(),
            "unreadable": self.unreadable.iter().map(|(p, e)| json!({"path": p.display().to_string(), "error": e})).collect::<Vec<_>>(),
            "more": self.more,
            "timed_out": self.timed_out,
        })
    }
}

/// The milliseconds since 1970 of the local date and time a recording's or replay's name starts
/// with (`2026-11-21 10-00-00`, after `Replay `), when it starts with one.
fn stamp_unix_ms(name: &str) -> Option<i64> {
    let stamp = name.strip_prefix(REPLAY_PREFIX).unwrap_or(name).get(..19)?;
    let naive = chrono::NaiveDateTime::parse_from_str(stamp, "%Y-%m-%d %H-%M-%S").ok()?;
    chrono::Local.from_local_datetime(&naive).earliest().map(|t| t.timestamp_millis())
}

/// The name `<base> (N)` with N a number has, as a name given when `<base>` was taken.
fn numbered_base(name: &str) -> Option<&str> {
    let (base, n) = name.strip_suffix(')')?.rsplit_once(" (")?;
    (!base.is_empty() && !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())).then_some(base)
}

/// How far apart two times may be and still be read as the same moment when a timeline or take
/// is matched to its recording; StudioCraft's own value.
const MATCH_SLACK_MS: i64 = 10_000;

/// A recording a timeline or take can belong to.
struct Owner<'a> {
    stem: &'a str,
    /// When the recording started, from its name.
    start: Option<i64>,
    /// The time the right timeline or take is nearest to.
    target: Option<i64>,
    /// The file's last change: of recordings with one name (`<name>.mkv` and `<name>.mp4`), the
    /// one changed first is served first.
    modified: Option<i64>,
}

/// The recording each candidate `(name, time)` belongs to, as an index into `recs`. A candidate
/// named after recordings can belong only to one of them, and one named `<stem> (N)` (the name a
/// timeline or take folder gets when its first name is taken) only to a recording named `<stem>`,
/// when no recording has its own name. A candidate made more than [`MATCH_SLACK_MS`] before the
/// recording started is not its. Each recording takes at most one candidate: first the pairs
/// whose candidate is nearest the recording's `target` within [`MATCH_SLACK_MS`], then, for the
/// recordings left, the candidate of their own name, then the first numbered one. The candidates
/// left over map to `None`.
fn owners(cands: &[(&str, Option<i64>)], recs: &[Owner<'_>]) -> Vec<Option<usize>> {
    let mut index: HashMap<&str, Vec<usize>> = HashMap::new();
    for (ri, r) in recs.iter().enumerate() {
        index.entry(r.stem).or_default().push(ri);
    }
    // The recordings of one name, the one changed first first.
    for list in index.values_mut() {
        list.sort_by_key(|&ri| (recs[ri].modified.is_none(), recs[ri].modified, ri));
    }
    let mut groups: HashMap<&str, Vec<usize>> = HashMap::new();
    for (ci, (name, _)) in cands.iter().enumerate() {
        let stem = match index.get_key_value(name) {
            Some((stem, _)) => Some(*stem),
            None => numbered_base(name).and_then(|base| index.get_key_value(base)).map(|(stem, _)| *stem),
        };
        if let Some(stem) = stem {
            groups.entry(stem).or_default().push(ci);
        }
    }
    let mut out = vec![None; cands.len()];
    for (stem, list) in groups {
        let rs = index.get(stem).map(Vec::as_slice).unwrap_or_default();
        let fits = |ri: usize, ci: usize| match (recs[ri].start, cands[ci].1) {
            (Some(start), Some(t)) => t >= start.saturating_sub(MATCH_SLACK_MS),
            _ => true,
        };
        let exact = |ci: usize| cands[ci].0 == stem;
        let mut served = vec![false; rs.len()];
        let mut pairs: Vec<(u64, bool, usize, usize)> = Vec::new();
        for (k, &ri) in rs.iter().enumerate() {
            let Some(target) = recs[ri].target else { continue };
            for &ci in &list {
                if let Some(t) = cands[ci].1
                    && fits(ri, ci)
                    && t.abs_diff(target) <= MATCH_SLACK_MS.unsigned_abs()
                {
                    pairs.push((t.abs_diff(target), !exact(ci), k, ci));
                }
            }
        }
        pairs.sort_unstable();
        for (_, _, k, ci) in pairs {
            if !served[k] && out[ci].is_none() {
                served[k] = true;
                out[ci] = Some(rs[k]);
            }
        }
        for (k, &ri) in rs.iter().enumerate() {
            if served[k] {
                continue;
            }
            let free = |ci: &&usize| out[**ci].is_none() && fits(ri, **ci);
            let pick = list.iter().filter(free).find(|ci| exact(**ci)).or_else(|| list.iter().find(free)).copied();
            if let Some(ci) = pick {
                out[ci] = Some(ri);
            }
        }
    }
    out
}

fn modified_ms(m: &std::fs::Metadata) -> Option<i64> {
    m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).and_then(|d| i64::try_from(d.as_millis()).ok())
}

/// Scan `folder` (see the module documentation), stopping the look at files once `now()` reaches
/// `deadline`.
pub(crate) fn scan(folder: &Path, deadline: Instant, now: &dyn Fn() -> Instant) -> Scan {
    let mut out =
        Scan { folder: folder.to_path_buf(), entries: Vec::new(), unreadable: Vec::new(), more: false, timed_out: false, finished_unix_ms: None };
    let mut videos: Vec<(String, VideoFile)> = Vec::new();
    let mut timelines: Vec<(String, PathBuf, Option<i64>)> = Vec::new();
    match std::fs::read_dir(folder) {
        Err(e) => out.unreadable.push((folder.to_path_buf(), e.to_string())),
        Ok(entries) => {
            for (seen, entry) in entries.enumerate() {
                if seen == MAX_ENTRIES {
                    out.more = true;
                    break;
                }
                if now() >= deadline {
                    out.timed_out = true;
                    break;
                }
                let entry = match entry {
                    Ok(e) => e,
                    Err(e) => {
                        out.unreadable.push((folder.to_path_buf(), e.to_string()));
                        continue;
                    }
                };
                // The entry's own type: a link is not a file here.
                match entry.file_type() {
                    Ok(t) if t.is_file() => {}
                    Ok(_) => continue,
                    Err(e) => {
                        out.unreadable.push((entry.path(), e.to_string()));
                        continue;
                    }
                }
                let path = entry.path();
                let (Some(stem), Some(ext)) = (path.file_stem().and_then(|s| s.to_str()), path.extension().and_then(|s| s.to_str())) else {
                    continue;
                };
                let ext = ext.to_ascii_lowercase();
                let timeline = ext == "otio";
                if !timeline && !RecordingFormat::ALL.iter().any(|f| f.extension() == ext) {
                    continue;
                }
                let meta = match entry.metadata() {
                    Ok(m) => m,
                    Err(e) => {
                        out.unreadable.push((path, e.to_string()));
                        continue;
                    }
                };
                if timeline {
                    timelines.push((stem.to_string(), path.clone(), modified_ms(&meta)));
                    continue;
                }
                let replay = ext == "mkv" && stem.starts_with(REPLAY_PREFIX);
                let file = VideoFile { path: path.clone(), extension: ext, bytes: meta.len(), modified_unix_ms: modified_ms(&meta), replay };
                videos.push((stem.to_string(), file));
            }
        }
    }
    let listing = if out.timed_out || now() >= deadline {
        out.timed_out = true;
        takes::Listing::default()
    } else {
        takes::list(folder)
    };
    out.more |= listing.more;
    // A folder that cannot be read is reported once.
    let folder_reported = out.unreadable.iter().any(|(p, _)| p == folder);
    out.unreadable.extend(listing.unreadable.into_iter().filter(|(p, _)| p != folder || !folder_reported));
    // Replays have no timeline or take. A timeline is written when its recording's file is
    // finished, so it is nearest the file's last change; a take starts with its recording.
    let recs: Vec<(usize, &str)> = videos.iter().enumerate().filter(|(_, (_, f))| !f.replay).map(|(i, (s, _))| (i, s.as_str())).collect();
    let starts: Vec<Option<i64>> = recs.iter().map(|(_, s)| stamp_unix_ms(s)).collect();
    let owners_for = |target: &dyn Fn(usize) -> Option<i64>| -> Vec<Owner<'_>> {
        recs.iter()
            .zip(&starts)
            .map(|(&(vi, stem), &start)| Owner { stem, start, target: target(vi).or(start), modified: videos[vi].1.modified_unix_ms })
            .collect()
    };
    let by_end = owners_for(&|vi| videos[vi].1.modified_unix_ms);
    let by_start = owners_for(&|_| None);
    let cands: Vec<(&str, Option<i64>)> = timelines.iter().map(|(s, _, t)| (s.as_str(), *t)).collect();
    let mut found: Vec<(Option<PathBuf>, Option<TakeInfo>)> = recs.iter().map(|_| (None, None)).collect();
    for ((_, path, _), owner) in timelines.iter().zip(owners(&cands, &by_end)) {
        if let Some(ri) = owner {
            found[ri].0 = Some(path.clone());
        }
    }
    let cands: Vec<(&str, Option<i64>)> = listing.takes.iter().map(|t| (t.name.as_str(), t.started_unix_ms)).collect();
    let take_owners = owners(&cands, &by_start);
    let mut lone = Vec::new();
    for (take, owner) in listing.takes.into_iter().zip(take_owners) {
        match owner {
            Some(ri) => found[ri].1 = Some(take),
            None => lone.push(take),
        }
    }
    let mut found_for: HashMap<usize, (Option<PathBuf>, Option<TakeInfo>)> = recs.iter().map(|(vi, _)| *vi).zip(found).collect();
    for (vi, (name, file)) in videos.into_iter().enumerate() {
        let (timeline, take) = found_for.remove(&vi).unwrap_or_default();
        out.entries.push(Entry { name, file: Some(file), timeline, take });
    }
    out.entries.extend(lone.into_iter().map(|t| Entry { name: t.name.clone(), file: None, timeline: None, take: Some(t) }));
    out.entries.sort_by(|a, b| match (a.sort_ms(), b.sort_ms()) {
        (Some(x), Some(y)) => y.cmp(&x).then_with(|| a.name.cmp(&b.name)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.name.cmp(&b.name),
    });
    out.unreadable.sort_by(|a, b| a.0.cmp(&b.0));
    out.finished_unix_ms = std::time::SystemTime::now().duration_since(UNIX_EPOCH).ok().and_then(|d| i64::try_from(d.as_millis()).ok());
    out
}

/// The running scan and the last finished one.
#[derive(Default)]
pub(crate) struct Recordings {
    /// The running scan and the folder it reads.
    running: Option<(PathBuf, JoinHandle<Scan>)>,
    last: Option<Scan>,
}

impl Studio {
    /// `recordings.list`: the last finished scan of the recording folder, and a new scan started
    /// on its own thread when none runs. Returns at once. A scan of another folder, made before
    /// the recording folder changed, is not returned.
    pub(crate) fn list_recordings(&mut self) -> Result<Value, StudioError> {
        self.collect_recordings_scan();
        let folder = recording_folder(&self.profile.output.folder);
        if self.recordings.running.is_none() {
            let for_thread = folder.clone();
            let spawned = std::thread::Builder::new().name("recordings scan".into()).spawn(move || {
                let started = Instant::now();
                scan(&for_thread, started + SCAN_LIMIT, &Instant::now)
            });
            match spawned {
                Ok(h) => self.recordings.running = Some((folder.clone(), h)),
                Err(e) => return Err(StudioError::Output(format!("the recording folder could not be scanned: {e}"))),
            }
        }
        // A scan of the folder the recording folder was before it changed does not count; the
        // scan of the new folder starts at the first call after it ends.
        let scanning = self.recordings.running.as_ref().is_some_and(|(f, _)| *f == folder);
        Ok(match self.recordings.last.as_ref().filter(|last| last.folder == folder) {
            Some(last) => last.json(scanning),
            None => json!({
                "folder": folder.display().to_string(),
                "scanning": scanning,
                "scanned_unix_ms": null,
                "recordings": [],
                "unreadable": [],
                "more": false,
                "timed_out": false,
            }),
        })
    }

    /// Keep the running scan's result when it has finished.
    fn collect_recordings_scan(&mut self) {
        if !self.recordings.running.as_ref().is_some_and(|(_, h)| h.is_finished()) {
            return;
        }
        if let Some((_, h)) = self.recordings.running.take() {
            match h.join() {
                Ok(scan) => self.recordings.last = Some(scan),
                Err(_) => log::warn!("recordings: the scan of the recording folder stopped with a panic"),
            }
        }
    }

    /// Wait for the running scan and keep its result, for tests.
    #[cfg(test)]
    pub(crate) fn recordings_scanned(&mut self) {
        while self.recordings.running.as_ref().is_some_and(|(_, h)| !h.is_finished()) {
            std::thread::yield_now();
        }
        self.collect_recordings_scan();
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use studiocraft_audio_tracks::journal::{Journal, Line};
    use studiocraft_audio_tracks::session::FILES_FOLDER;

    use super::*;

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

    /// A take folder `<stem> Audio` in `folder` whose journal holds `lines`.
    fn take(folder: &Path, stem: &str, lines: &[Line]) {
        let dir = folder.join(format!("{stem} Audio")).join(FILES_FOLDER);
        std::fs::create_dir_all(&dir).unwrap();
        let mut j = Journal::create(&dir).unwrap();
        for l in lines {
            j.write(l).unwrap();
        }
    }

    fn far() -> Instant {
        Instant::now() + Duration::from_secs(3600)
    }

    fn names(s: &Scan) -> Vec<&str> {
        s.entries.iter().map(|e| e.name.as_str()).collect()
    }

    #[test]
    fn recordings_are_joined_with_their_timelines_and_takes_and_listed_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path();
        // Past dates, so the files written now are not older than the recordings.
        let older = "2025-11-20 10-00-00";
        let newer = "2025-11-21 09-00-00";
        std::fs::write(f.join(format!("{older}.mkv")), b"recording").unwrap();
        std::fs::write(f.join(format!("{older}.otio")), b"{}").unwrap();
        take(f, older, &[start(stamp_unix_ms(older)), Line::Marker { at: 5, name: "Goal".into(), number: None }, Line::End { frames: 96_000 }]);
        // The take folder got a numbered name because `<newer> Audio` was taken. Its journal gives
        // no start, so the take's time is the journal's last change, not the recording's start.
        std::fs::write(f.join(format!("{newer}.MP4")), b"rec").unwrap();
        take(f, &format!("{newer} (2)"), &[start(None)]);
        std::fs::write(f.join("Replay 2025-11-21 11-00-00.mkv"), b"clip").unwrap();
        // A stream's take has no recording file.
        take(f, "Stream 2025-11-19 08-00-00", &[start(stamp_unix_ms("2025-11-19 08-00-00")), Line::End { frames: 48_000 }]);
        // Not listed: other files, a timeline whose recording is gone, files in other folders.
        std::fs::write(f.join("notes.txt"), b"x").unwrap();
        std::fs::write(f.join("gone.otio"), b"{}").unwrap();
        std::fs::create_dir_all(f.join("clips")).unwrap();
        std::fs::write(f.join("clips").join("2025-11-22 10-00-00.mkv"), b"x").unwrap();
        let s = scan(f, far(), &Instant::now);
        assert_eq!(names(&s), ["Replay 2025-11-21 11-00-00", newer, older, "Stream 2025-11-19 08-00-00"]);
        assert!(!s.more && !s.timed_out && s.unreadable.is_empty(), "{s:?}");
        let v = s.json(false);
        let list = v["recordings"].as_array().unwrap();
        assert_eq!(list[0]["kind"], json!("replay"));
        assert_eq!((list[0]["timeline"].clone(), list[0]["audio_tracks"].clone()), (Value::Null, Value::Null));
        assert_eq!((list[1]["kind"].clone(), list[1]["format"].clone()), (json!("recording"), json!("mp4")));
        assert_eq!(list[1]["audio_tracks"]["state"], json!("needs_repair"));
        assert_eq!(list[1]["started_unix_ms"], json!(stamp_unix_ms(newer)), "the recording's start comes from its name");
        assert_eq!(list[2]["timeline"], json!(f.join(format!("{older}.otio")).display().to_string()));
        assert_eq!(list[2]["bytes"], json!(9));
        assert_eq!(list[2]["started_unix_ms"], json!(stamp_unix_ms(older)));
        let tracks = &list[2]["audio_tracks"];
        assert_eq!((tracks["state"].clone(), tracks["markers"].clone(), tracks["seconds"].clone()), (json!("finished"), json!(1), json!(2.0)));
        assert_eq!((list[3]["kind"].clone(), list[3]["file"].clone()), (json!("audio_tracks"), Value::Null));
        assert_eq!(v["scanning"], json!(false));
    }

    #[test]
    fn a_recording_takes_the_timeline_or_take_nearest_its_time_else_the_one_of_its_own_name() {
        let untimed = [
            Owner { stem: "Show", start: None, target: None, modified: None },
            Owner { stem: "Show (3)", start: None, target: None, modified: None },
        ];
        let names = ["Show (2)", "Show", "Show (3)", "Show (4)", "Talk (2)", "Show ()", "Show (x)"];
        let cands: Vec<(&str, Option<i64>)> = names.iter().map(|n| (*n, None)).collect();
        assert_eq!(owners(&cands, &untimed), [None, Some(0), Some(1), None, None, None, None]);
        // With no exact name, the first numbered one is taken and the rest are left over.
        assert_eq!(owners(&[("Show (2)", None), ("Show (5)", None)], &untimed), [Some(0), None]);
        // `Show Audio` is left from an older take; the recording's own take is `Show (2) Audio`.
        let timed = [Owner { stem: "Show", start: Some(1_000_000), target: Some(1_000_000), modified: None }];
        let cands = [("Show", Some(5_000)), ("Show (2)", Some(1_001_500))];
        assert_eq!(owners(&cands, &timed), [None, Some(0)]);
        // A candidate made before the recording started is not its, even with the same name.
        assert_eq!(owners(&[("Show", Some(5_000))], &timed), [None]);
        // Within the slack the nearest one is taken; none near it leaves the name rule.
        let cands = [("Show", Some(1_009_000)), ("Show (2)", Some(1_000_200))];
        assert_eq!(owners(&cands, &timed), [None, Some(0)]);
        let cands = [("Show (2)", Some(9_000_000)), ("Show", Some(8_000_000))];
        assert_eq!(owners(&cands, &timed), [None, Some(0)]);
        // Two recordings of one name (`Show.mkv` and its copy `Show.mp4`): each takes the
        // timeline nearest its last change, and with no times the one changed first is served.
        let twins = [
            Owner { stem: "Show", start: None, target: Some(5_000_000), modified: Some(5_000_000) },
            Owner { stem: "Show", start: None, target: Some(1_000_000), modified: Some(1_000_000) },
        ];
        assert_eq!(owners(&[("Show", Some(1_000_100)), ("Show (2)", Some(5_000_100))], &twins), [Some(1), Some(0)]);
        assert_eq!(owners(&[("Show", None)], &twins), [Some(1)]);
        assert_eq!(numbered_base("Show (12)"), Some("Show"));
        assert_eq!(numbered_base(" (2)"), None);
    }

    #[test]
    fn a_new_recording_does_not_take_the_timeline_or_take_left_from_an_older_one_of_its_name() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path();
        let stem = "2025-11-21 10-00-00";
        let start_ms = stamp_unix_ms(stem).unwrap();
        // Left from an earlier recording of this name whose file was removed.
        take(f, stem, &[start(Some(start_ms - 86_400_000)), Line::End { frames: 48_000 }]);
        let old = std::fs::File::create(f.join(format!("{stem}.otio"))).unwrap();
        old.set_modified(UNIX_EPOCH + Duration::from_millis((start_ms - 86_400_000) as u64)).unwrap();
        drop(old);
        // The new recording, its timeline and its take, each with the next free name.
        let rec = std::fs::File::create(f.join(format!("{stem}.mkv"))).unwrap();
        rec.set_modified(UNIX_EPOCH + Duration::from_millis((start_ms + 60_000) as u64)).unwrap();
        drop(rec);
        let new = std::fs::File::create(f.join(format!("{stem} (2).otio"))).unwrap();
        new.set_modified(UNIX_EPOCH + Duration::from_millis((start_ms + 60_500) as u64)).unwrap();
        drop(new);
        take(f, &format!("{stem} (2)"), &[start(Some(start_ms + 300)), Line::End { frames: 48_000 }]);
        let s = scan(f, far(), &Instant::now);
        let rec = s.entries.iter().find(|e| e.file.is_some()).unwrap();
        assert_eq!(rec.timeline.as_deref(), Some(f.join(format!("{stem} (2).otio")).as_path()));
        assert_eq!(rec.take.as_ref().map(|t| t.name.as_str()), Some(format!("{stem} (2)").as_str()));
        let lone: Vec<&str> = s.entries.iter().filter(|e| e.file.is_none()).map(|e| e.name.as_str()).collect();
        assert_eq!(lone, [stem], "the older take is listed on its own");
    }

    #[test]
    fn a_scan_reads_at_most_its_entries_and_stops_at_its_time_limit() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..=MAX_ENTRIES {
            std::fs::write(dir.path().join(format!("2026-11-21 10-00-00 ({i}).mkv")), b"").unwrap();
        }
        let s = scan(dir.path(), far(), &Instant::now);
        assert!(s.more && !s.timed_out);
        assert_eq!(s.entries.len(), MAX_ENTRIES);
        // A clock that reaches the deadline at its 10th look: the scan stops there, after at most 9
        // entries, and lists no takes.
        let dir = tempfile::tempdir().unwrap();
        for i in 0..20 {
            std::fs::write(dir.path().join(format!("2026-11-21 10-00-{i:02}.mkv")), b"").unwrap();
        }
        take(dir.path(), "2026-11-21 10-00-00", &[start(Some(1))]);
        let base = Instant::now();
        let looks = Cell::new(0u64);
        let now = || {
            looks.set(looks.get() + 1);
            base + Duration::from_secs(looks.get())
        };
        let s = scan(dir.path(), base + Duration::from_secs(10), &now);
        assert!(s.timed_out, "{s:?}");
        assert!((8..=9).contains(&s.entries.len()), "{}", s.entries.len());
        assert!(s.entries.iter().all(|e| e.take.is_none()));
        assert_eq!(looks.get(), 10, "the scan stops at the look that reaches the deadline");
    }

    #[test]
    fn a_folder_that_cannot_be_read_is_reported_once() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        let s = scan(&missing, far(), &Instant::now);
        assert!(s.entries.is_empty());
        assert_eq!(s.unreadable.len(), 1, "{:?}", s.unreadable);
        assert_eq!(s.unreadable[0].0, missing);
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_file_is_not_listed() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        std::fs::write(elsewhere.path().join("secret.mkv"), b"x").unwrap();
        std::os::unix::fs::symlink(elsewhere.path().join("secret.mkv"), dir.path().join("2026-11-21 10-00-00.mkv")).unwrap();
        assert!(scan(dir.path(), far(), &Instant::now).entries.is_empty());
    }

    #[test]
    fn recordings_list_returns_the_last_scan_at_once_and_scans_on_its_own_thread() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("2026-11-21 10-00-00.mkv"), b"x").unwrap();
        let mut s = crate::tests::headless();
        let mut profile = s.profile().clone();
        profile.output.folder = dir.path().display().to_string();
        s.set_profile(profile).unwrap();
        let (v, _) = s.run("recordings.list", &json!({})).unwrap();
        assert_eq!((v["scanning"].clone(), v["recordings"].clone(), v["scanned_unix_ms"].clone()), (json!(true), json!([]), Value::Null));
        assert_eq!(v["folder"], json!(dir.path().display().to_string()));
        s.recordings_scanned();
        std::fs::write(dir.path().join("2026-11-21 11-00-00.mkv"), b"x").unwrap();
        // The finished scan is returned, and a new one starts.
        let (v, _) = s.run("recordings.list", &json!({})).unwrap();
        assert_eq!(v["recordings"].as_array().map(Vec::len), Some(1), "{v}");
        assert_eq!(v["scanning"], json!(true));
        assert!(v["scanned_unix_ms"].is_i64());
        s.recordings_scanned();
        let (v, _) = s.run("recordings.list", &json!({})).unwrap();
        assert_eq!(v["recordings"].as_array().map(Vec::len), Some(2), "{v}");
        assert!(s.run("recordings.list", &json!({"folder": "C:/"})).is_err(), "the command takes no params");
        s.recordings_scanned();
        // After the recording folder changes, the scan of the old folder is not returned.
        let other = tempfile::tempdir().unwrap();
        let mut profile = s.profile().clone();
        profile.output.folder = other.path().display().to_string();
        s.set_profile(profile).unwrap();
        let (v, _) = s.run("recordings.list", &json!({})).unwrap();
        assert_eq!((v["folder"].clone(), v["recordings"].clone()), (json!(other.path().display().to_string()), json!([])), "{v}");
        s.recordings_scanned();
        // A scan of the old folder that is still running does not count as a scan of the new one.
        let (go, wait) = std::sync::mpsc::channel::<()>();
        let held = dir.path().to_path_buf();
        let for_thread = held.clone();
        s.recordings.running = Some((
            held,
            std::thread::spawn(move || {
                let _ = wait.recv();
                scan(&for_thread, far(), &Instant::now)
            }),
        ));
        let (v, _) = s.run("recordings.list", &json!({})).unwrap();
        assert_eq!(v["scanning"], json!(false), "{v}");
        go.send(()).unwrap();
        s.recordings_scanned();
        let (v, _) = s.run("recordings.list", &json!({})).unwrap();
        assert_eq!(v["scanning"], json!(true), "the new folder's scan starts once the old one has ended: {v}");
        s.recordings_scanned();
    }
}
