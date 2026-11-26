//! Markers set while a recording runs, written beside the recording for the video editor.
//!
//! `record.marker` marks the running recording at that moment. When a recording that has at least
//! one marker stops and its file is finished, `<recording name>.otio` is written beside it on a
//! `recording-markers` thread: an OpenTimelineIO timeline (https://opentimelineio.readthedocs.io)
//! with the recording as one clip on a video track and on an audio track, and each marker on the
//! timeline at its frame. FilmCraft's File › Import reads it. A file of that name that already
//! exists is not replaced: the timeline is written as `<recording name> (2).otio`, and so on.
//!
//! The copy a stream saves of what it sends (`Stream <date> <time>.mkv`) is marked the same way:
//! `record.marker` marks the running recording and the running stream's copy, and once the stream
//! has stopped and the copy's file is finished, `<copy name>.otio` is written beside the copy by
//! the same rules.
//!
//! `marker.add` sets one marker on the running recording, the running stream's copy and the
//! running take of audio tracks at once, and keeps it in the session's list that `marker.list`
//! returns. A session lasts from the start of the first of those outputs until all have stopped.
//!
//! `marker.rename` renames one of the session's markers in each output that holds it. While the
//! recording or the copy runs, or while its file is being finished, its marker gets the new name
//! before its timeline is written; once the timeline is written, the file is written again with
//! the new name through a temporary file and a rename. The take of audio tracks renames its marker
//! as [`studiocraft_audio_tracks::TakeMarkers::rename`] describes.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::{Value, json};
use studiocraft_audio_tracks::TakeMarkers;
use studiocraft_media::clock::{duration_of, tick_of};
use studiocraft_media::stats::EncoderStats;
use studiocraft_media::{FrameRate, Tick};

use crate::outputs::OutputKind;
use crate::{Notice, Studio, StudioError};

/// Markers one recording, or one stream's copy, holds at most.
pub(crate) const MAX_MARKERS: usize = 10_000;

/// The output a [`MarkerWatch`] marks, as messages name it: the recording, or with `copy` the
/// stream's copy.
fn noun(copy: bool) -> &'static str {
    if copy { "the stream's copy" } else { "the recording" }
}

/// The largest timeline file `marker.rename` reads.
const MAX_TIMELINE_BYTES: u64 = 64 << 20;

/// A marker: its time on the studio clock from the recording's first frame, and its name.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Marker {
    pub at: Tick,
    pub name: String,
}

/// The encoder counters of a recording. Each frame handed to the encoder is one frame of the
/// file, so once the file is finished they hold the number of frames in it.
#[derive(Clone, Debug)]
pub(crate) struct Written(pub Arc<EncoderStats>);

impl PartialEq for Written {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Written {
    fn frames(&self) -> i64 {
        i64::try_from(self.0.frames.load(Ordering::Relaxed)).unwrap_or(i64::MAX)
    }

    /// Where `at`, a time on the studio clock from the recording's first frame, lies in the
    /// recording: the gaps the recording left out before it (the computer was suspended) are not
    /// counted.
    fn in_recording(&self, at: Tick) -> Tick {
        let at_ns = u64::try_from(duration_of(at).as_nanos()).unwrap_or(u64::MAX);
        let skipped = tick_of(Duration::from_nanos(self.0.skipped_before(at_ns)));
        Tick(at.0.saturating_sub(skipped.0).max(0))
    }
}

/// [`Written::in_recording`], or `at` for a recording without encoder counters.
fn in_recording(written: Option<&Written>, at: Tick) -> Tick {
    written.map_or(at, |w| w.in_recording(at))
}

/// What [`MarkerWatch::observe`] looks up for a new recording: its file, its frame rate and its
/// encoder counters.
pub(crate) type Target = (PathBuf, FrameRate, Option<Written>);

/// A recording that stopped with markers, waiting for its timeline file.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Ended {
    pub recording: PathBuf,
    pub rate: FrameRate,
    /// The recording's length by the clock, from its first frame to its stop.
    pub length: Tick,
    /// The encoder counters. The timeline's clip holds the frames the encoder was handed.
    pub written: Option<Written>,
    /// The part of the output before its file's start, without the gaps the output left out: for
    /// a stream's copy that lost the stream's first packets, the stream up to the copy's first
    /// keyframe. 0 for a file that holds the output from its first frame.
    pub opening: Tick,
    /// The presentation time of the latest video frame the file holds, counted from the output's
    /// first frame, when the output reports it: a stream's copy whose disk fell behind near the
    /// stream's end ends there. `None` when the file holds the frames the encoder was handed.
    pub last_frame: Option<Tick>,
    pub markers: Vec<Marker>,
    /// Why the output could no longer finish its file while it ran, once [`MarkerWatch::fail`]
    /// said so.
    pub failed: Option<String>,
}

struct Watched {
    start: Tick,
    recording: PathBuf,
    rate: FrameRate,
    written: Option<Written>,
    /// The part of the output before its file's start, as far as it is known: see
    /// [`Ended::opening`].
    opening: Tick,
    markers: Vec<Marker>,
}

/// Which recording markers go to: [`MarkerWatch::observe`] follows the recording, and returns the
/// recording that stopped when it had markers. With `copy`, it follows the running stream's copy
/// in the same way.
#[derive(Default)]
pub(crate) struct MarkerWatch {
    recording: Option<Watched>,
    /// Whether the watch follows the copies of streams.
    copy: bool,
    /// The start of the output for which `target` gave nothing, so it is not asked again.
    passed: Option<Tick>,
    /// Why the watched output can no longer finish its file, once [`MarkerWatch::fail`] said so.
    failed: Option<String>,
}

impl MarkerWatch {
    /// A watch for the copy of the running stream.
    pub(crate) fn of_stream_copy() -> MarkerWatch {
        MarkerWatch { copy: true, ..MarkerWatch::default() }
    }

    /// Follow the recording that started at `start` (a recording is told apart by its start).
    /// `target` gives its file, frame rate and encoder counters, and is called only when a new
    /// recording is seen; when it gives nothing, as for a stream that saves no copy, it is not
    /// called again for that start.
    pub(crate) fn observe(&mut self, start: Option<Tick>, target: impl FnOnce() -> Option<Target>, now: Tick) -> Option<Ended> {
        let ended = match (&self.recording, start) {
            (Some(w), Some(s)) if w.start == s => None,
            (Some(_), _) => self.recording.take(),
            (None, _) => None,
        };
        let failed = if self.recording.is_none() { self.failed.take() } else { None };
        if self.recording.is_none()
            && let Some(start) = start
            && self.passed != Some(start)
        {
            match target() {
                Some((recording, rate, written)) => {
                    self.recording = Some(Watched { start, recording, rate, written, opening: Tick(0), markers: Vec::new() });
                }
                None => self.passed = Some(start),
            }
        }
        let w = ended.filter(|w| !w.markers.is_empty())?;
        Some(Ended {
            length: Tick(now.0.saturating_sub(w.start.0).max(0)),
            recording: w.recording,
            rate: w.rate,
            written: w.written,
            opening: Tick(0),
            last_frame: None,
            markers: w.markers,
            failed,
        })
    }

    /// Mark the watched recording at `now`: named `name`, or "Marker N" for its Nth marker.
    /// Returns the marker, and where it lies in the recording as far as the encoder has written:
    /// in a file that starts after the output's opening, from the file's first frame.
    pub(crate) fn add(&mut self, now: Tick, name: Option<String>) -> Result<(Marker, Tick), String> {
        let Some(w) = &mut self.recording else {
            return Err(if self.copy { "no stream with a copy is running" } else { "no recording is running" }.into());
        };
        if let Some(cause) = &self.failed {
            return Err(format!("{} failed: {cause}", noun(self.copy)));
        }
        if w.markers.len() >= MAX_MARKERS {
            let what = if self.copy { "a stream's copy" } else { "a recording" };
            return Err(format!("{what} holds at most {MAX_MARKERS} markers"));
        }
        let name = name.unwrap_or_else(|| format!("Marker {}", w.markers.len() + 1));
        let marker = Marker { at: Tick(now.0.saturating_sub(w.start.0).max(0)), name };
        w.markers.push(marker.clone());
        let placed = Tick(in_recording(w.written.as_ref(), marker.at).0.saturating_sub(w.opening.0).max(0));
        Ok((marker, placed))
    }

    /// Note that the file at `recording` starts `opening` after its output's first frame, as a
    /// stream's copy that lost the stream's first packets does: when that file is watched,
    /// [`MarkerWatch::add`] then gives a marker's place in the file.
    pub(crate) fn set_opening(&mut self, recording: &Path, opening: Tick) {
        if let Some(w) = self.recording.as_mut().filter(|w| w.recording == recording) {
            w.opening = opening;
        }
    }

    /// Whether the watched output writes the file at `recording`.
    fn watches(&self, recording: &Path) -> bool {
        self.recording.as_ref().is_some_and(|w| w.recording == recording)
    }

    /// The opening [`MarkerWatch::set_opening`] gave the watched output; 0 without one.
    fn opening(&self) -> Tick {
        self.recording.as_ref().map_or(Tick(0), |w| w.opening)
    }

    /// The start of the watched recording on the studio clock.
    pub(crate) fn start(&self) -> Option<Tick> {
        self.recording.as_ref().map(|w| w.start)
    }

    /// Note that the watched output can no longer finish its file, for `cause`: it keeps the
    /// markers it holds and takes no more. Nothing happens when no output is watched.
    pub(crate) fn fail(&mut self, cause: &str) {
        if self.recording.is_some() && self.failed.is_none() {
            self.failed = Some(cause.to_string());
        }
    }

    /// Whether the watched output takes markers: it runs and has not failed.
    fn takes_markers(&self) -> bool {
        self.recording.is_some() && self.failed.is_none()
    }

    /// Where the next marker of the watched recording goes: its file and its place among the
    /// recording's markers.
    fn holds(&self, name: &str) -> Option<InRecording> {
        self.recording.as_ref().map(|w| InRecording {
            recording: w.recording.clone(),
            index: w.markers.len(),
            name: name.to_string(),
            copy: self.copy,
        })
    }

    /// Mark the watched recording at `now` for a marker of `marker.add` named `name`: what it did,
    /// and where it holds the marker when it kept it.
    fn mark(&mut self, now: Tick, name: &str) -> (Placed, Option<InRecording>) {
        let Some(held) = self.holds(name) else {
            return (Placed::Off, None);
        };
        match self.add(now, Some(name.to_string())) {
            Ok((_, placed)) => (Placed::Kept(placed.seconds()), Some(held)),
            Err(e) => (Placed::Failed(e), None),
        }
    }

    /// Rename the marker at `index` of the watched recording, named `old`, to `name`. `None` when
    /// the watched recording is not `recording`.
    fn rename(&mut self, recording: &Path, index: usize, old: &str, name: &str) -> Option<Result<(), String>> {
        let w = self.recording.as_mut().filter(|w| w.recording == recording)?;
        if let Some(cause) = &self.failed {
            // Its file will not be finished, so no timeline would carry the new name.
            return Some(Err(format!("{} failed: {cause}", noun(self.copy))));
        }
        Some(rename_listed(&mut w.markers, index, old, name, self.copy))
    }
}

/// Rename the marker at `index` of `markers`, named `old`, to `name`. An error when the list
/// holds no marker named `old` there; it names the recording, or with `copy` the stream's copy.
fn rename_listed(markers: &mut [Marker], index: usize, old: &str, name: &str, copy: bool) -> Result<(), String> {
    match markers.get_mut(index) {
        Some(m) if m.name == old => {
            m.name = name.to_string();
            Ok(())
        }
        _ => Err(format!("{} holds no marker {old:?} in that place", noun(copy))),
    }
}

/// Mark the running recording and the running stream's copy at `now`, for `record.marker`: each
/// names the marker `name`, or "Marker N" for its own Nth marker. Returns the name and time of the
/// marker in the recording, or in the copy when no recording kept it, and what each output did.
/// An output that does not keep the marker is reported, and the other keeps it.
fn mark_recording_and_copy(recording: &mut MarkerWatch, copy: &mut MarkerWatch, now: Tick, name: Option<&str>) -> Result<Value, String> {
    let mark = |w: &mut MarkerWatch| w.start().map(|_| w.add(now, name.map(str::to_owned)).map(|(m, at)| (m.name, at.seconds())));
    let (r, c) = (mark(recording), mark(copy));
    let kept = r.iter().chain(c.iter()).find_map(|x| x.as_ref().ok()).cloned();
    let Some((name, seconds)) = kept else {
        return Err(match (r, c) {
            (Some(Err(r)), Some(Err(c))) => format!("the marker was not kept (the recording: {r}; the stream's copy: {c})"),
            (Some(Err(e)), _) | (_, Some(Err(e))) => e,
            _ => "neither a recording nor a stream's copy is running".into(),
        });
    };
    let json_of = |x: Option<Result<(String, f64), String>>| match x {
        None => Value::Null,
        Some(Ok((name, seconds))) => json!({"name": name, "seconds": seconds}),
        Some(Err(e)) => json!({"error": e}),
    };
    Ok(json!({"name": name, "seconds": seconds, "recording": json_of(r), "stream_copy": json_of(c)}))
}

/// What one output did with a marker of `marker.add`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Placed {
    /// The output does not run.
    Off,
    /// The output kept the marker at this time, in seconds from the output's start.
    Kept(f64),
    /// The output runs and did not keep the marker, for this reason.
    Failed(String),
}

impl Placed {
    /// From what an output's mark returned: `None` when the output does not run.
    fn of(mark: Option<Result<f64, String>>) -> Placed {
        match mark {
            None => Placed::Off,
            Some(Ok(seconds)) => Placed::Kept(seconds),
            Some(Err(e)) => Placed::Failed(e),
        }
    }

    fn json(&self) -> Value {
        match self {
            Placed::Off => Value::Null,
            Placed::Kept(seconds) => json!({"seconds": seconds}),
            Placed::Failed(e) => json!({"error": e}),
        }
    }
}

/// Where a recording, or with `copy` a stream's copy, holds a marker of `marker.add`: its file,
/// the marker's place among its markers, and the name it gave the marker last.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct InRecording {
    pub recording: PathBuf,
    pub index: usize,
    pub name: String,
    pub copy: bool,
}

/// Where a take of audio tracks holds a marker: the take's marker log, the marker's number in the
/// take, its take frame, and the name the take gave it last.
#[derive(Clone)]
pub(crate) struct InTake {
    pub markers: TakeMarkers,
    pub number: u64,
    pub at: u64,
    pub name: String,
}

impl InTake {
    /// The marker's time in the take, in seconds.
    pub(crate) fn seconds(&self) -> f64 {
        self.at as f64 / 48_000.0
    }
}

/// A marker `marker.add` set: its number in the session (from 1), its name, its time from the
/// session's start, its time in each output that kept it, and where each of those outputs holds
/// it.
#[derive(Clone)]
pub(crate) struct SessionMarker {
    pub index: usize,
    pub name: String,
    pub seconds: f64,
    pub recording: Option<f64>,
    pub stream_copy: Option<f64>,
    pub audio_tracks: Option<f64>,
    pub in_recording: Option<InRecording>,
    pub in_copy: Option<InRecording>,
    pub in_take: Option<InTake>,
    /// The part of the stream before the copy's file, in seconds, as far as it was known when
    /// `stream_copy` was last placed; [`MarkerSession::copy_opened`] places it again.
    pub copy_opening: f64,
}

impl SessionMarker {
    fn json(&self) -> Value {
        let kept = |s: Option<f64>| s.map_or(Value::Null, |s| json!({"seconds": s}));
        json!({
            "index": self.index,
            "name": self.name,
            "seconds": self.seconds,
            "recording": kept(self.recording),
            "stream_copy": kept(self.stream_copy),
            "audio_tracks": kept(self.audio_tracks),
        })
    }
}

/// What each output did with one marker of `marker.add` or `marker.rename`.
pub(crate) struct Results {
    pub recording: Placed,
    pub stream_copy: Placed,
    pub audio_tracks: Placed,
}

impl Results {
    /// The outputs with what each did, as messages name them.
    fn named(&self) -> [(&'static str, &Placed); 3] {
        [("the recording", &self.recording), ("the stream's copy", &self.stream_copy), ("the audio tracks", &self.audio_tracks)]
    }

    /// `v` with each output's result.
    fn into_json(self, mut v: Value) -> Value {
        v["recording"] = self.recording.json();
        v["stream_copy"] = self.stream_copy.json();
        v["audio_tracks"] = self.audio_tracks.json();
        v
    }
}

/// The message when no session runs.
const NO_SESSION: &str = "no recording, stream copy or take of audio tracks is running";

/// The markers of a session: the time while the recording, the stream's copy or a take of audio
/// tracks runs, from the start of the first of them until all have stopped. The last session's
/// markers stay listed until the next session starts.
#[derive(Default)]
pub(crate) struct MarkerSession {
    /// The session's start on the studio clock, while it runs.
    start: Option<Tick>,
    markers: Vec<SessionMarker>,
}

impl MarkerSession {
    /// Follow the recording or stream's copy that started first, at `started`, and whether a take
    /// runs. A session starts when one of them runs and no session runs, at that start or at `now`
    /// for a take alone. It ends when a look finds none running, so a take that hands over to the
    /// next one while a stream runs keeps the session. Runs after each runtime command and each
    /// tick.
    pub(crate) fn observe(&mut self, started: Option<Tick>, take_runs: bool, now: Tick) {
        let runs = started.is_some() || take_runs;
        if runs && self.start.is_none() {
            self.start = Some(started.unwrap_or(now));
            self.markers.clear();
        } else if !runs {
            self.start = None;
        }
    }

    /// Whether a session runs.
    pub(crate) fn running(&self) -> bool {
        self.start.is_some()
    }

    /// The number and name of the next marker: `name`, or "Marker N" for the session's Nth
    /// marker. An error when no session runs or the session holds [`MAX_MARKERS`].
    pub(crate) fn next(&self, name: Option<String>) -> Result<(usize, String), String> {
        if self.start.is_none() {
            return Err(NO_SESSION.into());
        }
        if self.markers.len() >= MAX_MARKERS {
            return Err(format!("a session holds at most {MAX_MARKERS} markers"));
        }
        let index = self.markers.len() + 1;
        Ok((index, name.unwrap_or_else(|| format!("Marker {index}"))))
    }

    /// Keep the marker [`MarkerSession::next`] numbered, set at `now`, with what each output did
    /// with it and where each output that kept it holds it, the copy with the opening its time
    /// leaves out. Returns the marker's number, name, time from the session's start and each
    /// output's result. When no output kept it, nothing is kept and the error names each cause.
    pub(crate) fn push(
        &mut self,
        (index, name): (usize, String),
        now: Tick,
        results: Results,
        (in_recording, in_copy, in_take): (Option<InRecording>, Option<(InRecording, f64)>, Option<InTake>),
    ) -> Result<Value, String> {
        let Some(start) = self.start else {
            return Err(NO_SESSION.into());
        };
        let kept = |p: &Placed| match p {
            Placed::Kept(s) => Some(*s),
            _ => None,
        };
        let marker = SessionMarker {
            index,
            name,
            seconds: Tick(now.0.saturating_sub(start.0).max(0)).seconds(),
            recording: kept(&results.recording),
            stream_copy: kept(&results.stream_copy),
            audio_tracks: kept(&results.audio_tracks),
            in_recording: in_recording.filter(|_| kept(&results.recording).is_some()),
            copy_opening: in_copy.as_ref().map_or(0.0, |(_, opening)| *opening),
            in_copy: in_copy.map(|(r, _)| r).filter(|_| kept(&results.stream_copy).is_some()),
            in_take: in_take.filter(|_| kept(&results.audio_tracks).is_some()),
        };
        if marker.recording.is_none() && marker.stream_copy.is_none() && marker.audio_tracks.is_none() {
            return Err(not_done("the marker was not kept", &results).unwrap_or_else(|| NO_SESSION.into()));
        }
        let v = results.into_json(marker.json());
        self.markers.push(marker);
        Ok(v)
    }

    /// Note that the file of the stream's copy at `copy` starts `opening` seconds into the stream:
    /// each marker the copy holds gets its time in the file, at 0 for one from before the file's
    /// start.
    pub(crate) fn copy_opened(&mut self, copy: &Path, opening: f64) {
        for m in self.markers.iter_mut().filter(|m| m.in_copy.as_ref().is_some_and(|r| r.recording == copy)) {
            if let Some(s) = m.stream_copy.as_mut() {
                *s = (*s + m.copy_opening - opening).max(0.0);
            }
            m.copy_opening = opening;
        }
    }

    /// The files of the streams' copies that hold the session's markers, each once.
    pub(crate) fn copy_files(&self) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = Vec::new();
        for r in self.markers.iter().filter_map(|m| m.in_copy.as_ref()) {
            if !files.contains(&r.recording) {
                files.push(r.recording.clone());
            }
        }
        files
    }

    /// The session's marker numbered `index` (from 1).
    pub(crate) fn get(&self, index: usize) -> Result<&SessionMarker, String> {
        index.checked_sub(1).and_then(|i| self.markers.get(i)).ok_or_else(|| match self.markers.len() {
            0 => format!("there is no marker {index}: the session holds no markers"),
            n => format!("there is no marker {index}: the session holds markers 1 to {n}"),
        })
    }

    /// Note that the session's marker `index` was renamed to `name` in each output that `results`
    /// reports as done. Returns the marker as `marker.list` lists it, with each output's result.
    /// When no output renamed it, the marker keeps its name and the error names each cause.
    pub(crate) fn renamed(&mut self, index: usize, name: String, results: Results) -> Result<Value, String> {
        let Some(m) = index.checked_sub(1).and_then(|i| self.markers.get_mut(i)) else {
            return Err(format!("there is no marker {index}"));
        };
        let done = |p: &Placed| matches!(p, Placed::Kept(_));
        if !results.named().iter().any(|(_, p)| done(p)) {
            return Err(not_done("the marker was not renamed", &results)
                .unwrap_or_else(|| format!("no recording, stream copy or take of audio tracks holds marker {index}")));
        }
        if let Some(r) = m.in_recording.as_mut().filter(|_| done(&results.recording)) {
            r.name.clone_from(&name);
        }
        if let Some(r) = m.in_copy.as_mut().filter(|_| done(&results.stream_copy)) {
            r.name.clone_from(&name);
        }
        if let Some(t) = m.in_take.as_mut().filter(|_| done(&results.audio_tracks)) {
            t.name.clone_from(&name);
        }
        m.name = name;
        Ok(results.into_json(m.json()))
    }

    /// The session's markers, oldest first, and whether the session runs.
    pub(crate) fn json(&self) -> Value {
        json!({"running": self.running(), "markers": self.markers.iter().map(SessionMarker::json).collect::<Vec<_>>()})
    }
}

/// `what`, followed by the cause each output that failed gave, or `None` when none failed.
fn not_done(what: &str, results: &Results) -> Option<String> {
    let causes: Vec<String> = results
        .named()
        .into_iter()
        .filter_map(|(output, p)| match p {
            Placed::Failed(e) => Some(format!("{output}: {e}")),
            _ => None,
        })
        .collect();
    (!causes.is_empty()).then(|| format!("{what} ({})", causes.join("; ")))
}

/// The `marked_range` of each marker of the timeline written for `ended`, in order.
pub(crate) fn marker_ranges(ended: &Ended) -> Vec<Value> {
    let v = timeline(ended);
    let markers = v.pointer("/tracks/markers").and_then(Value::as_array);
    markers.map(|ms| ms.iter().map(|m| m["marked_range"].clone()).collect()).unwrap_or_default()
}

/// Rename the marker at `index` of the timeline file `path`, named `old`, to `name`, and write
/// the file again through a temporary file and a rename. `ranges` holds each marker's
/// `marked_range` as the file was written ([`marker_ranges`]). The file's size is checked before
/// it is read. A file that has no marker named `old` at that place, or another number of markers,
/// or the marker at another time, as after it was changed in another app, is an error, and the
/// file is left as it is. So is a file that changed after it was read, as when another app saved
/// it meanwhile ([`studiocraft_audio_tracks::replace::replace_unchanged`]). A marker that is
/// already named `name` is not written again.
pub(crate) fn rename_in_timeline(path: &Path, ranges: &[Value], index: usize, old: &str, name: &str) -> Result<(), String> {
    use std::io::Read;
    let at = |e: &dyn std::fmt::Display| format!("{}: {e}", path.display());
    let meta = std::fs::symlink_metadata(path).map_err(|e| at(&e))?;
    if !meta.is_file() {
        return Err(at(&"not a file"));
    }
    let too_large = || at(&format!("larger than {MAX_TIMELINE_BYTES} bytes"));
    if meta.len() > MAX_TIMELINE_BYTES {
        return Err(too_large());
    }
    let mut bytes = Vec::with_capacity(usize::try_from(meta.len()).unwrap_or(0));
    let file = std::fs::File::open(path).map_err(|e| at(&e))?;
    // The file can grow between the size check and the read; the read stops one byte past it.
    file.take(MAX_TIMELINE_BYTES.saturating_add(1)).read_to_end(&mut bytes).map_err(|e| at(&e))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_TIMELINE_BYTES {
        return Err(too_large());
    }
    let mut v: Value = serde_json::from_slice(&bytes).map_err(|e| at(&e))?;
    let markers = v.pointer("/tracks/markers").and_then(Value::as_array);
    let same = markers.is_some_and(|ms| ms.len() == ranges.len() && ms.get(index).map(|m| &m["marked_range"]) == ranges.get(index));
    let slot = v.pointer_mut(&format!("/tracks/markers/{index}/name")).filter(|n| same && n.as_str() == Some(old));
    let Some(slot) = slot else {
        return Err(at(&format!("the timeline has no marker {old:?} in that place")));
    };
    if old == name {
        return Ok(());
    }
    *slot = Value::String(name.to_string());
    let out = serde_json::to_vec_pretty(&v).map_err(|e| at(&e))?;
    #[cfg(test)]
    if let Some(save) = BEFORE_TIMELINE_WRITE.with(std::cell::Cell::get) {
        save(path);
    }
    studiocraft_audio_tracks::replace::replace_unchanged(path, &bytes, &out).map_err(|e| at(&e))
}

#[cfg(test)]
thread_local! {
    /// Run on the timeline file's path after a rename has read and checked it, before the rename
    /// writes it: a test saves the file there as another app would.
    static BEFORE_TIMELINE_WRITE: std::cell::Cell<Option<fn(&Path)>> = const { std::cell::Cell::new(None) };
}

/// `name` as a relative URL: every byte outside the unreserved characters of RFC 3986 section
/// 2.3 is percent-encoded.
fn url_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The first frame at `rate` that starts at or after `t`: the frame a packet at `t` holds. A
/// packet's time in whole nanoseconds lies up to a nanosecond before its frame's start, and still
/// gives that frame.
fn frame_starting_at(rate: FrameRate, t: Tick) -> i64 {
    let f = rate.frame_at(t);
    if rate.tick_of(f) < t { f.saturating_add(1) } else { f }
}

fn rational_time(frames: i64, rate: FrameRate) -> Value {
    json!({"OTIO_SCHEMA": "RationalTime.1", "rate": rate.as_f64(), "value": frames as f64})
}

fn time_range(start: i64, duration: i64, rate: FrameRate) -> Value {
    json!({"OTIO_SCHEMA": "TimeRange.1", "start_time": rational_time(start, rate), "duration": rational_time(duration, rate)})
}

/// The OpenTimelineIO timeline for `ended`: the recording, named by its file name relative to the
/// timeline file, as one clip on a video track and an audio track, and each marker at the frame
/// that holds its time in the recording, without the gaps the recording left out before it. The
/// clip holds the frames the encoder was handed, or without encoder counters the frames that start
/// before the stop. The frames of the output's opening that its file does not hold
/// ([`Ended::opening`]) are left out of the clip, and the markers are placed in the file. A file
/// that ends before the last of those frames ([`Ended::last_frame`]) ends the clip there too. A
/// marker after the last frame is put on the last frame, and one before the file's start on its
/// first.
pub(crate) fn timeline(ended: &Ended) -> Value {
    let rate = ended.rate.sane();
    let file = ended.recording.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let title = ended.recording.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    // The frames that start before the stop.
    let by_clock = || if ended.length.0 > 0 { rate.frame_at(Tick(ended.length.0 - 1)).saturating_add(1) } else { 1 };
    // The frames before the file's start.
    let before = if ended.opening.0 > 0 { frame_starting_at(rate, ended.opening) } else { 0 };
    let handed = ended.written.as_ref().map_or_else(by_clock, Written::frames);
    // The frames up to the file's last one.
    let held = ended.last_frame.map_or(handed, |t| handed.min(frame_starting_at(rate, t).saturating_add(1)));
    // A recording is at least one frame.
    let frames = held.saturating_sub(before).max(1);
    let available = time_range(0, frames, rate);
    let clip = json!({
        "OTIO_SCHEMA": "Clip.2",
        "name": title,
        "source_range": available,
        "media_references": {"DEFAULT_MEDIA": {
            "OTIO_SCHEMA": "ExternalReference.1",
            "name": file,
            "target_url": url_name(&file),
            "available_range": available,
            "available_image_bounds": null,
            "metadata": {},
        }},
        "active_media_reference_key": "DEFAULT_MEDIA",
        "effects": [],
        "markers": [],
        "enabled": true,
        "metadata": {},
    });
    let track = |name: &str, kind: &str| {
        json!({
            "OTIO_SCHEMA": "Track.1",
            "name": name,
            "kind": kind,
            "source_range": null,
            "children": [clip],
            "effects": [],
            "markers": [],
            "enabled": true,
            "metadata": {},
        })
    };
    let markers: Vec<Value> = ended
        .markers
        .iter()
        .map(|m| {
            json!({
                "OTIO_SCHEMA": "Marker.2",
                "name": m.name,
                "comment": "",
                "color": "GREEN",
                "marked_range": time_range(rate.frame_at(in_recording(ended.written.as_ref(), m.at)).saturating_sub(before).clamp(0, frames - 1), 0, rate),
                "metadata": {},
            })
        })
        .collect();
    json!({
        "OTIO_SCHEMA": "Timeline.1",
        "name": title,
        "global_start_time": null,
        "metadata": {},
        "tracks": {
            "OTIO_SCHEMA": "Stack.1",
            "name": title,
            "source_range": null,
            "children": [track("Video", "Video"), track("Audio", "Audio")],
            "effects": [],
            "markers": markers,
            "enabled": true,
            "metadata": {},
        },
    })
}

/// Names a timeline file tries before it gives up: `<recording>.otio`, then `<recording> (2).otio`
/// up to this number.
const MAX_NAME_NUMBER: u32 = 999;

/// Write `<recording>.otio` for `ended`; the path written. A file that exists is left as it is,
/// and the next free name of `<recording> (N).otio` is used instead. A file that could not be
/// written in full is removed.
pub(crate) fn write_timeline(ended: &Ended) -> Result<PathBuf, String> {
    use std::io::Write;
    let first = ended.recording.with_extension("otio");
    let bytes = serde_json::to_vec_pretty(&timeline(ended)).map_err(|e| format!("{}: {e}", first.display()))?;
    let stem = ended.recording.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let mut opened = None;
    for n in 1..=MAX_NAME_NUMBER {
        let path = if n == 1 { first.clone() } else { first.with_file_name(format!("{stem} ({n}).otio")) };
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => {
                opened = Some((path, file));
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(format!("{}: {e}", path.display())),
        }
    }
    let Some((path, mut file)) = opened else {
        return Err(format!("{}: {MAX_NAME_NUMBER} timeline files with this name already exist", first.display()));
    };
    match file.write_all(&bytes).and_then(|()| file.sync_all()) {
        Ok(()) => Ok(path),
        Err(e) => {
            drop(file);
            let _ = std::fs::remove_file(&path);
            Err(format!("{}: {e}", path.display()))
        }
    }
}

/// The markers of the running recording and of the running stream's copy, the recordings and
/// copies whose files are still being finished, the timeline files being written with their
/// recordings, the markers of the session, and the timeline files written for the session's
/// recordings and copies.
pub(crate) struct RecordingMarkers {
    watch: MarkerWatch,
    stream: MarkerWatch,
    session: MarkerSession,
    finishing: Vec<Ended>,
    /// Copies of streams that stopped with markers, until their files are finished.
    copies_finishing: Vec<Ended>,
    writes: Vec<(PathBuf, JoinHandle<Result<(PathBuf, Vec<Value>), String>>)>,
    /// Each recording or copy of the session whose timeline was written, the timeline file, and
    /// the `marked_range` of each marker as it was written.
    timelines: Vec<(PathBuf, PathBuf, Vec<Value>)>,
}

impl Default for RecordingMarkers {
    fn default() -> Self {
        RecordingMarkers {
            watch: MarkerWatch::default(),
            stream: MarkerWatch::of_stream_copy(),
            session: MarkerSession::default(),
            finishing: Vec::new(),
            copies_finishing: Vec::new(),
            writes: Vec::new(),
            timelines: Vec::new(),
        }
    }
}

/// The ended outputs of `list` that `ready` finds ready, taken out of it. A list without one is
/// left as it is, so a tick allocates nothing.
fn take_ready(list: &mut Vec<Ended>, ready: impl Fn(&Ended) -> bool) -> Vec<Ended> {
    if !list.iter().any(&ready) {
        return Vec::new();
    }
    let (taken, waiting): (Vec<_>, Vec<_>) = std::mem::take(list).into_iter().partition(ready);
    *list = waiting;
    taken
}

impl Studio {
    /// Follow the recording for markers, and start writing the timeline of a recording that
    /// stopped with markers once its file is finished. Runs each tick and after each runtime
    /// command.
    pub(crate) fn sync_recording_markers(&mut self) {
        let now = self.clock.now();
        let start = self.outputs.start_of(OutputKind::Recording);
        let outputs = &self.outputs;
        let video = self.profile.video.sanitized();
        let target = || {
            let path = PathBuf::from(outputs.state(OutputKind::Recording)?.target);
            let written = outputs.stats_of(OutputKind::Recording).map(Written);
            Some((path, FrameRate::new(i64::from(video.fps_num), i64::from(video.fps_den)), written))
        };
        if let Some(ended) = self.recording_markers.watch.observe(start, target, now) {
            self.recording_markers.finishing.push(ended);
        }
        // The running stream's copy, when it writes one; a stream's encoder counters count the
        // frames the copy gets. The copy writes each packet at its own time from the first one it
        // got, so packets it loses later leave a gap in the file rather than shorten it; a copy
        // that lost the stream's first packets starts later, and its timeline leaves that opening
        // out. A copy that could not write its last packets is not finished and gets no timeline. `observe` calls `copy` once for each stream it sees, not each
        // frame; a failure later is read from `running_copy_failure`, which reads no stream state.
        let stream = self.outputs.start_of(OutputKind::Streaming);
        let mut failed_at_start = None;
        let copy = || {
            let c = outputs.stream_copy()?;
            failed_at_start = c.failure;
            let written = outputs.stats_of(OutputKind::Streaming).map(Written);
            Some((c.path, FrameRate::new(i64::from(video.fps_num), i64::from(video.fps_den)), written))
        };
        if let Some(ended) = self.recording_markers.stream.observe(stream, copy, now) {
            self.recording_markers.copies_finishing.push(ended);
        }
        // A copy that failed while its stream runs, or before it was first seen, cannot be
        // finished, so it takes no more markers and a marker gets the cause.
        if self.recording_markers.stream.takes_markers()
            && let Some(cause) = failed_at_start.as_deref().or(self.outputs.running_copy_failure())
        {
            self.recording_markers.stream.fail(cause);
        }
        let take_runs = self.audio_tracks_running();
        let ran = self.recording_markers.session.running();
        // A copy that failed takes no markers, so it does not keep a session running.
        let copy_start = self.recording_markers.stream.start().filter(|_| self.recording_markers.stream.takes_markers());
        let started = [start, copy_start].into_iter().flatten().min();
        self.recording_markers.session.observe(started, take_runs, now);
        if !ran && self.recording_markers.session.running() {
            // The last session's markers are no longer listed, so their timelines are not renamed.
            self.recording_markers.timelines.clear();
        }
        self.write_finished_marker_files();
        self.report_marker_files(false);
    }

    /// Start writing the timelines of the recordings and copies whose files are finished.
    fn write_finished_marker_files(&mut self) {
        let outputs = &self.outputs;
        let finished = |e: &Ended| !outputs.finishing(OutputKind::Recording, &e.recording.to_string_lossy());
        let recordings = take_ready(&mut self.recording_markers.finishing, finished);
        let copies = take_ready(&mut self.recording_markers.copies_finishing, |e| outputs.copy_ended(&e.recording).is_some());
        for ended in recordings {
            self.write_marker_file(ended, false);
        }
        for ended in copies {
            // In the tick that sees the copy end, while its start is still known.
            if let Some(start_ns) = self.outputs.copy_start(&ended.recording) {
                self.recording_markers.session.copy_opened(&ended.recording, tick_of(Duration::from_nanos(start_ns)).seconds());
            }
            self.write_marker_file(ended, true);
        }
    }

    /// Start writing the timeline of `ended`, a recording or with `copy` a stream's copy, on a
    /// thread of its own, so a slow disk does not hold up a frame. A recording or copy that did not
    /// finish its file cleanly gets no timeline.
    fn write_marker_file(&mut self, mut ended: Ended, copy: bool) {
        let unfinished = if copy {
            self.outputs.copy_ended(&ended.recording) != Some(true)
        } else {
            self.outputs.take_unfinished(OutputKind::Recording, &ended.recording.to_string_lossy())
        };
        if unfinished {
            let error = format!("{}: {} was not finished", ended.recording.display(), noun(copy));
            self.notices.push(Notice::new("The markers could not be saved: {error}", vec![("error", error)], true));
            return;
        }
        if copy {
            ended.opening = tick_of(Duration::from_nanos(self.outputs.copy_start(&ended.recording).unwrap_or(0)));
            ended.last_frame = self.outputs.copy_last_video(&ended.recording).map(|ns| tick_of(Duration::from_nanos(ns)));
        }
        let recording = ended.recording.clone();
        match std::thread::Builder::new().name("recording-markers".into()).spawn(move || write_timeline(&ended).map(|p| (p, marker_ranges(&ended)))) {
            Ok(h) => self.recording_markers.writes.push((recording, h)),
            Err(e) => self.notices.push(Notice::new("The markers could not be saved: {error}", vec![("error", e.to_string())], true)),
        }
    }

    /// Mark the running recording and the running stream's copy now, for `record.marker`: each
    /// names the marker `name`, or "Marker N" for its own Nth marker. Returns the marker's name and
    /// time in the recording, or in the copy without a recording, and what each output did.
    pub(crate) fn add_recording_marker(&mut self, name: Option<String>) -> Result<Value, StudioError> {
        self.sync_recording_markers();
        self.note_stream_copy();
        let now = self.clock.now();
        let rm = &mut self.recording_markers;
        mark_recording_and_copy(&mut rm.watch, &mut rm.stream, now, name.as_deref()).map_err(StudioError::Output)
    }

    /// Give the watch of the running stream's copy, and the session's markers in the copies of
    /// streams that ran and that run, the part of the stream before each copy's file starts, so
    /// each marker reports its time in the file. A running copy that has failed since the last
    /// poll takes no more markers, and one whose stream has started stopping since then no longer
    /// renames its markers. It reads the stream's state, so it runs for a marker command,
    /// not each frame.
    fn note_stream_copy(&mut self) {
        for copy in self.recording_markers.session.copy_files() {
            if let Some(start_ns) = self.outputs.copy_start(&copy) {
                self.recording_markers.session.copy_opened(&copy, tick_of(Duration::from_nanos(start_ns)).seconds());
            }
        }
        let Some(c) = self.outputs.stream_copy() else { return };
        let opening = tick_of(Duration::from_nanos(c.start_ns));
        self.recording_markers.stream.set_opening(&c.path, opening);
        self.recording_markers.session.copy_opened(&c.path, opening.seconds());
        let Some(cause) = c.failure.as_deref() else { return };
        if self.recording_markers.stream.watches(&c.path) {
            if self.recording_markers.stream.takes_markers() {
                self.recording_markers.stream.fail(cause);
                // A failed copy no longer keeps the session running.
                self.sync_recording_markers();
            }
        } else if let Some(e) = self.recording_markers.copies_finishing.iter_mut().rev().find(|e| e.recording == c.path) {
            // The copy failed after the last poll, and its stream has started stopping since:
            // its file will not be finished, so its markers are no longer renamed.
            e.failed.get_or_insert_with(|| cause.to_string());
        }
    }

    /// Whether File › Add Recording Marker has an output to mark: a running recording, or a
    /// running stream that saves a copy.
    pub fn can_add_recording_marker(&self) -> bool {
        self.recording_markers.watch.start().is_some() || self.recording_markers.stream.takes_markers()
    }

    /// Mark the running recording, the running stream's copy and the running take of audio tracks
    /// now with one marker, for `marker.add`: named `name`, or "Marker N" for the session's Nth
    /// marker. An output that does not keep it is reported, and the others keep it.
    pub(crate) fn add_marker(&mut self, name: Option<String>) -> Result<Value, StudioError> {
        self.sync_recording_markers();
        self.note_stream_copy();
        let now = self.clock.now();
        let next = self.recording_markers.session.next(name).map_err(StudioError::Output)?;
        let (recording, in_recording) = self.recording_markers.watch.mark(now, &next.1);
        let (stream_copy, in_copy) = self.recording_markers.stream.mark(now, &next.1);
        let in_copy = in_copy.map(|r| (r, self.recording_markers.stream.opening().seconds()));
        let take = self.mark_audio_tracks(now, &next.1);
        let audio_tracks = Placed::of(take.as_ref().map(|r| r.as_ref().map(InTake::seconds).map_err(String::clone)));
        let in_take = take.and_then(Result::ok);
        let results = Results { recording, stream_copy, audio_tracks };
        self.recording_markers.session.push(next, now, results, (in_recording, in_copy, in_take)).map_err(StudioError::Output)
    }

    /// Rename the session's marker `index` (from 1) to `name` in each output that holds it, for
    /// `marker.rename`. An output that does not rename it is reported, and the other keeps the
    /// new name. An output that already gives the marker that name is not written again.
    pub(crate) fn rename_marker(&mut self, index: usize, name: String) -> Result<Value, StudioError> {
        self.sync_recording_markers();
        self.note_stream_copy();
        let m = self.recording_markers.session.get(index).map_err(StudioError::Output)?.clone();
        let mut rename_in = |held: &Option<InRecording>, kept: Option<f64>| match (held, kept) {
            (Some(r), Some(seconds)) => match self.rename_recording_marker(r, &name) {
                Ok(()) => Placed::Kept(seconds),
                Err(e) => Placed::Failed(e),
            },
            _ => Placed::Off,
        };
        let recording = rename_in(&m.in_recording, m.recording);
        let stream_copy = rename_in(&m.in_copy, m.stream_copy);
        let audio_tracks = match (&m.in_take, m.audio_tracks) {
            // A running take holds the name it was given; an ended take's file is checked.
            (Some(t), Some(seconds)) if t.name == name && !t.markers.ended() => Placed::Kept(seconds),
            (Some(t), Some(seconds)) => match t.markers.rename(t.number, &t.name, &name) {
                Ok(()) => Placed::Kept(seconds),
                Err(e) => Placed::Failed(e.to_string()),
            },
            _ => Placed::Off,
        };
        let results = Results { recording, stream_copy, audio_tracks };
        self.recording_markers.session.renamed(index, name, results).map_err(StudioError::Output)
    }

    /// Rename the marker `r` of a recording or a stream's copy to `name`: in the running
    /// recording's or copy's list or in the list of one whose file is being finished, or in its
    /// timeline file once that is written. A written timeline file is checked even for a rename to
    /// the name the marker has.
    fn rename_recording_marker(&mut self, r: &InRecording, name: &str) -> Result<(), String> {
        let rm = &mut self.recording_markers;
        let (watch, finishing) = if r.copy { (&mut rm.stream, &mut rm.copies_finishing) } else { (&mut rm.watch, &mut rm.finishing) };
        if let Some(renamed) = watch.rename(&r.recording, r.index, &r.name, name) {
            return renamed;
        }
        if let Some(e) = finishing.iter_mut().find(|e| e.recording == r.recording) {
            if let Some(cause) = &e.failed {
                // Its file will not be finished, so no timeline would carry the new name.
                return Err(format!("{} failed: {cause}", noun(r.copy)));
            }
            return rename_listed(&mut e.markers, r.index, &r.name, name, r.copy);
        }
        if rm.writes.iter().any(|(recording, _)| *recording == r.recording) {
            if r.name == name {
                return Ok(());
            }
            return Err("the timeline file is still being written; rename the marker once it is saved".into());
        }
        match rm.timelines.iter().find(|(recording, _, _)| *recording == r.recording) {
            Some((_, timeline, ranges)) => rename_in_timeline(timeline, ranges, r.index, &r.name, name),
            None => Err(format!("{} has no timeline file", noun(r.copy))),
        }
    }

    /// The markers `marker.add` set in the running session, or in the last one, for
    /// `marker.list`.
    pub(crate) fn marker_list(&mut self) -> Value {
        self.sync_recording_markers();
        self.note_stream_copy();
        self.recording_markers.session.json()
    }

    /// End the markers of the running recording and the running stream's copy at `closed`, the
    /// time the app closed, write the timelines still to be written, and wait for every timeline
    /// file. Runs after the outputs have finished their files.
    pub(crate) fn finish_recording_markers_blocking(&mut self, closed: Tick) {
        if let Some(ended) = self.recording_markers.watch.observe(None, || None, closed) {
            self.recording_markers.finishing.push(ended);
        }
        if let Some(ended) = self.recording_markers.stream.observe(None, || None, closed) {
            self.recording_markers.copies_finishing.push(ended);
        }
        for ended in std::mem::take(&mut self.recording_markers.finishing) {
            self.write_marker_file(ended, false);
        }
        for ended in std::mem::take(&mut self.recording_markers.copies_finishing) {
            self.write_marker_file(ended, true);
        }
        self.report_marker_files(true);
    }

    /// Report the timeline files that have been written: those finished, or with `wait` all of them.
    fn report_marker_files(&mut self, wait: bool) {
        // Nothing is moved while every write still runs, so a tick allocates nothing.
        if !self.recording_markers.writes.iter().any(|(_, h)| wait || h.is_finished()) {
            return;
        }
        let (done, running): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.recording_markers.writes).into_iter().partition(|(_, h)| wait || h.is_finished());
        self.recording_markers.writes = running;
        for (recording, h) in done {
            match h.join().unwrap_or_else(|_| Err("the marker file could not be written".into())) {
                Ok((path, ranges)) => {
                    self.notices.push(Notice::new("Markers saved to {path}", vec![("path", path.display().to_string())], false));
                    self.recording_markers.timelines.push((recording, path, ranges));
                }
                Err(e) => self.notices.push(Notice::new("The markers could not be saved: {error}", vec![("error", e)], true)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(ms: i64) -> Tick {
        Tick::from_units(ms, 1000)
    }

    fn rec(name: &str) -> Option<Target> {
        Some((PathBuf::from(name), FrameRate::new(30, 1), None))
    }

    fn written(frames: u64) -> Written {
        let stats = EncoderStats::default();
        stats.frames.store(frames, Ordering::Relaxed);
        Written(Arc::new(stats))
    }

    #[test]
    fn markers_go_to_the_running_recording_and_come_back_when_it_stops() {
        let mut w = MarkerWatch::default();
        assert_eq!(w.add(t(0), None).unwrap_err(), "no recording is running");
        assert_eq!(w.observe(Some(t(1000)), || rec("a.mkv"), t(1000)), None);
        let m = w.add(t(3500), Some("Goal".into())).unwrap();
        assert_eq!(m, (Marker { at: t(2500), name: "Goal".into() }, t(2500)));
        assert_eq!(w.add(t(4000), None).unwrap().0.name, "Marker 2");
        // The same recording seen again is not looked up again.
        assert_eq!(w.observe(Some(t(1000)), || panic!("looked up again"), t(5000)), None);
        let ended = w.observe(None, || panic!("no recording runs"), t(6000)).unwrap();
        assert_eq!(ended.recording, PathBuf::from("a.mkv"));
        assert_eq!(ended.length, t(5000));
        assert_eq!(ended.markers.len(), 2);
        assert_eq!(w.add(t(7000), None).unwrap_err(), "no recording is running");
    }

    #[test]
    fn a_recording_without_markers_gives_nothing_and_the_next_recording_starts_its_own_count() {
        let mut w = MarkerWatch::default();
        w.observe(Some(t(0)), || rec("a.mkv"), t(0));
        // Stopped and started again between two looks: the first ends, the second is watched.
        assert_eq!(w.observe(Some(t(9000)), || rec("b.mkv"), t(9000)), None);
        assert_eq!(w.add(t(9500), None).unwrap().0.name, "Marker 1");
        let ended = w.observe(Some(t(20_000)), || rec("c.mkv"), t(20_000)).unwrap();
        assert_eq!(ended.recording, PathBuf::from("b.mkv"));
        assert_eq!(w.add(t(20_000), None).unwrap().0.name, "Marker 1");
    }

    #[test]
    fn a_recording_holds_at_most_the_marker_limit() {
        let mut w = MarkerWatch::default();
        w.observe(Some(t(0)), || rec("a.mkv"), t(0));
        for _ in 0..MAX_MARKERS {
            w.add(t(1), None).unwrap();
        }
        assert_eq!(w.add(t(1), None).unwrap_err(), format!("a recording holds at most {MAX_MARKERS} markers"));
    }

    #[test]
    fn the_timeline_holds_the_recording_and_each_marker_at_its_frame() {
        let ended = Ended {
            recording: PathBuf::from("/rec/Recording 2026-11-21 10-00-00.mkv"),
            rate: FrameRate::new(30000, 1001),
            length: t(10_000),
            written: None,
            opening: Tick(0),
            last_frame: None,
            markers: vec![Marker { at: t(1000), name: "Goal".into() }, Marker { at: t(60_000), name: "Late".into() }],
            failed: None,
        };
        let v = timeline(&ended);
        assert_eq!(v["OTIO_SCHEMA"], "Timeline.1");
        assert_eq!(v["name"], "Recording 2026-11-21 10-00-00");
        let stack = &v["tracks"];
        let kinds: Vec<&str> = stack["children"].as_array().unwrap().iter().map(|c| c["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["Video", "Audio"]);
        let clip = &stack["children"][0]["children"][0];
        let media = &clip["media_references"]["DEFAULT_MEDIA"];
        assert_eq!(media["target_url"], "Recording%202026-11-21%2010-00-00.mkv");
        // Frames 0 to 299 start in the first 10 s at 29.97 frames per second.
        assert_eq!(clip["source_range"]["duration"]["value"], 300.0);
        assert!((clip["source_range"]["duration"]["rate"].as_f64().unwrap() - 30000.0 / 1001.0).abs() < 1e-9);
        let markers = stack["markers"].as_array().unwrap();
        assert_eq!(markers[0]["name"], "Goal");
        assert_eq!(markers[0]["marked_range"]["start_time"]["value"], 29.0);
        assert_eq!(markers[0]["marked_range"]["duration"]["value"], 0.0);
        assert_eq!(markers[1]["marked_range"]["start_time"]["value"], 299.0, "a marker after the last frame is put on it");
    }

    #[test]
    fn a_recording_that_stops_between_frames_keeps_its_last_frame() {
        let mut ended = Ended {
            recording: PathBuf::from("a.mkv"),
            rate: FrameRate::new(30, 1),
            length: t(1010),
            written: None,
            opening: Tick(0),
            last_frame: None,
            markers: vec![Marker { at: t(1005), name: "End".into() }],
            failed: None,
        };
        let v = timeline(&ended);
        assert_eq!(v["tracks"]["children"][0]["children"][0]["source_range"]["duration"]["value"], 31.0);
        assert_eq!(v["tracks"]["markers"][0]["marked_range"]["start_time"]["value"], 30.0);
        // Stopped on a frame's start, the recording ends with the frame before it.
        ended.length = t(1000);
        assert_eq!(timeline(&ended)["tracks"]["children"][0]["children"][0]["source_range"]["duration"]["value"], 30.0);
    }

    #[test]
    fn the_clip_holds_the_frames_the_encoder_was_handed() {
        let mut ended = Ended {
            recording: PathBuf::from("a.mkv"),
            rate: FrameRate::new(30, 1),
            length: t(10_000),
            written: Some(written(240)),
            opening: Tick(0),
            last_frame: None,
            markers: vec![Marker { at: t(9000), name: "Late".into() }],
            failed: None,
        };
        // The encoder stalled 2 s before the stop: the file holds 240 frames, not 300.
        let v = timeline(&ended);
        let clip = &v["tracks"]["children"][0]["children"][0];
        assert_eq!(clip["source_range"]["duration"]["value"], 240.0);
        assert_eq!(clip["media_references"]["DEFAULT_MEDIA"]["available_range"]["duration"]["value"], 240.0);
        assert_eq!(v["tracks"]["markers"][0]["marked_range"]["start_time"]["value"], 239.0, "a marker past the file is put on its last frame");
        // A last frame placed past the stop is in the file too, and none still gives one frame.
        ended.written = Some(written(301));
        assert_eq!(timeline(&ended)["tracks"]["children"][0]["children"][0]["source_range"]["duration"]["value"], 301.0);
        ended.written = Some(written(0));
        assert_eq!(timeline(&ended)["tracks"]["children"][0]["children"][0]["source_range"]["duration"]["value"], 1.0);
    }

    #[test]
    fn a_copy_that_lost_the_stream_s_opening_puts_its_markers_on_the_file_s_frames() {
        let mut ended = Ended {
            recording: PathBuf::from("copy.mkv"),
            rate: FrameRate::new(30, 1),
            length: t(10_000),
            written: Some(written(300)),
            opening: t(2000),
            last_frame: None,
            markers: vec![Marker { at: t(5000), name: "Goal".into() }, Marker { at: t(1000), name: "Early".into() }],
            failed: None,
        };
        // The file starts 2 s into the stream: it holds the last 240 of the 300 frames, and a
        // marker 5 s into the stream is 3 s into the file.
        let v = timeline(&ended);
        let clip = &v["tracks"]["children"][0]["children"][0];
        assert_eq!(clip["source_range"]["duration"]["value"], 240.0);
        let markers = v["tracks"]["markers"].as_array().unwrap();
        assert_eq!(markers[0]["marked_range"]["start_time"]["value"], 90.0);
        assert_eq!(markers[1]["marked_range"]["start_time"]["value"], 0.0, "a marker before the file's start is put on its first frame");
        // Without encoder counters, the clip holds the frames from the file's start to the stop.
        ended.written = None;
        assert_eq!(timeline(&ended)["tracks"]["children"][0]["children"][0]["source_range"]["duration"]["value"], 240.0);
    }

    #[test]
    fn a_copy_whose_opening_is_a_whole_nanosecond_starts_at_that_frame() {
        let rate = FrameRate::new(30000, 1001);
        // Frame 61 starts at 2.0353666... s; the copy's first packet gives its time in whole
        // nanoseconds, just before that frame's start.
        let opening = tick_of(Duration::from_nanos(2_035_366_666));
        assert!(opening < rate.tick_of(61));
        let ended = Ended {
            recording: PathBuf::from("copy.mkv"),
            rate,
            length: t(20_000),
            written: Some(written(300)),
            opening,
            last_frame: None,
            markers: vec![Marker { at: rate.tick_of(100), name: "Goal".into() }],
            failed: None,
        };
        let v = timeline(&ended);
        assert_eq!(v["tracks"]["children"][0]["children"][0]["source_range"]["duration"]["value"], 239.0, "frames 61 to 299");
        assert_eq!(v["tracks"]["markers"][0]["marked_range"]["start_time"]["value"], 39.0, "frame 100 of the stream");
    }

    #[test]
    fn a_copy_that_lost_the_stream_s_last_packets_ends_its_clip_at_its_last_frame() {
        let rate = FrameRate::new(30000, 1001);
        let mut ended = Ended {
            recording: PathBuf::from("copy.mkv"),
            rate,
            length: t(20_000),
            written: Some(written(300)),
            opening: Tick(0),
            // The file's last picture is frame 250, at its time in whole nanoseconds.
            last_frame: Some(tick_of(Duration::from_nanos(8_341_666_666))),
            markers: vec![Marker { at: rate.tick_of(280), name: "Late".into() }],
            failed: None,
        };
        let v = timeline(&ended);
        let clip = &v["tracks"]["children"][0]["children"][0];
        assert_eq!(clip["source_range"]["duration"]["value"], 251.0);
        assert_eq!(clip["media_references"]["DEFAULT_MEDIA"]["available_range"]["duration"]["value"], 251.0);
        assert_eq!(v["tracks"]["markers"][0]["marked_range"]["start_time"]["value"], 250.0, "a marker past the file is put on its last frame");
        // With a late start as well, the clip runs from the file's first frame to its last.
        ended.opening = rate.tick_of(61);
        assert_eq!(timeline(&ended)["tracks"]["children"][0]["children"][0]["source_range"]["duration"]["value"], 190.0);
        // A file that holds every frame the encoder was handed keeps them all.
        ended.opening = Tick(0);
        ended.last_frame = Some(rate.tick_of(299));
        assert_eq!(timeline(&ended)["tracks"]["children"][0]["children"][0]["source_range"]["duration"]["value"], 300.0);
    }

    #[test]
    fn the_recording_keeps_the_encoder_counters_it_started_with() {
        let mut w = MarkerWatch::default();
        let counters = written(0);
        let target = Some((PathBuf::from("a.mkv"), FrameRate::new(30, 1), Some(counters.clone())));
        w.observe(Some(t(0)), || target, t(0));
        w.add(t(500), None).unwrap();
        counters.0.frames.store(15, Ordering::Relaxed);
        let ended = w.observe(None, || None, t(1000)).unwrap();
        assert_eq!(ended.written, Some(counters));
        assert_eq!(ended.written.unwrap().frames(), 15, "the count is read when the timeline is made, not when the recording stops");
    }

    fn names(s: &MarkerSession) -> Vec<String> {
        s.markers.iter().map(|m| m.name.clone()).collect()
    }

    fn add(s: &mut MarkerSession, now: Tick, name: Option<&str>, recording: Placed, audio_tracks: Placed) -> Result<Value, String> {
        let next = s.next(name.map(str::to_owned))?;
        s.push(next, now, Results { recording, stream_copy: Placed::Off, audio_tracks }, (None, None, None))
    }

    #[test]
    fn a_session_lasts_while_the_recording_or_a_take_runs() {
        let mut s = MarkerSession::default();
        let idle = "no recording, stream copy or take of audio tracks is running";
        assert_eq!(s.next(None).unwrap_err(), idle);
        // The recording starts at 1 s and is seen at 2 s: the session starts with the recording.
        s.observe(Some(t(1000)), false, t(2000));
        assert!(s.running());
        let v = add(&mut s, t(4000), None, Placed::Kept(3.0), Placed::Off).unwrap();
        assert_eq!((v["index"].clone(), v["name"].clone(), v["seconds"].clone()), (json!(1), json!("Marker 1"), json!(3.0)));
        // A take joins, the recording stops, and a new recording starts while the take runs:
        // all of it is one session.
        s.observe(Some(t(1000)), true, t(5000));
        s.observe(None, true, t(6000));
        s.observe(Some(t(7000)), true, t(7000));
        add(&mut s, t(8000), Some("Goal"), Placed::Kept(1.0), Placed::Kept(3.0)).unwrap();
        assert_eq!(names(&s), ["Marker 1", "Goal"]);
        // Both stop: the markers stay listed, and no marker can be added.
        s.observe(None, false, t(9000));
        assert!(!s.running());
        assert_eq!(s.json()["markers"].as_array().unwrap().len(), 2);
        assert_eq!(s.json()["running"], json!(false));
        assert_eq!(s.next(None).unwrap_err(), idle);
        // A take alone starts the next session at the moment it is seen, with a new count.
        s.observe(None, true, t(20_000));
        assert!(s.running() && names(&s).is_empty());
        let v = add(&mut s, t(21_500), None, Placed::Off, Placed::Kept(1.5)).unwrap();
        assert_eq!((v["name"].clone(), v["seconds"].clone()), (json!("Marker 1"), json!(1.5)));
    }

    #[test]
    fn a_take_handed_over_when_a_recording_starts_keeps_the_session() {
        let mut s = MarkerSession::default();
        // A stream's take runs; a recording starts and the take hands over to a new one, so
        // the recording and the take both differ from the last look.
        s.observe(None, true, t(0));
        add(&mut s, t(500), None, Placed::Off, Placed::Kept(0.5)).unwrap();
        s.observe(Some(t(9000)), true, t(9000));
        let v = add(&mut s, t(10_000), None, Placed::Kept(1.0), Placed::Kept(0.0)).unwrap();
        assert_eq!((v["index"].clone(), v["seconds"].clone()), (json!(2), json!(10.0)));
        assert_eq!(names(&s), ["Marker 1", "Marker 2"]);
    }

    #[test]
    fn an_output_that_does_not_keep_the_marker_is_reported_and_the_other_keeps_it() {
        let mut s = MarkerSession::default();
        s.observe(Some(t(0)), true, t(0));
        let full = "the marker was not kept: the take is ending or 1024 markers are waiting to be written";
        let v = add(&mut s, t(2000), Some("Goal"), Placed::Kept(2.0), Placed::Failed(full.into())).unwrap();
        assert_eq!(v["recording"], json!({"seconds": 2.0}));
        assert_eq!(v["audio_tracks"], json!({"error": full}));
        let v = add(&mut s, t(3000), None, Placed::Failed("a recording holds at most 10000 markers".into()), Placed::Kept(3.0)).unwrap();
        assert_eq!(v["name"], json!("Marker 2"));
        assert_eq!(v["recording"]["error"], json!("a recording holds at most 10000 markers"));
        // Neither keeps it: an error naming both, and the session's count does not move.
        let e = add(&mut s, t(4000), None, Placed::Failed("r".into()), Placed::Failed("a".into())).unwrap_err();
        assert_eq!(e, "the marker was not kept (the recording: r; the audio tracks: a)");
        let e = add(&mut s, t(4000), None, Placed::Off, Placed::Failed("a".into())).unwrap_err();
        assert_eq!(e, "the marker was not kept (the audio tracks: a)");
        let listed = s.json();
        let markers = listed["markers"].as_array().unwrap();
        assert_eq!(markers.len(), 2);
        assert_eq!(
            markers[0],
            json!({"index": 1, "name": "Goal", "seconds": 2.0, "recording": {"seconds": 2.0}, "stream_copy": null, "audio_tracks": null})
        );
        assert_eq!(markers[1]["audio_tracks"], json!({"seconds": 3.0}));
        assert_eq!(add(&mut s, t(5000), None, Placed::Kept(5.0), Placed::Off).unwrap()["index"], json!(3));
    }

    #[test]
    fn a_session_holds_at_most_the_marker_limit() {
        let mut s = MarkerSession::default();
        s.observe(None, true, t(0));
        for _ in 0..MAX_MARKERS {
            add(&mut s, t(1), None, Placed::Off, Placed::Kept(0.0)).unwrap();
        }
        assert_eq!(s.next(None).unwrap_err(), format!("a session holds at most {MAX_MARKERS} markers"));
    }

    #[test]
    fn a_rename_one_output_does_not_make_is_reported_and_the_other_keeps_the_new_name() {
        let mut s = MarkerSession::default();
        s.observe(Some(t(0)), true, t(0));
        add(&mut s, t(1000), Some("Goal"), Placed::Kept(1.0), Placed::Kept(1.0)).unwrap();
        assert_eq!(s.get(0).err(), Some("there is no marker 0: the session holds markers 1 to 1".into()));
        assert_eq!(s.get(2).err(), Some("there is no marker 2: the session holds markers 1 to 1".into()));
        let r = |recording, audio_tracks| Results { recording, stream_copy: Placed::Off, audio_tracks };
        let v = s.renamed(1, "Kickoff".into(), r(Placed::Kept(1.0), Placed::Failed("full".into()))).unwrap();
        assert_eq!(
            (v["name"].clone(), v["recording"].clone(), v["audio_tracks"].clone()),
            (json!("Kickoff"), json!({"seconds": 1.0}), json!({"error": "full"}))
        );
        let e = s.renamed(1, "Late".into(), r(Placed::Failed("r".into()), Placed::Failed("a".into()))).unwrap_err();
        assert_eq!(e, "the marker was not renamed (the recording: r; the audio tracks: a)");
        let e = s.renamed(1, "Late".into(), r(Placed::Off, Placed::Off)).unwrap_err();
        assert_eq!(e, "no recording, stream copy or take of audio tracks holds marker 1");
        assert_eq!(names(&s), ["Kickoff"], "a rename no output made keeps the name");
        assert_eq!(s.renamed(2, "Late".into(), r(Placed::Kept(1.0), Placed::Off)).unwrap_err(), "there is no marker 2");
    }

    #[test]
    fn a_marker_of_the_running_recording_is_renamed_in_its_list() {
        let mut w = MarkerWatch::default();
        w.observe(Some(t(0)), || rec("a.mkv"), t(0));
        let held = w.holds("Goal").unwrap();
        assert_eq!((held.recording.as_path(), held.index, held.copy), (Path::new("a.mkv"), 0, false));
        w.add(t(100), Some("Goal".into())).unwrap();
        assert_eq!(w.rename(Path::new("b.mkv"), 0, "Goal", "Kickoff"), None, "another recording");
        assert_eq!(w.rename(Path::new("a.mkv"), 0, "Goal", "Kickoff"), Some(Ok(())));
        assert_eq!(w.rename(Path::new("a.mkv"), 0, "Goal", "Again"), Some(Err("the recording holds no marker \"Goal\" in that place".into())));
        assert!(matches!(w.rename(Path::new("a.mkv"), 1, "Kickoff", "Again"), Some(Err(_))), "an index past the list");
        let ended = w.observe(None, || None, t(1000)).unwrap();
        assert_eq!(ended.markers[0].name, "Kickoff");
    }

    #[test]
    fn a_recording_marker_is_renamed_before_its_timeline_is_written_and_in_the_file_after() {
        let dir = tempfile::tempdir().unwrap();
        let recording = dir.path().join("take.mkv");
        let mut studio = crate::tests::headless();
        let hold = studio.outputs.hold_finishing(OutputKind::Recording, &recording.to_string_lossy(), false);
        studio.recording_markers.finishing.push(Ended {
            recording: recording.clone(),
            rate: FrameRate::new(30, 1),
            length: t(1000),
            written: None,
            opening: Tick(0),
            last_frame: None,
            markers: vec![Marker { at: t(100), name: "Goal".into() }, Marker { at: t(500), name: "Save".into() }],
            failed: None,
        });
        let save = InRecording { recording: recording.clone(), index: 1, name: "Save".into(), copy: false };
        // The file is being finished: the marker is renamed in the list the timeline is made from.
        studio.rename_recording_marker(&save, "Final save").unwrap();
        assert_eq!(studio.recording_markers.finishing[0].markers[1].name, "Final save");
        assert!(studio.rename_recording_marker(&save, "Again").is_err(), "the recording no longer has the old name");
        let save = InRecording { name: "Final save".into(), ..save };
        drop(hold);
        studio.outputs.finish_held(OutputKind::Recording);
        studio.sync_recording_markers();
        studio.report_marker_files(true);
        let timeline = dir.path().join("take.otio");
        let names = || {
            let v: Value = serde_json::from_slice(&std::fs::read(&timeline).unwrap()).unwrap();
            v["tracks"]["markers"].as_array().unwrap().iter().map(|m| m["name"].as_str().unwrap().to_string()).collect::<Vec<_>>()
        };
        assert_eq!(names(), ["Goal", "Final save"]);
        // Once the timeline is written, the file is written again with the new name.
        studio.rename_recording_marker(&save, "Last save").unwrap();
        assert_eq!(names(), ["Goal", "Last save"]);
        let before = std::fs::read(&timeline).unwrap();
        let e = studio.rename_recording_marker(&save, "Again").unwrap_err();
        assert!(e.ends_with("the timeline has no marker \"Final save\" in that place"), "{e}");
        assert_eq!(std::fs::read(&timeline).unwrap(), before, "the file is left as it is");
        // The same name is checked against the file and writes nothing.
        let last = InRecording { name: "Last save".into(), ..save };
        studio.rename_recording_marker(&last, "Last save").unwrap();
        assert_eq!(std::fs::read(&timeline).unwrap(), before, "the file is not written again");
        let edited = String::from_utf8(before.clone()).unwrap().replace("\"Last save\"", "\"Edited\"");
        std::fs::write(&timeline, &edited).unwrap();
        let e = studio.rename_recording_marker(&last, "Last save").unwrap_err();
        assert!(e.ends_with("the timeline has no marker \"Last save\" in that place"), "{e}");
        assert_eq!(std::fs::read_to_string(&timeline).unwrap(), edited, "the edited file is left as it is");
        std::fs::remove_file(&timeline).unwrap();
        assert!(studio.rename_recording_marker(&last, "Last save").is_err(), "a timeline that is gone");
        assert!(!timeline.exists());
        let other = InRecording { recording: dir.path().join("other.mkv"), index: 0, name: "Goal".into(), copy: false };
        assert_eq!(studio.rename_recording_marker(&other, "Kickoff").unwrap_err(), "the recording has no timeline file");
    }

    #[test]
    fn a_recording_marker_is_not_renamed_while_its_timeline_is_being_written() {
        let mut studio = crate::tests::headless();
        let recording = PathBuf::from("take.mkv");
        let (go, wait) = std::sync::mpsc::channel::<()>();
        let h = std::thread::spawn(move || {
            let _ = wait.recv();
            Err("stopped".to_string())
        });
        studio.recording_markers.writes.push((recording.clone(), h));
        let goal = InRecording { recording, index: 0, name: "Goal".into(), copy: false };
        let e = studio.rename_recording_marker(&goal, "Kickoff").unwrap_err();
        assert_eq!(e, "the timeline file is still being written; rename the marker once it is saved");
        assert_eq!(studio.rename_recording_marker(&goal, "Goal"), Ok(()), "the timeline being written has the name");
        go.send(()).unwrap();
        studio.report_marker_files(true);
    }

    #[test]
    fn a_timeline_file_that_cannot_be_read_is_not_written_again() {
        let dir = tempfile::tempdir().unwrap();
        let ended = Ended {
            recording: dir.path().join("take.mkv"),
            rate: FrameRate::new(30, 1),
            length: t(1000),
            written: None,
            opening: Tick(0),
            last_frame: None,
            markers: vec![Marker { at: t(0), name: "Goal".into() }, Marker { at: t(500), name: "Goal".into() }],
            failed: None,
        };
        let path = write_timeline(&ended).unwrap();
        let ranges = marker_ranges(&ended);
        assert_eq!(ranges.len(), 2);
        assert!(rename_in_timeline(&path, &ranges, 2, "Goal", "Kickoff").unwrap_err().contains("no marker"), "an index past the markers");
        rename_in_timeline(&path, &ranges, 1, "Goal", "Kickoff").unwrap();
        let files: Vec<String> = dir.path().read_dir().unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(files, ["take.otio"], "no temporary file is left");
        // The first marker was deleted in another app, and the next one, with the same name, moved
        // into its place: it is not taken for it.
        let mut v: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        v.pointer_mut("/tracks/markers").and_then(Value::as_array_mut).unwrap().remove(0);
        v.pointer_mut("/tracks/markers/0/name").map(|n| *n = json!("Goal")).unwrap();
        let shifted = serde_json::to_vec_pretty(&v).unwrap();
        std::fs::write(&path, &shifted).unwrap();
        assert!(rename_in_timeline(&path, &ranges, 0, "Goal", "Kickoff").unwrap_err().contains("no marker"));
        assert_eq!(std::fs::read(&path).unwrap(), shifted);
        // The same count of markers, with the marker at another time.
        let moved = Ended { markers: vec![Marker { at: t(500), name: "Goal".into() }, Marker { at: t(900), name: "Goal".into() }], ..ended };
        std::fs::write(&path, serde_json::to_vec_pretty(&timeline(&moved)).unwrap()).unwrap();
        assert!(rename_in_timeline(&path, &ranges, 0, "Goal", "Kickoff").unwrap_err().contains("no marker"));
        let text = dir.path().join("text.otio");
        std::fs::write(&text, b"not a timeline").unwrap();
        assert!(rename_in_timeline(&text, &ranges, 0, "Goal", "Kickoff").is_err());
        assert_eq!(std::fs::read(&text).unwrap(), b"not a timeline");
        let big = dir.path().join("big.otio");
        std::fs::File::create(&big).unwrap().set_len(MAX_TIMELINE_BYTES + 1).unwrap();
        let e = rename_in_timeline(&big, &ranges, 0, "Goal", "Kickoff").unwrap_err();
        assert!(e.ends_with(&format!("larger than {MAX_TIMELINE_BYTES} bytes")), "{e}");
        assert!(rename_in_timeline(dir.path(), &ranges, 0, "Goal", "Kickoff").unwrap_err().ends_with("not a file"));
        assert!(rename_in_timeline(&dir.path().join("none.otio"), &ranges, 0, "Goal", "Kickoff").is_err());
    }

    #[test]
    fn a_timeline_file_another_app_saved_while_a_marker_was_renamed_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let ended = Ended {
            recording: dir.path().join("take.mkv"),
            rate: FrameRate::new(30, 1),
            length: t(1000),
            written: None,
            opening: Tick(0),
            last_frame: None,
            markers: vec![Marker { at: t(0), name: "Goal".into() }],
            failed: None,
        };
        let path = write_timeline(&ended).unwrap();
        let ranges = marker_ranges(&ended);
        // Another app saves the file after the rename read and checked it.
        fn save(path: &Path) {
            let mut v: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            v["name"] = json!("Edited");
            std::fs::write(path, serde_json::to_vec_pretty(&v).unwrap()).unwrap();
        }
        BEFORE_TIMELINE_WRITE.with(|b| b.set(Some(save)));
        let e = rename_in_timeline(&path, &ranges, 0, "Goal", "Kickoff");
        BEFORE_TIMELINE_WRITE.with(|b| b.set(None));
        let e = e.unwrap_err();
        assert!(e.contains("changed after it was read"), "{e}");
        let v: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!((&v["name"], &v["tracks"]["markers"][0]["name"]), (&json!("Edited"), &json!("Goal")), "the other app's save is kept");
        let files: Vec<String> = dir.path().read_dir().unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(files, ["take.otio"], "no temporary file is left");
        // Without a save in between, the rename goes through.
        rename_in_timeline(&path, &ranges, 0, "Goal", "Kickoff").unwrap();
        let v: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!((&v["name"], &v["tracks"]["markers"][0]["name"]), (&json!("Edited"), &json!("Kickoff")));
    }

    #[test]
    fn a_name_outside_the_unreserved_characters_is_percent_encoded() {
        assert_eq!(url_name("a b%c#d?é.mkv"), "a%20b%25c%23d%3F%C3%A9.mkv");
        assert_eq!(url_name("Az09-._~"), "Az09-._~");
    }

    #[test]
    fn the_timeline_file_does_not_replace_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let ended = Ended {
            recording: dir.path().join("take.mkv"),
            rate: FrameRate::new(60, 1),
            length: t(0),
            written: None,
            opening: Tick(0),
            last_frame: None,
            markers: vec![Marker { at: t(0), name: "Start".into() }],
            failed: None,
        };
        let path = write_timeline(&ended).unwrap();
        assert_eq!(path, dir.path().join("take.otio"));
        let v: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(v["tracks"]["children"][0]["children"][0]["source_range"]["duration"]["value"], 1.0, "a recording is at least one frame");
        assert_eq!(v["tracks"]["markers"][0]["marked_range"]["start_time"]["value"], 0.0);
        let before = std::fs::read(&path).unwrap();
        let second = write_timeline(&ended).unwrap();
        assert_eq!(second, dir.path().join("take (2).otio"), "an existing file is not replaced");
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(write_timeline(&ended).unwrap(), dir.path().join("take (3).otio"));
    }

    #[test]
    fn the_timeline_waits_until_the_recording_file_is_finished() {
        let dir = tempfile::tempdir().unwrap();
        let recording = dir.path().join("take.mkv");
        let mut studio = crate::tests::headless();
        let hold = studio.outputs.hold_finishing(OutputKind::Recording, &recording.to_string_lossy(), false);
        studio.recording_markers.finishing.push(Ended {
            recording: recording.clone(),
            rate: FrameRate::new(30, 1),
            length: t(1000),
            written: None,
            opening: Tick(0),
            last_frame: None,
            markers: vec![Marker { at: t(500), name: "Half".into() }],
            failed: None,
        });
        studio.sync_recording_markers();
        assert!(studio.recording_markers.writes.is_empty(), "nothing is written while the recording is finishing");
        assert!(!dir.path().join("take.otio").exists());
        drop(hold);
        studio.outputs.finish_held(OutputKind::Recording);
        studio.sync_recording_markers();
        studio.report_marker_files(true);
        assert!(dir.path().join("take.otio").exists());
        assert!(studio.take_notices().iter().any(|n| n.text().starts_with("Markers saved to")));
    }

    #[test]
    fn a_recording_that_was_not_finished_gets_no_timeline() {
        let dir = tempfile::tempdir().unwrap();
        let recording = dir.path().join("take.mkv");
        let mut studio = crate::tests::headless();
        drop(studio.outputs.hold_finishing(OutputKind::Recording, &recording.to_string_lossy(), true));
        studio.outputs.finish_held(OutputKind::Recording);
        studio.recording_markers.finishing.push(Ended {
            recording: recording.clone(),
            rate: FrameRate::new(30, 1),
            length: t(1000),
            written: None,
            opening: Tick(0),
            last_frame: None,
            markers: vec![Marker { at: t(500), name: "Half".into() }],
            failed: None,
        });
        studio.sync_recording_markers();
        studio.report_marker_files(true);
        assert!(!dir.path().join("take.otio").exists());
        let notices = studio.take_notices();
        let texts: Vec<String> = notices.iter().map(|n| n.text()).collect();
        assert!(texts.iter().any(|t| t.ends_with("the recording was not finished")), "{texts:?}");
        assert!(!notices.iter().any(|n| n.text().starts_with("Markers saved to")));
    }

    #[test]
    fn a_marker_does_not_count_the_time_the_recording_left_out() {
        let mut w = MarkerWatch::default();
        let counters = written(400);
        w.observe(Some(t(0)), || Some((PathBuf::from("a.mkv"), FrameRate::new(30, 1), Some(counters.clone()))), t(0));
        assert_eq!(w.add(t(2000), None).unwrap().1, t(2000));
        // The computer was suspended, and the recording left out 3 s to 63 s. The markers come
        // before the encoder has reached the gap, so their replies still count the gap.
        assert_eq!(w.add(t(50_000), None).unwrap().1, t(50_000));
        assert_eq!(w.add(t(65_000), None).unwrap().1, t(65_000));
        counters.0.note_skipped(63_000_000_000, 60_000_000_000);
        let ended = w.observe(None, || None, t(70_000)).unwrap();
        // The timeline places them once the gap is known: a marker in the gap where the recording
        // goes on (3 s), and the one after it 5 s into the recording.
        let v = timeline(&ended);
        let markers = v["tracks"]["markers"].as_array().unwrap();
        let at: Vec<f64> = markers.iter().map(|m| m["marked_range"]["start_time"]["value"].as_f64().unwrap()).collect();
        assert_eq!(at, [60.0, 90.0, 150.0]);
    }

    /// A headless studio running a stream that started now with a copy at `copy`, whose encoder
    /// has handed over `frames` frames, and that ends with the copy `finished` or not.
    fn streaming_with_copy(copy: &Path, frames: u64, finished: bool) -> Studio {
        streaming_with_failing_copy(copy, frames, finished).0
    }

    /// [`streaming_with_copy`], with the copy's failure while the stream runs, for the test to set.
    fn streaming_with_failing_copy(copy: &Path, frames: u64, finished: bool) -> (Studio, Arc<std::sync::Mutex<Option<String>>>) {
        let mut studio = crate::tests::headless();
        let stats = EncoderStats::default();
        stats.frames.store(frames, Ordering::Relaxed);
        let ended = studiocraft_plugin::StreamCopyState { path: copy.to_path_buf(), finished, ..Default::default() };
        let now = studio.clock.now();
        let failure = crate::outputs::tests::stream_with_copy(&mut studio.outputs, now, Arc::new(stats), ended);
        (studio, failure)
    }

    #[test]
    fn a_streams_copy_that_fails_while_the_stream_runs_takes_no_more_markers() {
        let dir = tempfile::tempdir().unwrap();
        let copy = dir.path().join("Stream 2026-11-23 10-00-00.mkv");
        let (mut studio, failure) = streaming_with_failing_copy(&copy, 30, false);
        studio.sync_recording_markers();
        studio.add_recording_marker(Some("Goal".into())).unwrap();
        studio.add_marker(Some("Save".into())).unwrap();
        *failure.lock().unwrap() = Some("the disk is full".into());
        studio.sync_recording_markers();
        assert!(studio.can_add_recording_marker(), "the failure is seen once the outputs are polled");
        let events = studio.outputs.poll();
        studio.notices.extend(events.notices);
        studio.sync_recording_markers();
        assert!(!studio.can_add_recording_marker(), "a failed copy is not offered for markers");
        assert_eq!(studio.marker_list()["running"], json!(false), "the failed copy ends the session");
        let e = studio.rename_marker(1, "Kickoff".into()).unwrap_err().to_string();
        assert_eq!(e, "the marker was not renamed (the stream's copy: the stream's copy failed: the disk is full)");
        let e = studio.add_recording_marker(None).unwrap_err().to_string();
        assert_eq!(e, "the stream's copy failed: the disk is full");
        let e = studio.add_marker(None).unwrap_err().to_string();
        assert_eq!(e, NO_SESSION);
        stop_stream(&mut studio);
        assert!(!dir.path().join("Stream 2026-11-23 10-00-00.otio").exists());
        // The next stream's copy takes markers again.
        let next = dir.path().join("Stream 2026-11-23 11-00-00.mkv");
        let now = studio.clock.now();
        let ended = studiocraft_plugin::StreamCopyState { path: next, finished: true, ..Default::default() };
        let _ = crate::outputs::tests::stream_with_copy(&mut studio.outputs, now, Arc::new(EncoderStats::default()), ended);
        studio.sync_recording_markers();
        assert!(studio.can_add_recording_marker());
        assert_eq!(studio.add_recording_marker(None).unwrap()["stream_copy"]["name"], json!("Marker 1"));
        // A copy that failed before it was first seen gives its cause too.
        let (mut studio, failure) = streaming_with_failing_copy(&copy, 0, false);
        *failure.lock().unwrap() = Some("the folder is gone".into());
        assert_eq!(studio.add_recording_marker(None).unwrap_err().to_string(), "the stream's copy failed: the folder is gone");
        assert!(!studio.can_add_recording_marker());
    }

    #[test]
    fn a_marker_command_sees_a_copy_that_failed_since_the_last_poll() {
        let dir = tempfile::tempdir().unwrap();
        let copy = dir.path().join("Stream 2026-11-23 10-00-00.mkv");
        let (mut studio, failure) = streaming_with_failing_copy(&copy, 30, false);
        studio.sync_recording_markers();
        studio.add_marker(Some("Goal".into())).unwrap();
        *failure.lock().unwrap() = Some("the disk is full".into());
        // No poll yet: the command reads the copy's own state, and the session ends with the copy.
        assert_eq!(studio.marker_list()["running"], json!(false));
        assert_eq!(studio.add_recording_marker(None).unwrap_err().to_string(), "the stream's copy failed: the disk is full");
        // The failed copy ends the session, as it does once the outputs are polled.
        assert_eq!(studio.add_marker(None).unwrap_err().to_string(), NO_SESSION);
        assert!(!studio.can_add_recording_marker());
        stop_stream(&mut studio);
    }

    #[test]
    fn a_failed_copy_whose_stop_is_finishing_does_not_rename_its_marker() {
        let copy = PathBuf::from("/videos/Stream 2026-11-23 10-00-00.mkv");
        let mut w = MarkerWatch::of_stream_copy();
        assert!(w.observe(Some(t(0)), || Some((copy.clone(), FrameRate::new(30, 1), None)), t(0)).is_none());
        w.add(t(500), Some("Goal".into())).unwrap();
        w.fail("the disk is full");
        let ended = w.observe(None, || None, t(1000)).unwrap();
        assert_eq!(ended.failed.as_deref(), Some("the disk is full"), "the failure goes with the copy while its stop finishes");
        let mut studio = crate::tests::headless();
        studio.recording_markers.copies_finishing.push(ended);
        let goal = InRecording { recording: copy, index: 0, name: "Goal".into(), copy: true };
        assert_eq!(studio.rename_recording_marker(&goal, "Kickoff"), Err("the stream's copy failed: the disk is full".into()));
        assert_eq!(studio.recording_markers.copies_finishing[0].markers[0].name, "Goal");
    }

    #[test]
    fn a_copy_that_failed_just_before_its_stream_stopped_does_not_rename_its_marker() {
        let dir = tempfile::tempdir().unwrap();
        let copy = dir.path().join("Stream 2026-11-23 10-00-00.mkv");
        let mut studio = crate::tests::headless();
        let ended = studiocraft_plugin::StreamCopyState { path: copy.clone(), failure: Some("the disk is full".into()), ..Default::default() };
        let (release, gate) = std::sync::mpsc::channel::<()>();
        let now = studio.clock.now();
        let failure = crate::outputs::tests::stream_with_copy_gated(&mut studio.outputs, now, Arc::new(EncoderStats::default()), ended, Some(gate));
        studio.sync_recording_markers();
        studio.add_marker(Some("Goal".into())).unwrap();
        // The copy fails, and the stream starts stopping before the outputs are polled again.
        *failure.lock().unwrap() = Some("the disk is full".into());
        studio.stop_output(OutputKind::Streaming);
        let e = studio.rename_marker(1, "Kickoff".into()).unwrap_err().to_string();
        assert_eq!(e, "the marker was not renamed (the stream's copy: the stream's copy failed: the disk is full)");
        drop(release);
        stop_stream(&mut studio);
        assert!(!dir.path().join("Stream 2026-11-23 10-00-00.otio").exists());
    }

    #[test]
    fn a_healthy_copy_whose_stream_is_stopping_renames_its_marker() {
        let dir = tempfile::tempdir().unwrap();
        let copy = dir.path().join("Stream 2026-11-23 10-00-00.mkv");
        let mut studio = crate::tests::headless();
        let stats = EncoderStats::default();
        stats.frames.store(90, Ordering::Relaxed);
        // The copy's disk fell behind near the end: its file's last picture is 2 s into the stream.
        let ended =
            studiocraft_plugin::StreamCopyState { path: copy.clone(), finished: true, last_video_ns: Some(2_000_000_000), ..Default::default() };
        let (release, gate) = std::sync::mpsc::channel::<()>();
        let now = studio.clock.now();
        let _ = crate::outputs::tests::stream_with_copy_gated(&mut studio.outputs, now, Arc::new(stats), ended, Some(gate));
        studio.sync_recording_markers();
        studio.add_marker(Some("Goal".into())).unwrap();
        studio.stop_output(OutputKind::Streaming);
        studio.rename_marker(1, "Kickoff".into()).unwrap();
        drop(release);
        stop_stream(&mut studio);
        let timeline = dir.path().join("Stream 2026-11-23 10-00-00.otio");
        assert_eq!(timeline_names(&timeline), ["Kickoff"]);
        let v: Value = serde_json::from_slice(&std::fs::read(&timeline).unwrap()).unwrap();
        let fps = v["tracks"]["children"][0]["children"][0]["source_range"]["duration"]["rate"].as_f64().unwrap();
        let frames = v["tracks"]["children"][0]["children"][0]["source_range"]["duration"]["value"].as_f64().unwrap();
        assert_eq!(frames, (2.0 * fps).ceil() + 1.0, "the clip ends at the file's last picture, not the 90th frame");
    }

    /// Stop the stream of `studio` and wait for its timeline files.
    fn stop_stream(studio: &mut Studio) {
        studio.stop_output(OutputKind::Streaming);
        while studio.outputs.any_active() {
            let events = studio.outputs.poll();
            studio.notices.extend(events.notices);
            std::thread::yield_now();
        }
        studio.sync_recording_markers();
        studio.report_marker_files(true);
    }

    fn timeline_names(path: &Path) -> Vec<String> {
        let v: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        v["tracks"]["markers"].as_array().unwrap().iter().map(|m| m["name"].as_str().unwrap().to_string()).collect()
    }

    #[test]
    fn a_streams_copy_is_marked_and_gets_its_timeline_when_the_stream_stops() {
        let dir = tempfile::tempdir().unwrap();
        let copy = dir.path().join("Stream 2026-11-23 10-00-00.mkv");
        let mut studio = streaming_with_copy(&copy, 90, true);
        assert!(!studio.can_add_recording_marker(), "the copy is followed from the next look");
        studio.sync_recording_markers();
        assert!(studio.can_add_recording_marker());
        let v = studio.add_recording_marker(Some("Goal".into())).unwrap();
        assert_eq!((v["name"].clone(), v["recording"].clone(), v["stream_copy"]["name"].clone()), (json!("Goal"), Value::Null, json!("Goal")));
        assert!(v["seconds"].as_f64().is_some_and(|s| s >= 0.0), "{v}");
        assert_eq!(studio.add_recording_marker(None).unwrap()["name"], json!("Marker 2"), "numbered in the copy");
        // marker.add marks the copy too, and the session runs while the copy does.
        let v = studio.add_marker(Some("Kickoff".into())).unwrap();
        assert_eq!((v["index"].clone(), v["recording"].clone(), v["audio_tracks"].clone()), (json!(1), Value::Null, Value::Null));
        assert!(v["stream_copy"]["seconds"].as_f64().is_some(), "{v}");
        let listed = studio.marker_list();
        assert_eq!(listed["running"], json!(true));
        assert!(listed["markers"][0]["stream_copy"]["seconds"].as_f64().is_some(), "{listed}");
        // A rename while the stream runs goes into the timeline written at its end.
        let v = studio.rename_marker(1, "Opening".into()).unwrap();
        assert!(v["stream_copy"]["seconds"].as_f64().is_some(), "{v}");
        stop_stream(&mut studio);
        assert!(!studio.can_add_recording_marker());
        let timeline = dir.path().join("Stream 2026-11-23 10-00-00.otio");
        assert_eq!(timeline_names(&timeline), ["Goal", "Marker 2", "Opening"]);
        let v: Value = serde_json::from_slice(&std::fs::read(&timeline).unwrap()).unwrap();
        let clip = &v["tracks"]["children"][0]["children"][0];
        assert_eq!(clip["source_range"]["duration"]["value"], 90.0, "the clip holds the frames the stream's encoder was handed");
        assert_eq!(clip["media_references"]["DEFAULT_MEDIA"]["target_url"], "Stream%202026-11-23%2010-00-00.mkv");
        assert!(studio.take_notices().iter().any(|n| n.text() == format!("Markers saved to {}", timeline.display())));
        // The session has ended, and its marker is renamed in the written file.
        assert_eq!(studio.marker_list()["running"], json!(false));
        studio.rename_marker(1, "Start".into()).unwrap();
        assert_eq!(timeline_names(&timeline), ["Goal", "Marker 2", "Start"]);
        assert_eq!(studio.add_recording_marker(None).unwrap_err().to_string(), "neither a recording nor a stream's copy is running");
    }

    #[test]
    fn a_streams_copy_that_lost_the_stream_s_opening_gets_a_timeline_of_its_own_frames() {
        let dir = tempfile::tempdir().unwrap();
        let copy = dir.path().join("Stream 2026-11-23 10-00-00.mkv");
        let mut studio = crate::tests::headless();
        let stats = EncoderStats::default();
        stats.frames.store(90, Ordering::Relaxed);
        // The copy's disk fell behind before it got the stream's first packets: its file starts
        // 1 s into the stream.
        let ended = studiocraft_plugin::StreamCopyState { path: copy.clone(), finished: true, start_ns: 1_000_000_000, ..Default::default() };
        let now = studio.clock.now();
        let _ = crate::outputs::tests::stream_with_copy(&mut studio.outputs, now, Arc::new(stats), ended);
        studio.sync_recording_markers();
        studio.add_recording_marker(Some("Goal".into())).unwrap();
        stop_stream(&mut studio);
        let v: Value = serde_json::from_slice(&std::fs::read(dir.path().join("Stream 2026-11-23 10-00-00.otio")).unwrap()).unwrap();
        let fps = v["tracks"]["children"][0]["children"][0]["source_range"]["duration"]["rate"].as_f64().unwrap();
        let frames = v["tracks"]["children"][0]["children"][0]["source_range"]["duration"]["value"].as_f64().unwrap();
        assert_eq!(frames, 90.0 - fps.floor(), "the clip leaves out the frames before the file's start");
        assert_eq!(v["tracks"]["markers"][0]["marked_range"]["start_time"]["value"], 0.0, "a marker before the file's start is on its first frame");
    }

    #[test]
    fn a_marker_in_an_earlier_streams_copy_lists_its_time_in_that_copys_file() {
        let mut studio = crate::tests::headless();
        let copy = PathBuf::from("Stream 2026-11-23 10-00-00.mkv");
        let s = &mut studio.recording_markers.session;
        s.observe(Some(t(0)), false, t(0));
        let held = InRecording { recording: copy.clone(), index: 0, name: "Goal".into(), copy: true };
        let results = Results { recording: Placed::Off, stream_copy: Placed::Kept(1.5), audio_tracks: Placed::Off };
        // Set before the copy got its first packet.
        s.push(s.next(None).unwrap(), t(1500), results, (None, Some((held, 0.0)), None)).unwrap();
        // That stream's copy ends after a newer stream started; its file starts 1 s into the stream.
        let ended = studiocraft_plugin::StreamCopyState { path: copy, finished: true, start_ns: 1_000_000_000, ..Default::default() };
        crate::outputs::tests::earlier_copy_ended(&mut studio.outputs, ended);
        assert_eq!(studio.marker_list()["markers"][0]["stream_copy"], json!({"seconds": 0.5}));
    }

    #[test]
    fn a_marker_in_a_copy_gets_its_time_in_the_file_in_the_tick_that_sees_the_copy_end() {
        let dir = tempfile::tempdir().unwrap();
        let copy = dir.path().join("Stream 2026-11-23 10-00-00.mkv");
        let mut studio = crate::tests::headless();
        let s = &mut studio.recording_markers.session;
        s.observe(Some(t(0)), false, t(0));
        let held = InRecording { recording: copy.clone(), index: 0, name: "Goal".into(), copy: true };
        let results = Results { recording: Placed::Off, stream_copy: Placed::Kept(1.5), audio_tracks: Placed::Off };
        s.push(s.next(None).unwrap(), t(1500), results, (None, Some((held, 0.0)), None)).unwrap();
        let markers = vec![Marker { at: t(1500), name: "Goal".into() }];
        let finishing = Ended {
            recording: copy.clone(),
            rate: FrameRate::new(30, 1),
            length: t(3000),
            written: None,
            opening: Tick(0),
            last_frame: None,
            markers,
            failed: None,
        };
        studio.recording_markers.copies_finishing.push(finishing);
        let ended = studiocraft_plugin::StreamCopyState { path: copy, finished: true, start_ns: 1_000_000_000, ..Default::default() };
        crate::outputs::tests::earlier_copy_ended(&mut studio.outputs, ended);
        studio.sync_recording_markers();
        // Many more copies end before the next marker command: the first copy's start is forgotten.
        for n in 0..20 {
            let other = studiocraft_plugin::StreamCopyState { path: dir.path().join(format!("{n}.mkv")), finished: true, ..Default::default() };
            crate::outputs::tests::earlier_copy_ended(&mut studio.outputs, other);
        }
        let _ = studio.outputs.poll();
        assert_eq!(studio.marker_list()["markers"][0]["stream_copy"], json!({"seconds": 0.5}));
        studio.finish_recording_markers_blocking(t(3000));
    }

    #[test]
    fn a_streams_copy_that_was_not_finished_gets_no_timeline() {
        let dir = tempfile::tempdir().unwrap();
        let copy = dir.path().join("Stream 2026-11-23 10-00-00.mkv");
        let mut studio = streaming_with_copy(&copy, 30, false);
        studio.sync_recording_markers();
        studio.add_recording_marker(None).unwrap();
        stop_stream(&mut studio);
        assert!(!dir.path().join("Stream 2026-11-23 10-00-00.otio").exists());
        let texts: Vec<String> = studio.take_notices().iter().map(|n| n.text()).collect();
        assert!(texts.iter().any(|t| t.ends_with("the stream's copy was not finished")), "{texts:?}");
        // A renamed marker of that copy names the copy.
        let r = InRecording { recording: copy, index: 0, name: "Marker 1".into(), copy: true };
        assert_eq!(studio.rename_recording_marker(&r, "Goal").unwrap_err(), "the stream's copy has no timeline file");
    }

    #[test]
    fn a_stream_without_a_copy_is_looked_at_once_and_not_marked() {
        let mut w = MarkerWatch::of_stream_copy();
        let mut asked = 0;
        for now in [0, 100, 200] {
            assert_eq!(
                w.observe(
                    Some(t(0)),
                    || {
                        asked += 1;
                        None
                    },
                    t(now)
                ),
                None
            );
        }
        assert_eq!(asked, 1, "a stream without a copy is looked at once");
        assert_eq!(w.add(t(300), None).unwrap_err(), "no stream with a copy is running");
        // The next stream saves a copy.
        w.observe(Some(t(1000)), || rec("Stream a.mkv"), t(1000));
        assert_eq!(w.holds("Goal").map(|h| h.copy), Some(true));
        for _ in 0..MAX_MARKERS {
            w.add(t(1001), None).unwrap();
        }
        assert_eq!(w.add(t(1002), None).unwrap_err(), format!("a stream's copy holds at most {MAX_MARKERS} markers"));
        let e = w.rename(Path::new("Stream a.mkv"), 0, "Goal", "Kickoff").unwrap().unwrap_err();
        assert_eq!(e, "the stream's copy holds no marker \"Goal\" in that place");
    }

    #[test]
    fn record_marker_marks_the_recording_and_the_copy_and_reports_one_that_does_not_keep_it() {
        let (mut rec_watch, mut copy) = (MarkerWatch::default(), MarkerWatch::of_stream_copy());
        let idle = mark_recording_and_copy(&mut rec_watch, &mut copy, t(0), None).unwrap_err();
        assert_eq!(idle, "neither a recording nor a stream's copy is running");
        rec_watch.observe(Some(t(1000)), || rec("a.mkv"), t(1000));
        copy.observe(Some(t(0)), || rec("Stream a.mkv"), t(1000));
        copy.add(t(1500), None).unwrap();
        // Each output numbers its own markers; the reply's name and time are the recording's.
        let v = mark_recording_and_copy(&mut rec_watch, &mut copy, t(3000), None).unwrap();
        assert_eq!(
            v,
            json!({"name": "Marker 1", "seconds": 2.0, "recording": {"name": "Marker 1", "seconds": 2.0}, "stream_copy": {"name": "Marker 2", "seconds": 3.0}})
        );
        // The copy holds the most markers it can: the recording keeps the marker.
        for _ in 2..MAX_MARKERS {
            copy.add(t(3000), None).unwrap();
        }
        let full = format!("a stream's copy holds at most {MAX_MARKERS} markers");
        let v = mark_recording_and_copy(&mut rec_watch, &mut copy, t(4000), Some("Goal")).unwrap();
        assert_eq!((v["recording"]["name"].clone(), v["stream_copy"].clone()), (json!("Goal"), json!({"error": full})));
        for _ in 2..MAX_MARKERS {
            rec_watch.add(t(4000), None).unwrap();
        }
        let e = mark_recording_and_copy(&mut rec_watch, &mut copy, t(5000), None).unwrap_err();
        assert_eq!(e, format!("the marker was not kept (the recording: a recording holds at most {MAX_MARKERS} markers; the stream's copy: {full})"));
        // The copy alone: its name and time, and its own error when it does not keep the marker.
        rec_watch.observe(None, || None, t(5000));
        let e = mark_recording_and_copy(&mut rec_watch, &mut copy, t(5000), None).unwrap_err();
        assert_eq!(e, full);
        let mut copy = MarkerWatch::of_stream_copy();
        copy.observe(Some(t(0)), || rec("Stream b.mkv"), t(0));
        let v = mark_recording_and_copy(&mut rec_watch, &mut copy, t(500), Some("Late")).unwrap();
        assert_eq!((v["name"].clone(), v["seconds"].clone(), v["recording"].clone()), (json!("Late"), json!(0.5), Value::Null));
    }

    #[test]
    fn a_copy_that_lost_the_streams_opening_reports_each_markers_time_in_its_file() {
        let mut copy = MarkerWatch::of_stream_copy();
        copy.set_opening(Path::new("Stream a.mkv"), t(1000));
        copy.observe(Some(t(0)), || rec("Stream a.mkv"), t(0));
        // A new copy knows nothing yet of where its file starts.
        assert_eq!(copy.add(t(500), None).unwrap().1, t(500));
        // Another file's opening is not this copy's.
        copy.set_opening(Path::new("Stream b.mkv"), t(1000));
        assert_eq!(copy.add(t(600), None).unwrap().1, t(600));
        copy.set_opening(Path::new("Stream a.mkv"), t(1000));
        let (marker, placed) = copy.add(t(1500), None).unwrap();
        // The marker keeps its time in the stream, for the timeline, and reports its time in the file.
        assert_eq!((marker.at, placed), (t(1500), t(500)));
        // A marker from before the file's start is at its first frame.
        assert_eq!(copy.add(t(800), None).unwrap().1, t(0));
        let v = mark_recording_and_copy(&mut MarkerWatch::default(), &mut copy, t(2500), None).unwrap();
        assert_eq!(v["stream_copy"]["seconds"], json!(1.5));
    }

    #[test]
    fn a_session_starts_with_the_copy_and_reports_each_output() {
        let mut s = MarkerSession::default();
        // A stream with a copy started at 1 s and a recording at 3 s: the session starts with the copy.
        s.observe([Some(t(3000)), Some(t(1000))].into_iter().flatten().min(), false, t(4000));
        let results = Results { recording: Placed::Kept(2.0), stream_copy: Placed::Failed("full".into()), audio_tracks: Placed::Off };
        let v = s.push(s.next(None).unwrap(), t(5000), results, (None, None, None)).unwrap();
        assert_eq!(
            v,
            json!({"index": 1, "name": "Marker 1", "seconds": 4.0, "recording": {"seconds": 2.0}, "stream_copy": {"error": "full"}, "audio_tracks": null})
        );
        let results = Results { recording: Placed::Failed("r".into()), stream_copy: Placed::Failed("c".into()), audio_tracks: Placed::Off };
        let e = s.push(s.next(None).unwrap(), t(6000), results, (None, None, None)).unwrap_err();
        assert_eq!(e, "the marker was not kept (the recording: r; the stream's copy: c)");
        let results = Results { recording: Placed::Off, stream_copy: Placed::Kept(5.0), audio_tracks: Placed::Off };
        let v = s.renamed(1, "Goal".into(), results).unwrap();
        assert_eq!((v["name"].clone(), v["recording"].clone(), v["stream_copy"].clone()), (json!("Goal"), Value::Null, json!({"seconds": 5.0})));
        assert_eq!(s.json()["markers"][0]["stream_copy"], Value::Null, "the copy did not keep the marker when it was set");
    }

    #[test]
    fn a_sessions_markers_in_a_copy_move_into_its_file_once_its_start_is_known() {
        let mut s = MarkerSession::default();
        s.observe(Some(t(0)), false, t(0));
        let held = |index| InRecording { recording: PathBuf::from("Stream a.mkv"), index, name: "Marker".into(), copy: true };
        let copy = |seconds| Results { recording: Placed::Off, stream_copy: Placed::Kept(seconds), audio_tracks: Placed::Off };
        // Set before the copy got its first packet, and once its file was known to start at 1 s.
        s.push(s.next(None).unwrap(), t(500), copy(0.5), (None, Some((held(0), 0.0)), None)).unwrap();
        s.push(s.next(None).unwrap(), t(1500), copy(1.5), (None, Some((held(1), 0.0)), None)).unwrap();
        s.push(s.next(None).unwrap(), t(2500), copy(1.5), (None, Some((held(2), 1.0)), None)).unwrap();
        s.copy_opened(Path::new("Stream b.mkv"), 2.0);
        s.copy_opened(Path::new("Stream a.mkv"), 1.0);
        let seconds =
            |s: &MarkerSession| s.json()["markers"].as_array().unwrap().iter().map(|m| m["stream_copy"]["seconds"].clone()).collect::<Vec<_>>();
        assert_eq!(seconds(&s), [json!(0.0), json!(0.5), json!(1.5)]);
        // Looking again changes nothing.
        s.copy_opened(Path::new("Stream a.mkv"), 1.0);
        assert_eq!(seconds(&s), [json!(0.0), json!(0.5), json!(1.5)]);
    }
}
