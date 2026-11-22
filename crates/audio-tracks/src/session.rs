//! The session file of a take: its tracks laid out for the audio editor, as a `.scraft` session
//! (craft-scraft-format version 1). The session is built from the take's journal alone and the
//! lengths of its files, so a take repaired after a crash gets the same session as one that
//! finished.
//!
//! The program mix's track comes first, then each source's track in the order the take made them.
//! Each file is one audio file of the session and one clip, placed at the file's first frame on
//! the timeline of the output the take followed: session sample 0 is that output's first sample.
//! A track's later parts (a file closed at the size limit) follow their earlier part sample for
//! sample. Each dropout is a selection on its track's marker lane, and each marker set during the
//! take is a marker, with the name of the journal's last rename of it. A session lists at most
//! [`MAX_LOCATIONS`] of them; its comments count the rest, which stay in the journal.
//! [`rename_marker`] renames a marker in a take's written session file.
//!
//! The session sits in the take's folder, `<name> Audio/<name>.scraft`, beside its `Audio Files`
//! folder, which the session's paths point into.

use std::path::{Path, PathBuf};

use craft_scraft_format::time::{MAX_POSITION, SampleRate, Samples};
use craft_scraft_format::{
    BitDepthSetting, ChannelFormat, Clip, FileFormat, MarkerId, MarkerKind, MemoryLocation, SESSION_EXTENSION, Session, Source, SourceId, TrackId,
    TrackKind,
};

use crate::journal::{self, Line};

/// The folder name the files of a take are in, inside its take folder.
pub const FILES_FOLDER: &str = "Audio Files";

/// The name of the marker each dropout gets, before the track's name.
pub const DROPOUT_NAME: &str = "Dropout";

/// Markers and dropouts a session lists at most: markers set during the take first, then
/// dropouts in the room left.
pub const MAX_LOCATIONS: usize = 10_000;

/// The session file of the take whose `Audio Files` folder is `dir`: `<name>.scraft` in the take
/// folder `<name> Audio`. `None` when `dir` is not an `Audio Files` folder inside a take folder.
pub fn path_for(dir: &Path) -> Option<PathBuf> {
    if dir.file_name()? != FILES_FOLDER {
        return None;
    }
    let take = dir.parent()?;
    let folder = take.file_name()?.to_str()?;
    let stem = folder.strip_suffix(" Audio").unwrap_or(folder);
    if stem.is_empty() {
        return None;
    }
    Some(take.join(format!("{stem}.{SESSION_EXTENSION}")))
}

/// The session's name: the session file's name without its extension.
fn name_of(path: &Path) -> String {
    path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

/// A position on the session's timeline, within the positions a session holds.
fn position(frames: u64) -> Samples {
    Samples::try_from(frames).unwrap_or(MAX_POSITION).min(MAX_POSITION)
}

/// One track of the session while it is built.
struct Built {
    journal_track: usize,
    name: String,
    program: bool,
    /// Each file in the order the journal names them: its name and first take frame.
    files: Vec<(String, u64)>,
}

/// The session for a take, from its journal `lines`, named `name`. `frames` gives the frames a
/// file holds, or `None` for a file that is not there (a part a cut removed); such a file is left
/// out, and so is an empty one.
pub fn build(name: &str, lines: &[Line], frames: impl Fn(&str) -> Option<u64>) -> Session {
    build_with(name, lines, 0, frames)
}

/// [`build`], for `lines` that leave out `left_out` marker and dropout lines of the journal: the
/// session's comments count them with those past [`MAX_LOCATIONS`].
pub fn build_with(name: &str, lines: &[Line], left_out: u64, frames: impl Fn(&str) -> Option<u64>) -> Session {
    let mut offset = 0u64;
    let mut rate = SampleRate::HZ_48000;
    let mut tracks: Vec<Built> = Vec::new();
    let mut dropouts: Vec<(usize, u64, u64)> = Vec::new();
    let mut markers: Vec<(u64, &str)> = Vec::new();
    // Where each numbered marker is in `markers`, for the rename lines.
    let mut numbered: std::collections::HashMap<u64, usize> = std::collections::HashMap::new();
    let mut unlisted = 0u64;
    let mut beyond = 0u64;
    let mut stopped: Option<&str> = None;
    for line in lines {
        match line {
            Line::Start { sample_rate, offset_frames, .. } => {
                offset = *offset_frames;
                rate = SampleRate::new(*sample_rate).unwrap_or(SampleRate::HZ_48000);
            }
            Line::File { track, name, file, first_frame, program, .. } => {
                let i = match tracks.iter().position(|t| t.journal_track == *track) {
                    Some(i) => i,
                    None => {
                        tracks.push(Built { journal_track: *track, name: name.clone(), program: *program, files: Vec::new() });
                        tracks.len() - 1
                    }
                };
                if let Some(built) = tracks.get_mut(i)
                    && !built.files.iter().any(|(f, _)| f == file)
                {
                    built.files.push((file.clone(), *first_frame));
                }
            }
            // Each list keeps at most the locations a session can list; the rest are counted.
            Line::Dropout { track, at, frames } if dropouts.len() < MAX_LOCATIONS => dropouts.push((*track, *at, *frames)),
            Line::Marker { at, name, number } if markers.len() < MAX_LOCATIONS => {
                if let Some(n) = number {
                    numbered.insert(*n, markers.len());
                }
                markers.push((*at, name.as_str()));
            }
            Line::Dropout { .. } | Line::Marker { .. } => beyond = beyond.saturating_add(1),
            // A marker past the limit is not listed, so its rename changes nothing.
            Line::Rename { number, name } => {
                if let Some(m) = numbered.get(number).and_then(|&i| markers.get_mut(i)) {
                    m.1 = name.as_str();
                }
            }
            Line::Unlisted { frames } => unlisted = unlisted.saturating_add(*frames),
            Line::Stop { reason } => stopped = Some(reason.as_str()),
            _ => {}
        }
    }
    // The program mix first; the sort keeps the take's order otherwise.
    tracks.sort_by_key(|t| !t.program);

    let mut s = Session::new(name, rate);
    s.bit_depth = BitDepthSetting::Float32;
    let mut ids: Vec<(usize, TrackId, String)> = Vec::with_capacity(tracks.len());
    for t in &tracks {
        let id = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some(&t.name));
        let shown = s.track(id).map(|t| t.name.clone()).unwrap_or_default();
        ids.push((t.journal_track, id, shown));
        let mut clips = Vec::with_capacity(t.files.len());
        for (file, first) in &t.files {
            // Only a name in the writer's format points into the take's folder.
            if !crate::names::is_track_file(file) {
                continue;
            }
            let Some(n) = frames(file).filter(|n| *n > 0) else { continue };
            let source = SourceId(s.alloc());
            let stem = Path::new(file).file_stem().map(|f| f.to_string_lossy().into_owned()).unwrap_or_else(|| file.clone());
            let start = offset.saturating_add(*first);
            s.sources.push(Source {
                id: source,
                name: stem.clone(),
                path: format!("{FILES_FOLDER}/{file}"),
                channels: 2,
                frames: n,
                sample_rate: rate.hz(),
                format: FileFormat::Wav,
                time_reference: start,
                unsaved: false,
            });
            clips.push(Clip::audio(s.new_clip_id(), stem, source, 0, position(start), position(n)));
        }
        if let Some(p) = s.track_mut(id).and_then(|t| t.playlist_mut()) {
            p.clips = clips;
            p.sort();
        }
    }

    // Markers set during the take come first; dropouts fill the room left.
    let listed_markers = markers.len().min(MAX_LOCATIONS);
    let listed_dropouts = dropouts.len().min(MAX_LOCATIONS - listed_markers);
    let not_listed = left_out.saturating_add(beyond).saturating_add((markers.len() - listed_markers + dropouts.len() - listed_dropouts) as u64);
    let mut locations: Vec<MemoryLocation> = Vec::with_capacity(listed_markers + listed_dropouts);
    let location = |s: &mut Session, name: String, kind: MarkerKind, start: u64, end: u64, track: Option<TrackId>| MemoryLocation {
        id: MarkerId(s.alloc()),
        number: 0,
        name,
        kind,
        start: position(start),
        end: position(end),
        comments: String::new(),
        color: None,
        ruler: 1,
        track,
    };
    for &(at, name) in markers.iter().take(listed_markers) {
        let start = offset.saturating_add(at);
        locations.push(location(&mut s, name.to_string(), MarkerKind::Marker, start, start, None));
    }
    for &(track, at, n) in dropouts.iter().take(listed_dropouts) {
        let Some((_, id, shown)) = ids.iter().find(|(j, _, _)| *j == track) else { continue };
        let start = offset.saturating_add(at);
        let (id, label) = (*id, format!("{DROPOUT_NAME}: {shown}"));
        locations.push(location(&mut s, label, MarkerKind::Selection, start, start.saturating_add(n), Some(id)));
    }
    // One sort for all of them, then numbers in timeline order.
    locations.sort_by_key(|m| (m.start, m.id.0));
    for (n, m) in locations.iter_mut().enumerate() {
        m.number = u32::try_from(n + 1).unwrap_or(u32::MAX);
    }
    s.markers = locations;

    let mut notes = Vec::new();
    if let Some(reason) = stopped {
        notes.push(format!("The audio tracks stopped early: {reason}."));
    }
    if unlisted > 0 {
        notes.push(format!("{:.3} s of lost audio have no dropout marker.", unlisted as f64 / f64::from(rate.hz())));
    }
    if not_listed > 0 {
        notes.push(format!("{not_listed} more markers and dropouts are in the take's journal."));
    }
    s.comments = notes.join("\n");
    // The format's own limits on names and comments, applied as a read would.
    s.sanitize();
    s
}

/// The frames the file `name` in `dir` holds, from its length: `None` when it is not a file
/// there.
pub fn file_frames(dir: &Path, name: &str) -> Option<u64> {
    let meta = std::fs::symlink_metadata(dir.join(name)).ok().filter(|m| m.is_file())?;
    let head = crate::wav::header_len().ok()?;
    Some(meta.len().saturating_sub(head) / crate::wav::FRAME_BYTES)
}

/// Write `session` to `path` in one step: a reader sees the old file or the new one, never part
/// of one.
pub fn write(path: &Path, session: &Session) -> std::io::Result<()> {
    let text = craft_scraft_format::write_session_named(session, &path.display().to_string(), craft_scraft_format::MAX_SESSION_BYTES)
        .map_err(std::io::Error::other)?;
    craft_atomic::atomic_write(path, text.as_bytes())
}

/// The session of the take whose `Audio Files` folder is `dir`, built from `lines` and written
/// beside the folder. Its path. `left_out` is as for [`build_with`].
pub fn write_from(dir: &Path, lines: &[Line], left_out: u64, frames: impl Fn(&str) -> Option<u64>) -> std::io::Result<PathBuf> {
    let path = path_for(dir).ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "the folder is not a take's Audio Files folder"))?;
    write(&path, &build_with(&name_of(&path), lines, left_out, frames))?;
    Ok(path)
}

/// The session of the take whose `Audio Files` folder is `dir`, from its journal and its files'
/// lengths, written beside the folder. Its path. For a take whose threads have ended.
pub fn write_take(dir: &Path) -> std::io::Result<PathBuf> {
    let lines = journal::read(dir)?;
    write_from(dir, &lines, 0, |f| file_frames(dir, f))
}

/// Where the session built from `lines` lists the take's marker `number`: its sample, and how many
/// of the markers listed before it have the same sample. `None` when the session does not list it.
fn listed_place(lines: &[Line], number: u64) -> Option<(u64, usize)> {
    let mut offset = 0u64;
    let mut listed: Vec<(u64, Option<u64>)> = Vec::new();
    for line in lines {
        match line {
            Line::Start { offset_frames, .. } => offset = *offset_frames,
            Line::Marker { at, number, .. } if listed.len() < MAX_LOCATIONS => listed.push((*at, *number)),
            _ => {}
        }
    }
    let i = listed.iter().position(|(_, n)| *n == Some(number))?;
    let sample = |at: u64| position(offset.saturating_add(at));
    let at = listed.get(i)?.0;
    let before = listed.iter().take(i).filter(|(a, _)| sample(*a) == sample(at)).count();
    Some((offset.saturating_add(at), before))
}

/// Rename the take's marker `number`, now named `old`, to `name` in the take's session file
/// `path`, from the take's journal in its `Audio Files` folder `dir`. The session lists markers at
/// the same sample in the order the journal holds them, so the marker is the one in that place
/// among them; it must still be named `old`. See [`rename_marker`].
pub fn rename_take_marker(dir: &Path, path: &Path, number: u64, old: &str, name: &str) -> std::io::Result<()> {
    let lines = journal::read(dir)?;
    let Some((at, before)) = listed_place(&lines, number) else {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{}: the session does not list the marker", path.display())));
    };
    rename_marker(path, at, before, old, name)
}

/// Rename a marker of the session file `path` to `name`, and write the file again in one step:
/// the marker at sample `at` with `before` markers at that sample ahead of it, which must be
/// named `old`. The file must be a file, not a link, of at most
/// [`craft_scraft_format::MAX_SESSION_BYTES`]; its size is checked before it is read. A session
/// that does not have that marker with that name, as after the marker was moved or renamed in the
/// audio editor, is an error and the file is left as it is.
pub fn rename_marker(path: &Path, at: u64, before: usize, old: &str, name: &str) -> std::io::Result<()> {
    use std::io::Read;
    let shown = path.display().to_string();
    let invalid = |e: String| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{shown}: {e}"));
    let limit = craft_scraft_format::MAX_SESSION_BYTES;
    let file = crate::open::no_follow(std::fs::OpenOptions::new().read(true), path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(invalid("not a file".into()));
    }
    if meta.len() > limit {
        return Err(invalid(format!("larger than {limit} bytes")));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(meta.len()).unwrap_or(0));
    // The file can grow between the size check and the read; the read stops one byte past it.
    file.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
    // The error names the file.
    let text = craft_scraft_format::read_session_data(&shown, bytes, limit).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    let mut session = Session::from_json(&text).map_err(|e| invalid(e.to_string()))?;
    let place = position(at);
    let marker = session.markers.iter_mut().filter(|m| m.kind == MarkerKind::Marker && m.track.is_none() && m.start == place).nth(before);
    let Some(marker) = marker.filter(|m| m.name == old) else {
        return Err(invalid(format!("the session no longer lists the marker {old:?} at its place")));
    };
    marker.name = name.to_string();
    session.sanitize();
    write(path, &session)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(track: usize, name: &str, file: &str, segment: u32, first_frame: u64, program: bool) -> Line {
        Line::File { track, name: name.into(), file: file.into(), segment, first_frame, program }
    }

    fn start(offset_frames: u64) -> Line {
        Line::Start {
            version: 1,
            sample_rate: 48_000,
            channels: 2,
            anchor: "recording".into(),
            origin_tick: Some(0),
            offset_frames,
            started_unix_ms: None,
            session: true,
        }
    }

    #[test]
    fn the_session_goes_beside_the_audio_files_folder_and_is_named_for_the_take() {
        let take = Path::new("Videos").join("2026-11-18 20-00-00 Audio");
        assert_eq!(path_for(&take.join(FILES_FOLDER)), Some(take.join("2026-11-18 20-00-00.scraft")));
        assert_eq!(
            path_for(&Path::new("Videos").join("Show (2) Audio").join(FILES_FOLDER)),
            Some(Path::new("Videos").join("Show (2) Audio").join("Show (2).scraft"))
        );
        assert_eq!(path_for(&take), None, "only an Audio Files folder has a session");
        assert_eq!(path_for(Path::new(FILES_FOLDER)), None, "an Audio Files folder needs a take folder");
    }

    #[test]
    fn parts_of_a_track_follow_each_other_sample_for_sample() {
        let lines = [
            start(1_000),
            file(1, "Mic", "Mic.wav", 1, 0, false),
            file(0, "00 Program", "00 Program.wav", 1, 0, true),
            file(1, "Mic", "Mic part 2.wav", 2, 500_000, false),
            file(1, "Mic", "Mic part 3.wav", 3, 1_000_000, false),
            file(2, "Late", "Late.wav", 1, 77_777, false),
        ];
        let lens = |f: &str| match f {
            "Mic.wav" | "Mic part 2.wav" => Some(500_000),
            "Mic part 3.wav" => Some(123),
            "00 Program.wav" => Some(1_000_123),
            "Late.wav" => Some(10),
            _ => None,
        };
        let s = build("Show", &lines, lens);
        let names: Vec<&str> = s.tracks.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["00 Program", "Mic", "Late"], "the program mix first, then the take's order");
        let mic = &s.tracks[1];
        let clips: Vec<(Samples, Samples)> = mic.clips().iter().map(|c| (c.start, c.length)).collect();
        assert_eq!(clips, [(1_000, 500_000), (501_000, 500_000), (1_001_000, 123)]);
        for pair in mic.clips().windows(2) {
            assert_eq!(pair[0].end(), pair[1].start, "each part starts where the one before it ends");
        }
        assert_eq!(s.tracks[2].clips()[0].start, 1_000 + 77_777, "a late source starts at its first frame");
        let paths: Vec<&str> = s.sources.iter().map(|x| x.path.as_str()).collect();
        assert!(paths.contains(&"Audio Files/Mic part 2.wav"), "{paths:?}");
        assert!(s.sources.iter().all(|x| x.channels == 2 && x.sample_rate == 48_000 && x.format == FileFormat::Wav));
        assert_eq!(s.sources.iter().find(|x| x.name == "Late").map(|x| x.time_reference), Some(78_777));
        assert_eq!(s.bit_depth, BitDepthSetting::Float32);
    }

    #[test]
    fn a_dropout_is_marked_at_its_exact_sample_on_its_track() {
        let lines = [
            start(48_000),
            file(0, "00 Program", "00 Program.wav", 1, 0, true),
            file(1, "Desk", "Desk.wav", 1, 0, false),
            Line::Dropout { track: 1, at: 12_345, frames: 1_024 },
            Line::Marker { at: 96_000, name: "Goal".into(), number: None },
            Line::Marker { at: 5, name: "Start".into(), number: None },
            Line::Unlisted { frames: 24_000 },
        ];
        let s = build("Show", &lines, |_| Some(200_000));
        let desk = s.tracks.iter().find(|t| t.name == "Desk").map(|t| t.id);
        let got: Vec<(u32, &str, MarkerKind, Samples, Samples, Option<TrackId>)> =
            s.markers.iter().map(|m| (m.number, m.name.as_str(), m.kind, m.start, m.end, m.track)).collect();
        assert_eq!(
            got,
            [
                (1, "Start", MarkerKind::Marker, 48_005, 48_005, None),
                (2, "Dropout: Desk", MarkerKind::Selection, 60_345, 61_369, desk),
                (3, "Goal", MarkerKind::Marker, 144_000, 144_000, None),
            ]
        );
        assert!(s.comments.contains("0.500 s of lost audio"), "{}", s.comments);
    }

    #[test]
    fn a_session_lists_markers_first_and_counts_what_it_leaves_out() {
        let mut lines = vec![start(0), file(0, "Mic", "Mic.wav", 1, 0, false)];
        lines.extend((0..MAX_LOCATIONS as u64 - 2).map(|n| Line::Marker { at: n, name: format!("M{n}"), number: None }));
        lines.extend((0..5).map(|n| Line::Dropout { track: 0, at: n * 10, frames: 1 }));
        let s = build_with("Show", &lines, 7, |_| Some(1_000));
        assert_eq!(s.markers.len(), MAX_LOCATIONS);
        assert_eq!(s.markers.iter().filter(|m| m.kind == MarkerKind::Marker).count(), MAX_LOCATIONS - 2, "every marker is listed");
        assert!(s.comments.contains("10 more markers and dropouts"), "3 dropouts past the limit and the 7 left out: {}", s.comments);
        // A track name at the format's limit still gives a dropout name within it.
        let long = "x".repeat(craft_scraft_format::MAX_NAME_CHARS);
        let lines = [start(0), file(0, &long, "x.wav", 1, 0, false), Line::Dropout { track: 0, at: 0, frames: 1 }];
        let s = build("Show", &lines, |_| Some(10));
        assert!(s.markers.iter().all(|m| m.name.chars().count() <= craft_scraft_format::MAX_NAME_CHARS));
    }

    #[test]
    fn locations_past_the_limit_in_a_long_journal_are_counted() {
        let mut lines = vec![start(0), file(0, "Mic", "Mic.wav", 1, 0, false)];
        lines.extend((0..5).map(|n| Line::Dropout { track: 0, at: n, frames: 1 }));
        lines.extend((0..MAX_LOCATIONS as u64 + 3).map(|n| Line::Marker { at: n, name: format!("M{n}"), number: None }));
        lines.extend((0..MAX_LOCATIONS as u64 + 2).map(|n| Line::Dropout { track: 0, at: n, frames: 1 }));
        let s = build("Show", &lines, |_| Some(1_000));
        assert_eq!(s.markers.len(), MAX_LOCATIONS);
        assert!(s.markers.iter().all(|m| m.kind == MarkerKind::Marker), "markers take the room first");
        let more = MAX_LOCATIONS + 10;
        assert!(s.comments.contains(&format!("{more} more markers and dropouts")), "3 markers and every dropout: {}", s.comments);
    }

    #[test]
    fn a_marker_takes_the_name_of_its_last_rename() {
        let lines = [
            start(1_000),
            file(0, "Mic", "Mic.wav", 1, 0, false),
            Line::Marker { at: 10, name: "Goal".into(), number: Some(1) },
            Line::Rename { number: 1, name: "Kickoff".into() },
            Line::Marker { at: 20, name: "Save".into(), number: Some(2) },
            Line::Rename { number: 1, name: "Opening goal".into() },
            // A rename of a marker the journal does not hold changes nothing.
            Line::Rename { number: 9, name: "Nothing".into() },
        ];
        let s = build("Show", &lines, |_| Some(1_000));
        let got: Vec<(&str, Samples)> = s.markers.iter().map(|m| (m.name.as_str(), m.start)).collect();
        assert_eq!(got, [("Opening goal", 1_010), ("Save", 1_020)]);
    }

    #[test]
    fn a_written_session_has_its_marker_renamed_in_one_step() {
        let dir = std::env::temp_dir().join(format!("studiocraft-session-rename-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let files = dir.join("Show Audio").join(FILES_FOLDER);
        std::fs::create_dir_all(&files).unwrap();
        let lines = [
            start(1_000),
            file(0, "Mic", "Mic.wav", 1, 0, false),
            Line::Dropout { track: 0, at: 10, frames: 5 },
            Line::Marker { at: 10, name: "Goal".into(), number: Some(1) },
            Line::Marker { at: 10, name: "Goal".into(), number: Some(2) },
        ];
        let path = write_from(&files, &lines, 0, |_| Some(1_000)).unwrap();
        let mut j = journal::Journal::create(&files).unwrap();
        for line in &lines {
            j.write(line).unwrap();
        }
        j.sync().unwrap();
        drop(j);
        let names = || {
            let back = Session::from_json(&std::fs::read_to_string(&path).unwrap()).unwrap();
            back.markers.iter().map(|m| m.name.clone()).collect::<Vec<_>>()
        };
        // Two markers with the same place and name: the one asked for is renamed.
        rename_take_marker(&files, &path, 2, "Goal", "Kickoff").unwrap();
        assert_eq!(names(), ["Goal", "Kickoff", "Dropout: Mic"]);
        rename_take_marker(&files, &path, 1, "Goal", "Save").unwrap();
        assert_eq!(names(), ["Save", "Kickoff", "Dropout: Mic"]);
        // The marker no longer has the old name, and the file stays as it is.
        let before = std::fs::read(&path).unwrap();
        let e = rename_take_marker(&files, &path, 1, "Goal", "Again").unwrap_err();
        assert!(e.to_string().contains("no longer lists the marker"), "{e}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let e = rename_take_marker(&files, &path, 9, "Goal", "Again").unwrap_err();
        assert!(e.to_string().contains("does not list the marker"), "{e}");
        assert!(rename_marker(&path, 999, 0, "Save", "Again").is_err(), "another place");
        assert!(rename_marker(&path, 1_010, 2, "Dropout: Mic", "Again").is_err(), "a dropout is not a marker");
        assert_eq!(std::fs::read(&path).unwrap(), before);
        // A file past the size limit is refused before it is read.
        let big = dir.join("big.scraft");
        std::fs::File::create(&big).unwrap().set_len(craft_scraft_format::MAX_SESSION_BYTES + 1).unwrap();
        assert!(rename_marker(&big, 0, 0, "a", "b").unwrap_err().to_string().contains("larger than"));
        assert!(rename_marker(&dir.join("none.scraft"), 0, 0, "a", "b").is_err(), "a missing file");
        let not_session = dir.join("text.scraft");
        std::fs::write(&not_session, b"not a session").unwrap();
        assert!(rename_marker(&not_session, 0, 0, "a", "b").is_err(), "a file that is not a session");
        assert!(rename_marker(&dir, 0, 0, "a", "b").is_err(), "a folder");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_and_empty_files_are_left_out_and_the_session_reads_back() {
        let lines = [
            start(0),
            file(0, "Mic", "Mic.wav", 1, 0, false),
            file(0, "Mic", "Mic part 2.wav", 2, 10, false),
            file(1, "Quiet", "Quiet.wav", 1, 0, false),
            file(1, "Quiet", "../Elsewhere.wav", 2, 10, false),
        ];
        let s = build("Show", &lines, |f| match f {
            "Mic.wav" => Some(10),
            "Quiet.wav" => Some(0),
            "../Elsewhere.wav" => Some(10),
            _ => None,
        });
        assert_eq!(s.sources.len(), 1, "the removed part, the empty file and a name outside the folder have no source");
        assert_eq!(s.tracks.len(), 2, "a track whose file is empty is still listed");
        let text = craft_scraft_format::write_session(&s, craft_scraft_format::MAX_SESSION_BYTES).unwrap();
        let back = Session::from_json(&text).unwrap();
        assert_eq!(back, s);
    }
}
