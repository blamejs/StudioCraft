//! Markers set while a recording runs, written beside the recording for the video editor.
//!
//! `record.marker` marks the running recording at that moment. When a recording that has at least
//! one marker stops and its file is finished, `<recording name>.otio` is written beside it on a
//! `recording-markers` thread: an OpenTimelineIO timeline (https://opentimelineio.readthedocs.io)
//! with the recording as one clip on a video track and on an audio track, and each marker on the
//! timeline at its frame. FilmCraft's File › Import reads it. A file of that name that already
//! exists is not replaced: the timeline is written as `<recording name> (2).otio`, and so on.
//!
//! `marker.add` sets one marker on the running recording and the running take of audio tracks at
//! once, and keeps it in the session's list that `marker.list` returns. A session lasts from the
//! start of the first of those two outputs until both have stopped.
//!
//! `marker.rename` renames one of the session's markers in each output that holds it. While the
//! recording runs, or while its file is being finished, the recording's marker gets the new name
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

/// Markers one recording holds at most.
pub(crate) const MAX_MARKERS: usize = 10_000;

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
    pub markers: Vec<Marker>,
}

struct Watched {
    start: Tick,
    recording: PathBuf,
    rate: FrameRate,
    written: Option<Written>,
    markers: Vec<Marker>,
}

/// Which recording markers go to: [`MarkerWatch::observe`] follows the recording, and returns the
/// recording that stopped when it had markers.
#[derive(Default)]
pub(crate) struct MarkerWatch {
    recording: Option<Watched>,
}

impl MarkerWatch {
    /// Follow the recording that started at `start` (a recording is told apart by its start).
    /// `target` gives its file, frame rate and encoder counters, and is called only when a new
    /// recording is seen.
    pub(crate) fn observe(&mut self, start: Option<Tick>, target: impl FnOnce() -> Option<Target>, now: Tick) -> Option<Ended> {
        let ended = match (&self.recording, start) {
            (Some(w), Some(s)) if w.start == s => None,
            (Some(_), _) => self.recording.take(),
            (None, _) => None,
        };
        if self.recording.is_none()
            && let Some(start) = start
            && let Some((recording, rate, written)) = target()
        {
            self.recording = Some(Watched { start, recording, rate, written, markers: Vec::new() });
        }
        let w = ended.filter(|w| !w.markers.is_empty())?;
        Some(Ended {
            length: Tick(now.0.saturating_sub(w.start.0).max(0)),
            recording: w.recording,
            rate: w.rate,
            written: w.written,
            markers: w.markers,
        })
    }

    /// Mark the watched recording at `now`: named `name`, or "Marker N" for its Nth marker.
    /// Returns the marker, and where it lies in the recording as far as the encoder has written.
    pub(crate) fn add(&mut self, now: Tick, name: Option<String>) -> Result<(Marker, Tick), String> {
        let Some(w) = &mut self.recording else {
            return Err("no recording is running".into());
        };
        if w.markers.len() >= MAX_MARKERS {
            return Err(format!("a recording holds at most {MAX_MARKERS} markers"));
        }
        let name = name.unwrap_or_else(|| format!("Marker {}", w.markers.len() + 1));
        let marker = Marker { at: Tick(now.0.saturating_sub(w.start.0).max(0)), name };
        w.markers.push(marker.clone());
        let placed = in_recording(w.written.as_ref(), marker.at);
        Ok((marker, placed))
    }

    /// The watched recording's file and the number of markers it holds.
    fn holds(&self) -> Option<(&Path, usize)> {
        self.recording.as_ref().map(|w| (w.recording.as_path(), w.markers.len()))
    }

    /// Rename the marker at `index` of the watched recording, named `old`, to `name`. `None` when
    /// the watched recording is not `recording`.
    fn rename(&mut self, recording: &Path, index: usize, old: &str, name: &str) -> Option<Result<(), String>> {
        let w = self.recording.as_mut().filter(|w| w.recording == recording)?;
        Some(rename_listed(&mut w.markers, index, old, name))
    }
}

/// Rename the marker at `index` of `markers`, named `old`, to `name`. An error when the list
/// holds no marker named `old` there.
fn rename_listed(markers: &mut [Marker], index: usize, old: &str, name: &str) -> Result<(), String> {
    match markers.get_mut(index) {
        Some(m) if m.name == old => {
            m.name = name.to_string();
            Ok(())
        }
        _ => Err(format!("the recording holds no marker {old:?} in that place")),
    }
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

/// Where a recording holds a marker of `marker.add`: the recording's file, the marker's place in
/// the recording's markers, and the name the recording gave it last.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct InRecording {
    pub recording: PathBuf,
    pub index: usize,
    pub name: String,
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
    pub audio_tracks: Option<f64>,
    pub in_recording: Option<InRecording>,
    pub in_take: Option<InTake>,
}

impl SessionMarker {
    fn json(&self) -> Value {
        let kept = |s: Option<f64>| s.map_or(Value::Null, |s| json!({"seconds": s}));
        json!({
            "index": self.index,
            "name": self.name,
            "seconds": self.seconds,
            "recording": kept(self.recording),
            "audio_tracks": kept(self.audio_tracks),
        })
    }
}

/// The markers of a session: the time while the recording or a take of audio tracks runs, from
/// the start of the first of them until both have stopped. The last session's markers stay listed
/// until the next session starts.
#[derive(Default)]
pub(crate) struct MarkerSession {
    /// The session's start on the studio clock, while it runs.
    start: Option<Tick>,
    markers: Vec<SessionMarker>,
}

impl MarkerSession {
    /// Follow the recording that started at `recording` and whether a take runs. A session starts
    /// when one of them runs and no session runs, at the recording's start or at `now` for a take
    /// alone. It ends when a look finds neither running, so a take that hands over to the next one
    /// while a stream runs keeps the session. Runs after each runtime command and each tick.
    pub(crate) fn observe(&mut self, recording: Option<Tick>, take_runs: bool, now: Tick) {
        let runs = recording.is_some() || take_runs;
        if runs && self.start.is_none() {
            self.start = Some(recording.unwrap_or(now));
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
            return Err("neither a recording nor a take of audio tracks is running".into());
        }
        if self.markers.len() >= MAX_MARKERS {
            return Err(format!("a session holds at most {MAX_MARKERS} markers"));
        }
        let index = self.markers.len() + 1;
        Ok((index, name.unwrap_or_else(|| format!("Marker {index}"))))
    }

    /// Keep the marker [`MarkerSession::next`] numbered, set at `now`, with what each output did
    /// with it and where each output that kept it holds it. Returns the marker's number, name,
    /// time from the session's start and each output's result. When no output kept it, nothing is
    /// kept and the error names each cause.
    pub(crate) fn push(
        &mut self,
        (index, name): (usize, String),
        now: Tick,
        recording: Placed,
        audio_tracks: Placed,
        (in_recording, in_take): (Option<InRecording>, Option<InTake>),
    ) -> Result<Value, String> {
        let Some(start) = self.start else {
            return Err("neither a recording nor a take of audio tracks is running".into());
        };
        let kept = |p: &Placed| match p {
            Placed::Kept(s) => Some(*s),
            _ => None,
        };
        let marker = SessionMarker {
            index,
            name,
            seconds: Tick(now.0.saturating_sub(start.0).max(0)).seconds(),
            recording: kept(&recording),
            audio_tracks: kept(&audio_tracks),
            in_recording: in_recording.filter(|_| kept(&recording).is_some()),
            in_take: in_take.filter(|_| kept(&audio_tracks).is_some()),
        };
        if marker.recording.is_none() && marker.audio_tracks.is_none() {
            return Err(not_done("the marker was not kept", &recording, &audio_tracks)
                .unwrap_or_else(|| "neither a recording nor a take of audio tracks is running".into()));
        }
        let mut v = marker.json();
        v["recording"] = recording.json();
        v["audio_tracks"] = audio_tracks.json();
        self.markers.push(marker);
        Ok(v)
    }

    /// The session's marker numbered `index` (from 1).
    pub(crate) fn get(&self, index: usize) -> Result<&SessionMarker, String> {
        index.checked_sub(1).and_then(|i| self.markers.get(i)).ok_or_else(|| match self.markers.len() {
            0 => format!("there is no marker {index}: the session holds no markers"),
            n => format!("there is no marker {index}: the session holds markers 1 to {n}"),
        })
    }

    /// Note that the session's marker `index` was renamed to `name` in each output that
    /// `recording` and `audio_tracks` report as done. Returns the marker as `marker.list` lists
    /// it, with each output's result. When no output renamed it, the marker keeps its name and
    /// the error names each cause.
    pub(crate) fn renamed(&mut self, index: usize, name: String, recording: Placed, audio_tracks: Placed) -> Result<Value, String> {
        let Some(m) = index.checked_sub(1).and_then(|i| self.markers.get_mut(i)) else {
            return Err(format!("there is no marker {index}"));
        };
        let done = |p: &Placed| matches!(p, Placed::Kept(_));
        if !done(&recording) && !done(&audio_tracks) {
            return Err(not_done("the marker was not renamed", &recording, &audio_tracks)
                .unwrap_or_else(|| format!("neither the recording nor the audio tracks hold marker {index}")));
        }
        if let Some(r) = m.in_recording.as_mut().filter(|_| done(&recording)) {
            r.name.clone_from(&name);
        }
        if let Some(t) = m.in_take.as_mut().filter(|_| done(&audio_tracks)) {
            t.name.clone_from(&name);
        }
        m.name = name;
        let mut v = m.json();
        v["recording"] = recording.json();
        v["audio_tracks"] = audio_tracks.json();
        Ok(v)
    }

    /// The session's markers, oldest first, and whether the session runs.
    pub(crate) fn json(&self) -> Value {
        json!({"running": self.running(), "markers": self.markers.iter().map(SessionMarker::json).collect::<Vec<_>>()})
    }
}

/// `what`, followed by the cause each output that failed gave, or `None` when none failed.
fn not_done(what: &str, recording: &Placed, audio_tracks: &Placed) -> Option<String> {
    let causes: Vec<String> = [("the recording", recording), ("the audio tracks", audio_tracks)]
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
/// before the stop. A marker after the last frame is put on the last frame.
pub(crate) fn timeline(ended: &Ended) -> Value {
    let rate = ended.rate.sane();
    let file = ended.recording.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let title = ended.recording.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    // The frames that start before the stop.
    let by_clock = || if ended.length.0 > 0 { rate.frame_at(Tick(ended.length.0 - 1)).saturating_add(1) } else { 1 };
    // A recording is at least one frame.
    let frames = ended.written.as_ref().map_or_else(by_clock, Written::frames).max(1);
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
                "marked_range": time_range(rate.frame_at(in_recording(ended.written.as_ref(), m.at)).min(frames - 1), 0, rate),
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

/// The markers of the running recording, the recordings whose files are still being finished, the
/// timeline files being written with their recordings, the markers of the session, and the
/// timeline files written for the session's recordings.
#[derive(Default)]
pub(crate) struct RecordingMarkers {
    watch: MarkerWatch,
    session: MarkerSession,
    finishing: Vec<Ended>,
    writes: Vec<(PathBuf, JoinHandle<Result<(PathBuf, Vec<Value>), String>>)>,
    /// Each recording of the session whose timeline was written, the timeline file, and the
    /// `marked_range` of each marker as it was written.
    timelines: Vec<(PathBuf, PathBuf, Vec<Value>)>,
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
        let take_runs = self.audio_tracks_running();
        let ran = self.recording_markers.session.running();
        self.recording_markers.session.observe(start, take_runs, now);
        if !ran && self.recording_markers.session.running() {
            // The last session's markers are no longer listed, so their timelines are not renamed.
            self.recording_markers.timelines.clear();
        }
        self.write_finished_marker_files();
        self.report_marker_files(false);
    }

    /// Start writing the timelines of the recordings whose files are finished.
    fn write_finished_marker_files(&mut self) {
        let outputs = &self.outputs;
        let finished = |e: &Ended| !outputs.finishing(OutputKind::Recording, &e.recording.to_string_lossy());
        if !self.recording_markers.finishing.iter().any(finished) {
            return;
        }
        let (ready, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut self.recording_markers.finishing).into_iter().partition(finished);
        self.recording_markers.finishing = waiting;
        for ended in ready {
            self.write_marker_file(ended);
        }
    }

    /// Start writing the timeline of `ended` on a thread of its own, so a slow disk does not hold
    /// up a frame. A recording that did not finish its file cleanly gets no timeline.
    fn write_marker_file(&mut self, ended: Ended) {
        if self.outputs.take_unfinished(OutputKind::Recording, &ended.recording.to_string_lossy()) {
            let error = format!("{}: the recording was not finished", ended.recording.display());
            self.notices.push(Notice::new("The markers could not be saved: {error}", vec![("error", error)], true));
            return;
        }
        let recording = ended.recording.clone();
        match std::thread::Builder::new().name("recording-markers".into()).spawn(move || write_timeline(&ended).map(|p| (p, marker_ranges(&ended)))) {
            Ok(h) => self.recording_markers.writes.push((recording, h)),
            Err(e) => self.notices.push(Notice::new("The markers could not be saved: {error}", vec![("error", e.to_string())], true)),
        }
    }

    /// Mark the running recording now, for `record.marker`: the marker is named `name`, or
    /// "Marker N" for the recording's Nth marker. Returns its name and its time in the recording.
    pub(crate) fn add_recording_marker(&mut self, name: Option<String>) -> Result<Value, StudioError> {
        self.sync_recording_markers();
        let now = self.clock.now();
        let (marker, placed) = self.recording_markers.watch.add(now, name).map_err(StudioError::Output)?;
        Ok(json!({"name": marker.name, "seconds": placed.seconds()}))
    }

    /// Mark the running recording and the running take of audio tracks now with one marker, for
    /// `marker.add`: named `name`, or "Marker N" for the session's Nth marker. An output that
    /// does not keep it is reported, and the other keeps it.
    pub(crate) fn add_marker(&mut self, name: Option<String>) -> Result<Value, StudioError> {
        self.sync_recording_markers();
        let now = self.clock.now();
        let next = self.recording_markers.session.next(name).map_err(StudioError::Output)?;
        let mut in_recording = None;
        let recording = if self.outputs.start_of(OutputKind::Recording).is_some() {
            let held =
                self.recording_markers.watch.holds().map(|(path, index)| InRecording { recording: path.to_path_buf(), index, name: next.1.clone() });
            let added = self.recording_markers.watch.add(now, Some(next.1.clone())).map(|(_, placed)| placed.seconds());
            in_recording = held.filter(|_| added.is_ok());
            Placed::of(Some(added))
        } else {
            Placed::Off
        };
        let take = self.mark_audio_tracks(now, &next.1);
        let audio_tracks = Placed::of(take.as_ref().map(|r| r.as_ref().map(InTake::seconds).map_err(String::clone)));
        let in_take = take.and_then(Result::ok);
        self.recording_markers.session.push(next, now, recording, audio_tracks, (in_recording, in_take)).map_err(StudioError::Output)
    }

    /// Rename the session's marker `index` (from 1) to `name` in each output that holds it, for
    /// `marker.rename`. An output that does not rename it is reported, and the other keeps the
    /// new name. An output that already gives the marker that name is not written again.
    pub(crate) fn rename_marker(&mut self, index: usize, name: String) -> Result<Value, StudioError> {
        self.sync_recording_markers();
        let m = self.recording_markers.session.get(index).map_err(StudioError::Output)?.clone();
        let recording = match (&m.in_recording, m.recording) {
            (Some(r), Some(seconds)) => match self.rename_recording_marker(r, &name) {
                Ok(()) => Placed::Kept(seconds),
                Err(e) => Placed::Failed(e),
            },
            _ => Placed::Off,
        };
        let audio_tracks = match (&m.in_take, m.audio_tracks) {
            // A running take holds the name it was given; an ended take's file is checked.
            (Some(t), Some(seconds)) if t.name == name && !t.markers.ended() => Placed::Kept(seconds),
            (Some(t), Some(seconds)) => match t.markers.rename(t.number, &t.name, &name) {
                Ok(()) => Placed::Kept(seconds),
                Err(e) => Placed::Failed(e.to_string()),
            },
            _ => Placed::Off,
        };
        self.recording_markers.session.renamed(index, name, recording, audio_tracks).map_err(StudioError::Output)
    }

    /// Rename the recording's marker `r` to `name`: in the running recording's list or in the
    /// list of a recording whose file is being finished, or in its timeline file once that is
    /// written. A written timeline file is checked even for a rename to the name the marker has.
    fn rename_recording_marker(&mut self, r: &InRecording, name: &str) -> Result<(), String> {
        let rm = &mut self.recording_markers;
        if let Some(renamed) = rm.watch.rename(&r.recording, r.index, &r.name, name) {
            return renamed;
        }
        if let Some(e) = rm.finishing.iter_mut().find(|e| e.recording == r.recording) {
            return rename_listed(&mut e.markers, r.index, &r.name, name);
        }
        if rm.writes.iter().any(|(recording, _)| *recording == r.recording) {
            if r.name == name {
                return Ok(());
            }
            return Err("the timeline file is still being written; rename the marker once it is saved".into());
        }
        match rm.timelines.iter().find(|(recording, _, _)| *recording == r.recording) {
            Some((_, timeline, ranges)) => rename_in_timeline(timeline, ranges, r.index, &r.name, name),
            None => Err("the recording has no timeline file".into()),
        }
    }

    /// The markers `marker.add` set in the running session, or in the last one, for
    /// `marker.list`.
    pub(crate) fn marker_list(&mut self) -> Value {
        self.sync_recording_markers();
        self.recording_markers.session.json()
    }

    /// End the running recording's markers at `closed`, the time the app closed, write the
    /// timelines still to be written, and wait for every timeline file. Runs after the outputs
    /// have finished their files.
    pub(crate) fn finish_recording_markers_blocking(&mut self, closed: Tick) {
        if let Some(ended) = self.recording_markers.watch.observe(None, || None, closed) {
            self.recording_markers.finishing.push(ended);
        }
        for ended in std::mem::take(&mut self.recording_markers.finishing) {
            self.write_marker_file(ended);
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
            markers: vec![Marker { at: t(1000), name: "Goal".into() }, Marker { at: t(60_000), name: "Late".into() }],
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
            markers: vec![Marker { at: t(1005), name: "End".into() }],
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
            markers: vec![Marker { at: t(9000), name: "Late".into() }],
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
        s.push(next, now, recording, audio_tracks, (None, None))
    }

    #[test]
    fn a_session_lasts_while_the_recording_or_a_take_runs() {
        let mut s = MarkerSession::default();
        let idle = "neither a recording nor a take of audio tracks is running";
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
        assert_eq!(markers[0], json!({"index": 1, "name": "Goal", "seconds": 2.0, "recording": {"seconds": 2.0}, "audio_tracks": null}));
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
        let v = s.renamed(1, "Kickoff".into(), Placed::Kept(1.0), Placed::Failed("full".into())).unwrap();
        assert_eq!(
            (v["name"].clone(), v["recording"].clone(), v["audio_tracks"].clone()),
            (json!("Kickoff"), json!({"seconds": 1.0}), json!({"error": "full"}))
        );
        let e = s.renamed(1, "Late".into(), Placed::Failed("r".into()), Placed::Failed("a".into())).unwrap_err();
        assert_eq!(e, "the marker was not renamed (the recording: r; the audio tracks: a)");
        let e = s.renamed(1, "Late".into(), Placed::Off, Placed::Off).unwrap_err();
        assert_eq!(e, "neither the recording nor the audio tracks hold marker 1");
        assert_eq!(names(&s), ["Kickoff"], "a rename no output made keeps the name");
        assert_eq!(s.renamed(2, "Late".into(), Placed::Kept(1.0), Placed::Off).unwrap_err(), "there is no marker 2");
    }

    #[test]
    fn a_marker_of_the_running_recording_is_renamed_in_its_list() {
        let mut w = MarkerWatch::default();
        w.observe(Some(t(0)), || rec("a.mkv"), t(0));
        assert_eq!(w.holds(), Some((Path::new("a.mkv"), 0)));
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
            markers: vec![Marker { at: t(100), name: "Goal".into() }, Marker { at: t(500), name: "Save".into() }],
        });
        let save = InRecording { recording: recording.clone(), index: 1, name: "Save".into() };
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
        let other = InRecording { recording: dir.path().join("other.mkv"), index: 0, name: "Goal".into() };
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
        let goal = InRecording { recording, index: 0, name: "Goal".into() };
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
            markers: vec![Marker { at: t(0), name: "Goal".into() }, Marker { at: t(500), name: "Goal".into() }],
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
            markers: vec![Marker { at: t(0), name: "Goal".into() }],
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
            markers: vec![Marker { at: t(0), name: "Start".into() }],
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
            markers: vec![Marker { at: t(500), name: "Half".into() }],
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
            markers: vec![Marker { at: t(500), name: "Half".into() }],
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
}
