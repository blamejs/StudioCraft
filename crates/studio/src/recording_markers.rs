//! Markers set while a recording runs, written beside the recording for the video editor.
//!
//! `record.marker` marks the running recording at that moment. When a recording that has at least
//! one marker stops and its file is finished, `<recording name>.otio` is written beside it on a
//! `recording-markers` thread: an OpenTimelineIO timeline (https://opentimelineio.readthedocs.io)
//! with the recording as one clip on a video track and on an audio track, and each marker on the
//! timeline at its frame. FilmCraft's File › Import reads it. A file of that name that already
//! exists is not replaced: the timeline is written as `<recording name> (2).otio`, and so on.

use std::path::PathBuf;
use std::thread::JoinHandle;

use serde_json::{Value, json};
use studiocraft_media::{FrameRate, Tick};

use crate::outputs::OutputKind;
use crate::{Notice, Studio, StudioError};

/// Markers one recording holds at most.
pub(crate) const MAX_MARKERS: usize = 10_000;

/// A marker: its time from the recording's first frame, and its name.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Marker {
    pub at: Tick,
    pub name: String,
}

/// A recording that stopped with markers, waiting for its timeline file.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Ended {
    pub recording: PathBuf,
    pub rate: FrameRate,
    /// The recording's length, from its first frame to its stop.
    pub length: Tick,
    pub markers: Vec<Marker>,
}

struct Watched {
    start: Tick,
    recording: PathBuf,
    rate: FrameRate,
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
    /// `target` gives its file and frame rate and is called only when a new recording is seen.
    pub(crate) fn observe(&mut self, start: Option<Tick>, target: impl FnOnce() -> Option<(PathBuf, FrameRate)>, now: Tick) -> Option<Ended> {
        let ended = match (&self.recording, start) {
            (Some(w), Some(s)) if w.start == s => None,
            (Some(_), _) => self.recording.take(),
            (None, _) => None,
        };
        if self.recording.is_none()
            && let Some(start) = start
            && let Some((recording, rate)) = target()
        {
            self.recording = Some(Watched { start, recording, rate, markers: Vec::new() });
        }
        let w = ended.filter(|w| !w.markers.is_empty())?;
        Some(Ended { length: Tick(now.0.saturating_sub(w.start.0).max(0)), recording: w.recording, rate: w.rate, markers: w.markers })
    }

    /// Mark the watched recording at `now`: named `name`, or "Marker N" for its Nth marker.
    /// Returns the marker.
    pub(crate) fn add(&mut self, now: Tick, name: Option<String>) -> Result<Marker, String> {
        let Some(w) = &mut self.recording else {
            return Err("no recording is running".into());
        };
        if w.markers.len() >= MAX_MARKERS {
            return Err(format!("a recording holds at most {MAX_MARKERS} markers"));
        }
        let name = name.unwrap_or_else(|| format!("Marker {}", w.markers.len() + 1));
        let marker = Marker { at: Tick(now.0.saturating_sub(w.start.0).max(0)), name };
        w.markers.push(marker.clone());
        Ok(marker)
    }
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
/// that holds its time. A marker after the last frame is put on the last frame.
pub(crate) fn timeline(ended: &Ended) -> Value {
    let rate = ended.rate.sane();
    let file = ended.recording.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let title = ended.recording.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    // The frames that start before the stop; a recording is at least one frame.
    let frames = if ended.length.0 > 0 { rate.frame_at(Tick(ended.length.0 - 1)).saturating_add(1) } else { 1 };
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
                "marked_range": time_range(rate.frame_at(m.at).min(frames - 1), 0, rate),
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

/// The markers of the running recording, the recordings whose files are still being finished, and
/// the timeline files being written.
#[derive(Default)]
pub(crate) struct RecordingMarkers {
    watch: MarkerWatch,
    finishing: Vec<Ended>,
    writes: Vec<JoinHandle<Result<PathBuf, String>>>,
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
            Some((path, FrameRate::new(i64::from(video.fps_num), i64::from(video.fps_den))))
        };
        if let Some(ended) = self.recording_markers.watch.observe(start, target, now) {
            self.recording_markers.finishing.push(ended);
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
    /// up a frame.
    fn write_marker_file(&mut self, ended: Ended) {
        match std::thread::Builder::new().name("recording-markers".into()).spawn(move || write_timeline(&ended)) {
            Ok(h) => self.recording_markers.writes.push(h),
            Err(e) => self.notices.push(Notice::new("The markers could not be saved: {error}", vec![("error", e.to_string())], true)),
        }
    }

    /// Mark the running recording now, for `record.marker`: the marker is named `name`, or
    /// "Marker N" for the recording's Nth marker. Returns its name and its time in the recording.
    pub(crate) fn add_recording_marker(&mut self, name: Option<String>) -> Result<Value, StudioError> {
        self.sync_recording_markers();
        let now = self.clock.now();
        let marker = self.recording_markers.watch.add(now, name).map_err(StudioError::Output)?;
        Ok(json!({"name": marker.name, "seconds": marker.at.seconds()}))
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
        if !self.recording_markers.writes.iter().any(|h| wait || h.is_finished()) {
            return;
        }
        let (done, running): (Vec<_>, Vec<_>) = std::mem::take(&mut self.recording_markers.writes).into_iter().partition(|h| wait || h.is_finished());
        self.recording_markers.writes = running;
        for h in done {
            match h.join().unwrap_or_else(|_| Err("the marker file could not be written".into())) {
                Ok(path) => self.notices.push(Notice::new("Markers saved to {path}", vec![("path", path.display().to_string())], false)),
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

    fn rec(name: &str) -> Option<(PathBuf, FrameRate)> {
        Some((PathBuf::from(name), FrameRate::new(30, 1)))
    }

    #[test]
    fn markers_go_to_the_running_recording_and_come_back_when_it_stops() {
        let mut w = MarkerWatch::default();
        assert_eq!(w.add(t(0), None).unwrap_err(), "no recording is running");
        assert_eq!(w.observe(Some(t(1000)), || rec("a.mkv"), t(1000)), None);
        let m = w.add(t(3500), Some("Goal".into())).unwrap();
        assert_eq!(m, Marker { at: t(2500), name: "Goal".into() });
        assert_eq!(w.add(t(4000), None).unwrap().name, "Marker 2");
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
        assert_eq!(w.add(t(9500), None).unwrap().name, "Marker 1");
        let ended = w.observe(Some(t(20_000)), || rec("c.mkv"), t(20_000)).unwrap();
        assert_eq!(ended.recording, PathBuf::from("b.mkv"));
        assert_eq!(w.add(t(20_000), None).unwrap().name, "Marker 1");
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
        let hold = studio.outputs.hold_finishing(OutputKind::Recording, &recording.to_string_lossy());
        studio.recording_markers.finishing.push(Ended {
            recording: recording.clone(),
            rate: FrameRate::new(30, 1),
            length: t(1000),
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
}
