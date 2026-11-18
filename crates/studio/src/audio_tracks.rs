//! Audio tracks: each source set to record its own track, and the program mix, written to WAV
//! files while a recording or the stream runs ([`studiocraft_audio_tracks`]).
//!
//! The studio's thread decides when a take runs, builds the taps (their pools are made here, not
//! on the mixer thread) and hands them to the mixer, and reads the take's status from atomics.
//! The files are written on the take's own threads. Stopping returns at once. While the setting
//! is off no tap, thread or pool exists, and the mixer's per-input cost is one `Option` check.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;

use serde_json::{Value, json};
use studiocraft_audio_tracks::recover::{self, Outcome};
use studiocraft_audio_tracks::{DiskConfig, Faults, MAX_TRACKS, State, StopAfter, StopReason, Take, TakeConfig, TrackSpec, space};
use studiocraft_engine::mixer::{MixerCmd, TapSet};
use studiocraft_media::tap::TapSlot;
use studiocraft_model::{AudioTracksWhen, SourceId};

use crate::{Notice, OutputKind, Studio, recording_folder};

/// The program mix's file name, first in a folder's listing.
pub const PROGRAM_NAME: &str = "00 Program";

/// The audio tracks part of the studio.
#[derive(Default)]
pub(crate) struct AudioTracks {
    take: Option<Take>,
    /// Takes asked to stop, kept until their files are complete, each with whether its early
    /// stop was already reported.
    pub(crate) finishing: Vec<(Take, bool)>,
    /// The tap of each recorded source, kept while the take runs.
    pub(crate) slots: HashMap<SourceId, Arc<TapSlot>>,
    pub(crate) program: Option<Arc<TapSlot>>,
    /// The sources recorded, in the order sent.
    pub(crate) recorded: Vec<SourceId>,
    /// The number of the last tap set sent to the mixer.
    pub(crate) generation: u64,
    /// A list the take's queue had no room for, sent again on the next tick, with whether it
    /// comes from a replaced collection.
    pending: Option<(Vec<TrackSpec>, bool)>,
    /// The collection was replaced since the last list was sent.
    replaced: bool,
    /// Whether the mixer has the running take's taps. They are sent once the take's collect
    /// thread reads them, so a tap does not fill and drop the take's first audio meanwhile.
    pub(crate) armed: bool,
    /// Whether the last take's early stop was reported.
    reported: bool,
    /// The last take's folder and status, kept after it ends for the status line.
    last: Option<(PathBuf, Arc<studiocraft_audio_tracks::Status>)>,
    /// Folders of takes started here whose threads may not have made them yet.
    taken: Vec<PathBuf>,
    /// The write buffer and file size the running take uses; changed settings apply from the
    /// next take.
    take_sizes: Option<(u32, u32)>,
    repair: Option<JoinHandle<Outcome>>,
}

impl Studio {
    /// Start, stop or update the take to match the settings and the outputs. Called each tick
    /// and after the collection changes; it does nothing while the setting is off.
    pub(crate) fn sync_audio_tracks(&mut self) {
        self.finish_audio_tracks();
        let settings = self.profile.audio_tracks;
        let recording = self.output(OutputKind::Recording).filter(|s| s.active);
        let streaming = self.output(OutputKind::Streaming).is_some_and(|s| s.active);
        #[cfg(test)]
        let streaming = streaming || self.test_streaming;
        let wanted = settings.enabled
            && match settings.when {
                AudioTracksWhen::Recording => recording.is_some(),
                AudioTracksWhen::Streaming => streaming,
                AudioTracksWhen::Either => recording.is_some() || streaming,
            };
        if let Some(take) = &self.audio_tracks.take
            && take.status().state() == Some(State::Stopped)
        {
            self.report_audio_tracks_stop();
            // The outputs go on; only the tracks stop.
            self.stop_audio_tracks();
            if let Some(last) = self.audio_tracks.finishing.last_mut() {
                last.1 = true;
            }
            return;
        }
        match (wanted, self.audio_tracks.take.is_some()) {
            // After an early stop or a failed start, the next take waits until the outputs stop.
            (true, false) if !self.audio_tracks.reported => self.start_audio_tracks(recording.map(|r| PathBuf::from(r.target))),
            (true, true) => self.update_audio_tracks(),
            (false, true) => self.stop_audio_tracks(),
            _ => {}
        }
        if !wanted {
            // The next take may start again after an early stop.
            self.audio_tracks.reported = false;
        }
    }

    /// The sources that audio tracks would write a file for, with the program not counted.
    pub fn audio_track_sources(&self) -> usize {
        self.track_source_ids().len()
    }

    /// The sources set to record their own track, in collection order, up to [`MAX_TRACKS`]
    /// tracks with the program. Later sources are not recorded, so a large collection cannot
    /// make a tap, with its preallocated blocks, for every source.
    fn track_source_ids(&self) -> Vec<SourceId> {
        let registry = self.engine.registry();
        let room = MAX_TRACKS - usize::from(self.profile.audio_tracks.include_program);
        let all =
            self.engine.collection().sources.iter().filter(|s| s.audio.records_track() && registry.source(&s.kind).is_ok_and(|p| p.caps().audio));
        all.take(room).map(|s| s.id).collect()
    }

    /// The sources to record now, with their taps, the program mix first.
    fn audio_track_specs(&mut self) -> Vec<TrackSpec> {
        let ids = self.track_source_ids();
        let c = self.engine.collection();
        let at = &mut self.audio_tracks;
        let mut specs = Vec::new();
        if self.profile.audio_tracks.include_program {
            let slot = at.program.get_or_insert_with(|| Arc::new(TapSlot::new()));
            specs.push(TrackSpec { source: None, name: PROGRAM_NAME.into(), slot: Arc::clone(slot) });
        }
        let mut recorded = Vec::new();
        for s in c.sources.iter().filter(|s| ids.contains(&s.id)) {
            let slot = at.slots.entry(s.id).or_insert_with(|| Arc::new(TapSlot::new()));
            specs.push(TrackSpec { source: Some(s.id), name: s.name.clone(), slot: Arc::clone(slot) });
            recorded.push(s.id);
        }
        // A source no longer recorded gives up its tap; added back, it gets a new one.
        at.slots.retain(|id, _| recorded.contains(id));
        at.recorded = recorded;
        specs
    }

    fn send_taps(&mut self, specs: &[TrackSpec]) {
        self.audio_tracks.generation += 1;
        let set = TapSet {
            program: specs.iter().find(|s| s.source.is_none()).map(|s| Arc::clone(&s.slot)),
            inputs: specs.iter().filter_map(|s| s.source.map(|id| (id, Arc::clone(&s.slot)))).collect(),
        };
        self.mixer.send(MixerCmd::Taps { set: Some(Arc::new(set)), generation: self.audio_tracks.generation });
    }

    fn start_audio_tracks(&mut self, recording: Option<PathBuf>) {
        let folder = recording_folder(&self.profile.output.folder);
        // A take started earlier may not have made its folder yet; once it has, the folder's
        // existence keeps its name taken.
        self.audio_tracks.taken.retain(|p| !p.exists());
        let Some(dir) = take_dir(recording.as_deref(), &folder, &self.audio_tracks.taken) else {
            self.notices.push(Notice::new("Audio tracks could not start: {error}", vec![("error", "no free folder name".into())], true));
            self.audio_tracks.reported = true;
            return;
        };
        let specs = self.audio_track_specs();
        let outputs = self.output_bytes_per_second();
        let settings = self.profile.audio_tracks;
        let tracks = specs.len() as u64;
        let cfg = TakeConfig {
            disk: DiskConfig {
                dir: dir.clone(),
                segment_bytes: u64::from(settings.segment_gib) << 30,
                reserve_bytes: space::reserve(outputs),
                warn_bytes: space::warning(outputs, tracks),
                faults: self.audio_tracks_faults(),
            },
            buffer_mib: settings.buffer_mib,
        };
        // The taps go to the mixer once the take is ready ([`Studio::update_audio_tracks`]); the
        // files start at this time either way.
        match Take::start_from(cfg, specs, Some(self.clock.now())) {
            Ok(take) => {
                self.audio_tracks.armed = false;
                if let Some(p) = dir.parent() {
                    self.audio_tracks.taken.push(p.to_path_buf());
                }
                self.audio_tracks.take_sizes = Some((settings.buffer_mib, settings.segment_gib));
                self.audio_tracks.last = Some((dir, Arc::clone(take.status())));
                self.audio_tracks.take = Some(take);
                self.audio_tracks.reported = false;
            }
            Err(e) => {
                self.remove_taps();
                self.notices.push(Notice::new("Audio tracks could not start: {error}", vec![("error", e.to_string())], true));
                self.audio_tracks.reported = true;
            }
        }
    }

    /// Send the taps once the take is ready, then follow sources added, removed or switched on
    /// or off during the take.
    fn update_audio_tracks(&mut self) {
        let arming = !self.audio_tracks.armed;
        if arming {
            if !self.audio_tracks.take.as_ref().is_some_and(|t| t.status().ready()) {
                return;
            }
            self.audio_tracks.armed = true;
        }
        if let Some((list, replace)) = self.audio_tracks.pending.take()
            && let Some(take) = &self.audio_tracks.take
            && !send_tracks(take, list.clone(), self.clock.now(), replace, self.taps_gate())
        {
            self.audio_tracks.pending = Some((list, replace));
        }
        let replace = std::mem::take(&mut self.audio_tracks.replaced);
        if replace {
            // The new collection's sources get new taps, even those whose ids match the old ones.
            self.audio_tracks.slots.clear();
        }
        let now = self.track_source_ids();
        let program = self.profile.audio_tracks.include_program == self.audio_tracks.program.is_some();
        if !arming && !replace && now == self.audio_tracks.recorded && program {
            return;
        }
        if !self.profile.audio_tracks.include_program {
            self.audio_tracks.program = None;
        }
        let specs = self.audio_track_specs();
        self.send_taps(&specs);
        // A list still waiting from a replaced collection keeps that, so the old files end.
        let replace = replace || self.audio_tracks.pending.as_ref().is_some_and(|(_, r)| *r);
        let gate = self.taps_gate();
        if let Some(take) = &self.audio_tracks.take {
            if send_tracks(take, specs.clone(), self.clock.now(), replace, gate) {
                self.audio_tracks.pending = None;
            } else {
                self.audio_tracks.pending = Some((specs, replace));
            }
        }
    }

    /// The scene collection is about to be replaced: while a take runs, its sources' files end
    /// and the new collection's sources start their own, even where the ids match. Called before
    /// the new collection's mixer inputs are sent, so the mixer lets go of the old taps first and
    /// no new source's audio reaches an old source's file.
    pub(crate) fn audio_tracks_collection_replaced(&mut self) {
        if self.audio_tracks.take.is_some() {
            let armed = self.audio_tracks.armed;
            self.remove_taps();
            self.audio_tracks.armed = armed;
            self.audio_tracks.replaced = true;
        }
    }

    /// The tap change sent last, for a track list to wait for. A mixer without a thread applies
    /// nothing, so a list waits for nothing then.
    fn taps_gate(&self) -> Option<StopAfter> {
        self.mixer.running().then(|| (self.mixer.taps_applied_counter(), self.audio_tracks.generation))
    }

    fn remove_taps(&mut self) {
        self.audio_tracks.generation += 1;
        self.mixer.send(MixerCmd::Taps { set: None, generation: self.audio_tracks.generation });
        self.audio_tracks.slots.clear();
        self.audio_tracks.program = None;
        self.audio_tracks.recorded.clear();
        self.audio_tracks.pending = None;
        self.audio_tracks.replaced = false;
    }

    /// Stop the take. Returns at once; its threads finish the files.
    pub(crate) fn stop_audio_tracks(&mut self) {
        let Some(take) = self.audio_tracks.take.take() else { return };
        self.remove_taps();
        // The take ends after the mixer has applied the removal, so no block is stored after
        // the collect thread's last look at the taps. A mixer without a thread applies nothing.
        if self.mixer.running() {
            take.stop_after(self.mixer.taps_applied_counter(), self.audio_tracks.generation);
        } else {
            take.stop();
        }
        self.audio_tracks.finishing.push((take, false));
    }

    /// Drop the takes whose files are complete, and report repairs made at launch.
    fn finish_audio_tracks(&mut self) {
        // A take that failed while it was finishing its files is reported like an early stop.
        let mut failed = Vec::new();
        self.audio_tracks.finishing.retain(|(t, reported)| {
            let done = t.finished();
            if done && !reported && t.status().state() == Some(State::Stopped) {
                failed.push(Arc::clone(t.status()));
            }
            !done
        });
        for status in failed {
            self.notices.push(stop_notice(&status));
        }
        if self.audio_tracks.repair.as_ref().is_some_and(JoinHandle::is_finished)
            && let Some(h) = self.audio_tracks.repair.take()
            && let Ok(out) = h.join()
        {
            if !out.repaired.is_empty() {
                let folders = out.repaired.iter().map(|r| r.dir.display().to_string()).collect::<Vec<_>>().join(", ");
                self.notices.push(Notice::new(
                    "Audio tracks repaired after the app closed during a take: {folders}",
                    vec![("folders", folders)],
                    false,
                ));
            }
            for (dir, e) in &out.failed {
                log::warn!("audio tracks: repair at launch: {}: {e}", dir.display());
            }
        }
    }

    fn report_audio_tracks_stop(&mut self) {
        if self.audio_tracks.reported {
            return;
        }
        self.audio_tracks.reported = true;
        let Some((_, status)) = &self.audio_tracks.last else { return };
        let notice = stop_notice(status);
        self.notices.push(notice);
    }

    /// Repair takes in the recording folder that ended without finishing, on a background
    /// thread, when audio tracks are on.
    pub(crate) fn repair_audio_tracks_at_launch(&mut self) {
        if !self.profile.audio_tracks.enabled || self.audio_tracks.repair.is_some() {
            return;
        }
        let folder = recording_folder(&self.profile.output.folder);
        let spawned = std::thread::Builder::new().name("audio-tracks-repair".into()).spawn(move || recover::repair_each(&folder));
        match spawned {
            Ok(h) => self.audio_tracks.repair = Some(h),
            Err(e) => log::warn!("audio tracks: the repair thread could not start: {e}"),
        }
    }

    /// Repair the takes in `folder` now, for `audio_tracks.repair`.
    pub(crate) fn repair_audio_tracks(&mut self, folder: &str) -> Result<Value, crate::StudioError> {
        let path = Path::new(folder);
        if !path.is_dir() {
            return Err(crate::StudioError::Output(format!("{folder} is not a folder")));
        }
        let out = recover::repair_each(path);
        let takes: Vec<Value> = out
            .repaired
            .iter()
            .map(|r| {
                let files: Vec<Value> = r.files.iter().map(|(f, n)| json!({"file": f, "seconds": *n as f64 / 48_000.0})).collect();
                json!({"folder": r.dir.display().to_string(), "files": files})
            })
            .collect();
        let failed: Vec<Value> = out.failed.iter().map(|(d, e)| json!({"folder": d.display().to_string(), "error": e.to_string()})).collect();
        Ok(json!({"repaired": takes, "failed": failed}))
    }

    /// The audio tracks part of the studio's state, read from atomics.
    pub fn audio_tracks_json(&self) -> Value {
        let s = &self.profile.audio_tracks;
        let mut v = json!({
            "enabled": s.enabled, "when": s.when.id(), "include_program": s.include_program, "buffer_mib": s.buffer_mib,
            "segment_gib": s.segment_gib, "active": self.audio_tracks.take.is_some(), "finishing": self.audio_tracks.finishing.len(),
        });
        if let (Some(o), Some((buffer, segment)), true) = (v.as_object_mut(), self.audio_tracks.take_sizes, self.audio_tracks.take.is_some()) {
            // The running take keeps the sizes it started with.
            o.insert("take_buffer_mib".into(), json!(buffer));
            o.insert("take_segment_gib".into(), json!(segment));
        }
        if let (Some(o), Some((dir, st))) = (v.as_object_mut(), &self.audio_tracks.last) {
            let state = match st.state() {
                Some(State::Recording) => "recording",
                Some(State::Finishing) => "finishing",
                Some(State::Done) => "done",
                Some(State::Stopped) => "stopped",
                None => "idle",
            };
            o.insert("state".into(), json!(state));
            o.insert("folder".into(), json!(dir.display().to_string()));
            o.insert("tracks".into(), json!(st.tracks()));
            o.insert("seconds".into(), json!(st.seconds()));
            o.insert("bytes".into(), json!(st.bytes()));
            o.insert("dropped_seconds".into(), json!(st.dropped_seconds()));
            o.insert("low_space".into(), json!(st.low_space()));
            o.insert("track_limit".into(), json!(st.track_limit()));
            if let Some(r) = st.reason() {
                o.insert("stopped_at".into(), json!(clock_text(st.stopped_at_seconds())));
                o.insert("reason".into(), json!(r.text()));
            }
        }
        v
    }

    /// The status of the last take, for the status bar; `None` before the first.
    pub fn audio_tracks_status(&self) -> Option<&Arc<studiocraft_audio_tracks::Status>> {
        self.audio_tracks.last.as_ref().map(|(_, s)| s)
    }

    /// Bytes a second the running outputs write to disk or send: what each has written so far,
    /// or the stream settings' bitrate when that is more.
    fn output_bytes_per_second(&self) -> u64 {
        let set = u64::from(self.profile.stream.video_bitrate_kbps.saturating_add(self.profile.stream.audio_bitrate_kbps)) * 1000 / 8;
        [OutputKind::Recording, OutputKind::Streaming, OutputKind::Replay]
            .into_iter()
            .filter_map(|k| self.output(k).filter(|s| s.active))
            .map(|s| {
                let measured = s.bytes.checked_div(s.elapsed.as_secs()).unwrap_or(0);
                measured.max(set)
            })
            .sum()
    }

    fn audio_tracks_faults(&self) -> Faults {
        #[cfg(test)]
        {
            self.test_faults.clone()
        }
        #[cfg(not(test))]
        {
            Faults::default()
        }
    }

    /// Stop the take and wait for its files, when the app closes.
    pub(crate) fn finish_audio_tracks_blocking(&mut self) {
        self.stop_audio_tracks();
        for (t, _) in self.audio_tracks.finishing.drain(..) {
            t.join();
        }
    }

    /// Whether a take is running, for tests.
    #[cfg(test)]
    pub(crate) fn audio_tracks_take(&self) -> Option<&Take> {
        self.audio_tracks.take.as_ref()
    }

    /// Whether a thread or a tap of audio tracks exists, for tests of the setting turned off.
    #[cfg(test)]
    pub(crate) fn audio_tracks_idle(&self) -> bool {
        let a = &self.audio_tracks;
        a.take.is_none() && a.finishing.is_empty() && a.slots.is_empty() && a.program.is_none() && a.repair.is_none() && a.generation == 0
    }
}

/// The take's `Audio Files` folder: beside the recording at `recording`, else a folder named for
/// the date and time in `folder` while only the stream runs. A name in `taken` is skipped even
/// before its take's thread has made the folder. `None` when 999 such names are taken.
fn take_dir(recording: Option<&Path>, folder: &Path, taken: &[PathBuf]) -> Option<PathBuf> {
    let (parent, stem) = match recording {
        Some(r) => (
            r.parent().map(Path::to_path_buf).unwrap_or_else(|| folder.to_path_buf()),
            r.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "Take".into()),
        ),
        None => (folder.to_path_buf(), format!("Stream {}", chrono::Local::now().format("%Y-%m-%d %H-%M-%S"))),
    };
    (1..1000)
        .map(|n| if n == 1 { format!("{stem} Audio") } else { format!("{stem} ({n}) Audio") })
        .map(|name| parent.join(name))
        .find(|p| !p.exists() && !taken.contains(p))
        .map(|p| p.join("Audio Files"))
}

/// Send `specs` to the take. With `after`, the take applies them once the mixer has applied that
/// tap change, so a source that stops being recorded keeps the blocks stored for it until then.
fn send_tracks(take: &Take, specs: Vec<TrackSpec>, at: studiocraft_media::Tick, replace: bool, after: Option<StopAfter>) -> bool {
    if replace { take.replace_sources(specs, at, after) } else { take.set_tracks(specs, at, after) }
}

/// `seconds` as `hh:mm:ss`.
pub fn clock_text(seconds: f64) -> String {
    let s = if seconds.is_finite() && seconds > 0.0 { seconds as u64 } else { 0 };
    format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

/// The notice for a take that stopped early, with one template per reason so the whole notice is
/// translated.
fn stop_notice(status: &studiocraft_audio_tracks::Status) -> Notice {
    let template = match status.reason() {
        Some(StopReason::DiskFull) => "Audio tracks stopped at {time}: the disk is full",
        Some(StopReason::LowSpace) => "Audio tracks stopped at {time}: the disk is almost full",
        Some(StopReason::CreateFailed) => "Audio tracks stopped at {time}: the files could not be created",
        Some(StopReason::SpaceUnknown) => "Audio tracks stopped at {time}: the free space on the disk could not be read",
        Some(StopReason::NoMemory) => "Audio tracks stopped at {time}: there is not enough memory for the write buffer",
        Some(StopReason::WriteFailed) | None => "Audio tracks stopped at {time}: a file could not be written",
    };
    Notice::new(template, vec![("time", clock_text(status.stopped_at_seconds()))], true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn take_folders_sit_beside_the_recording_and_never_reuse_a_name() {
        let base = std::env::temp_dir().join(format!("studiocraft-take-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let rec = base.join("2026-11-17 10-00-00.mkv");
        let first = take_dir(Some(&rec), &base, &[]);
        assert_eq!(first, Some(base.join("2026-11-17 10-00-00 Audio").join("Audio Files")));
        // A take that started but has not made its folder yet keeps the name.
        let started = [base.join("2026-11-17 10-00-00 Audio")];
        assert_eq!(take_dir(Some(&rec), &base, &started), Some(base.join("2026-11-17 10-00-00 (2) Audio").join("Audio Files")));
        std::fs::create_dir_all(base.join("2026-11-17 10-00-00 Audio")).unwrap();
        assert_eq!(take_dir(Some(&rec), &base, &[]), Some(base.join("2026-11-17 10-00-00 (2) Audio").join("Audio Files")));
        let stream = take_dir(None, &base, &[]).unwrap();
        assert!(stream.starts_with(&base) && stream.to_string_lossy().contains("Stream "));
        assert_eq!(clock_text(3725.9), "01:02:05");
        assert_eq!(clock_text(f64::NAN), "00:00:00");
    }
}
