//! Audio tracks: each source set to record its own track, and the program mix, written to WAV
//! files while a recording or the stream runs ([`studiocraft_audio_tracks`]).
//!
//! The studio's thread decides when a take runs, builds the taps (their pools are made here, not
//! on the mixer thread) and hands them to the mixer, and reads the take's status from atomics.
//! The files are written on the take's own threads, which also make the take's folder. Stopping
//! returns at once. While the setting is off no tap, thread or pool exists, and the mixer's
//! per-input cost is one `Option` check.
//!
//! A take follows one output, its anchor: the recording, or the stream. A take started with its
//! output starts at the output's first sample; a take started later (the setting turned on, or
//! the anchor changed) starts at that time, and each file's `bext` time reference says where on
//! the output's timeline it starts. A take ends at the time its output stopped.
//!
//! A take's disk thread writes its session file for the audio editor. When the take's files are
//! complete and no other take has started, the "after a take" setting opens the session in the
//! audio editor or shows its folder, on a short thread of its own ([`crate::audio_editor`]);
//! `audio_tracks.open_last` does the same for the last session on request.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use studiocraft_audio_tracks::collect::FRAME_TICKS;
use studiocraft_audio_tracks::recover::{self, Outcome};
use studiocraft_audio_tracks::{DiskConfig, Faults, MAX_TRACKS, State, StopAfter, StopReason, Take, TakeConfig, Timing, TrackSpec, space};
use studiocraft_engine::mixer::{MixerCmd, TapSet};
use studiocraft_media::Tick;
use studiocraft_media::tap::TapSlot;
use studiocraft_model::{AfterStop, AudioTracksWhen, SourceId};

use crate::audio_editor::{self, Launcher, Opened, SystemLauncher};
use crate::recording_markers::InTake;
use crate::{Notice, OutputKind, Studio, recording_folder};

/// The program mix's file name, first in a folder's listing.
pub const PROGRAM_NAME: &str = "00 Program";

/// Takes still finishing their files when a new take may start; a new take waits for one to end.
const MAX_FINISHING: usize = 2;

/// How long closing the app waits for takes to finish their files. A take still writing then is
/// left to its threads, and its journal lets the next launch repair it.
const CLOSE_WAIT: Duration = Duration::from_secs(10);

/// How long the app waits, as it closes, for the editor or file manager to start for the take
/// that ended then.
const OPEN_WAIT: Duration = Duration::from_secs(3);

/// Repairs that may run at once.
const MAX_REPAIRS: usize = 4;

/// Openings in the audio editor or the file manager that may run at once.
const MAX_OPENING: usize = 4;

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
    /// The output the running take follows, and that output's start on the studio clock.
    anchor: Option<(OutputKind, Tick)>,
    /// The take ended for the output starting now: that output, what the take followed and where
    /// it ended, so a start that fails can continue it from there.
    cut_for: Option<(OutputKind, (OutputKind, Tick), Tick)>,
    /// The collection's revision and the program setting the running take's list was made
    /// from, so an unchanged collection costs no work each tick.
    listed: Option<(u64, bool)>,
    /// Whether the last take's early stop was reported.
    reported: bool,
    /// The last take's status, kept after it ends for the status line; its folder once made.
    last: Option<Arc<studiocraft_audio_tracks::Status>>,
    /// The write buffer and file size the running take uses; changed settings apply from the
    /// next take.
    take_sizes: Option<(u32, u32)>,
    /// Repairs running on their own threads, at launch or for `audio_tracks.repair`, each with its
    /// place in `stop_order`'s count.
    repairs: Vec<(JoinHandle<Outcome>, u64)>,
    /// Holds the next repair `audio_tracks.repair` starts until the sender sends or is dropped,
    /// for tests.
    #[cfg(test)]
    pub(crate) repair_gate: Option<std::sync::mpsc::Receiver<()>>,
    /// What the last repair did, for the status.
    last_repair: Option<Value>,
    /// Sources set to record their own track were left out, because more than
    /// [`MAX_TRACKS`] tracks would run at once.
    source_limit: bool,
    /// The session file of the take that stopped last or the repair started last, of those that
    /// ended with one.
    last_session: Option<PathBuf>,
    /// Openings of a session or a folder running on their own threads.
    opening: Vec<JoinHandle<Result<Opened, String>>>,
    /// What starts the editor and the file manager; the system's when unset.
    pub(crate) launcher: Option<Arc<dyn Launcher>>,
    /// The takes still finishing that ended because the next one started.
    handed_on: Vec<Arc<studiocraft_audio_tracks::Status>>,
    /// The takes still finishing, each with its place in the order the takes stopped and the
    /// repairs started (`stops` counts both).
    stop_order: Vec<(Arc<studiocraft_audio_tracks::Status>, u64)>,
    stops: u64,
    /// The place of the newest take that ended with a session: a take that finishes after a newer
    /// one opens nothing.
    newest_take_stop: u64,
    /// The place of the take or repair whose session is `last_session`: one that finishes after a
    /// newer one does not replace that one's session.
    last_session_place: u64,
}

impl Studio {
    /// Start, stop or update the take to match the settings and the outputs. Called each tick
    /// and after the collection changes; it does nothing while the setting is off.
    pub(crate) fn sync_audio_tracks(&mut self) {
        self.finish_audio_tracks();
        let anchor = if self.profile.audio_tracks.enabled { self.audio_tracks_anchor() } else { None };
        if let Some(take) = &self.audio_tracks.take
            && take.status().state() == Some(State::Stopped)
        {
            self.report_audio_tracks_stop();
            // The outputs go on; only the tracks stop.
            self.stop_audio_tracks();
            if let Some(last) = self.audio_tracks.finishing.last_mut() {
                last.1 = true;
            }
            if anchor.is_some() {
                return;
            }
        }
        let running = self.audio_tracks.take.is_some();
        match anchor {
            Some(a) if running && self.audio_tracks.anchor == Some(a) => self.update_audio_tracks(),
            // The anchor changed (Either: the recording started or stopped while the stream
            // runs): the take ends, and one that follows the new anchor starts now.
            Some(a) if running => {
                self.hand_on_audio_tracks_at(self.clock.now());
                self.start_audio_tracks(a, self.clock.now());
                self.keep_handoff_if_started(true);
            }
            // After an early stop or a failed start, the next take waits until the outputs stop.
            Some(a) if !self.audio_tracks.reported => self.start_audio_tracks(a, self.clock.now()),
            None if running => self.stop_audio_tracks(),
            _ => {}
        }
        if anchor.is_none() {
            // The next take may start again after an early stop.
            self.audio_tracks.reported = false;
        }
    }

    /// The output a take follows now: the recording or the stream, as the setting says, and the
    /// recording when Either finds both running. `None` when it is not running.
    fn audio_tracks_anchor(&self) -> Option<(OutputKind, Tick)> {
        let start = |k: OutputKind| self.outputs.start_of(k).map(|t| (k, t));
        let stream = start(OutputKind::Streaming);
        #[cfg(test)]
        let stream = stream.or(self.test_streaming.then_some((OutputKind::Streaming, Tick(0))));
        match self.profile.audio_tracks.when {
            AudioTracksWhen::Recording => start(OutputKind::Recording),
            AudioTracksWhen::Streaming => stream,
            AudioTracksWhen::Either => start(OutputKind::Recording).or(stream),
        }
    }

    /// `kind` is about to start at `start` on the studio clock: start the take that follows it
    /// now, before its first sample, so the take's first sample is the output's. The take is
    /// named after `recording`: the recording's file, or the stream's copy.
    pub(crate) fn audio_tracks_output_starting(&mut self, kind: OutputKind, start: Tick, recording: Option<&Path>) {
        let (enabled, when) = (self.profile.audio_tracks.enabled, self.profile.audio_tracks.when);
        if !enabled || self.audio_tracks.reported {
            return;
        }
        let follows = match when {
            AudioTracksWhen::Recording => kind == OutputKind::Recording,
            AudioTracksWhen::Streaming => kind == OutputKind::Streaming,
            AudioTracksWhen::Either => {
                kind == OutputKind::Recording || (kind == OutputKind::Streaming && self.outputs.start_of(OutputKind::Recording).is_none())
            }
        };
        self.audio_tracks.cut_for = None;
        if !follows {
            return;
        }
        let handed = self.audio_tracks.take.is_some();
        if handed {
            self.audio_tracks.cut_for = self.audio_tracks.anchor.map(|a| (kind, a, start));
            self.hand_on_audio_tracks_at(start);
        }
        self.start_audio_tracks_for((kind, start), start, recording.map(Path::to_path_buf));
        self.keep_handoff_if_started(handed);
    }

    /// `kind` did not start after [`Studio::audio_tracks_output_starting`]: its take is thrown
    /// away, files and folder included. A take that ended for it and whose output still runs
    /// continues in a new take from the time it ended.
    pub(crate) fn audio_tracks_output_failed(&mut self, kind: OutputKind) {
        let cut_for = self.audio_tracks.cut_for.take().filter(|(starting, _, _)| *starting == kind);
        let mut thrown = None;
        if self.audio_tracks.anchor.is_some_and(|(k, _)| k == kind)
            && let Some(take) = self.audio_tracks.take.take()
        {
            self.remove_taps();
            self.audio_tracks.anchor = None;
            self.audio_tracks.take_sizes = None;
            if self.audio_tracks.last.as_ref().is_some_and(|s| Arc::ptr_eq(s, take.status())) {
                self.audio_tracks.last = None;
            }
            take.discard();
            thrown = Some(take);
        }
        // The ended take continues whether or not a take for `kind` started. Started before the
        // thrown-away take counts as finishing, so it has room.
        if let Some((_, anchor, end)) = cut_for
            && self.audio_tracks.take.is_none()
            && self.profile.audio_tracks.enabled
            && self.audio_tracks_anchor() == Some(anchor)
        {
            self.start_audio_tracks(anchor, end);
        }
        // With no take after it, the ended take is the last one and opens as any other does.
        // `keep_handoff_if_started` already did this when the thrown-away take never started.
        if cut_for.is_some() && thrown.is_some() && self.audio_tracks.take.is_none() {
            self.audio_tracks.handed_on.pop();
        }
        if let Some(take) = thrown {
            self.audio_tracks.finishing.push((take, true));
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
        self.track_sources().0
    }

    /// [`Self::track_source_ids`], and whether sources set to record were left out by the limit.
    fn track_sources(&self) -> (Vec<SourceId>, bool) {
        let registry = self.engine.registry();
        let room = MAX_TRACKS - usize::from(self.profile.audio_tracks.include_program);
        let mut all =
            self.engine.collection().sources.iter().filter(|s| s.audio.records_track() && registry.source(&s.kind).is_ok_and(|p| p.caps().audio));
        let ids: Vec<SourceId> = all.by_ref().take(room).map(|s| s.id).collect();
        (ids, all.next().is_some())
    }

    /// Whether the running take leaves out sources set to record their own track, because more
    /// than [`MAX_TRACKS`] tracks would run at once.
    pub fn audio_tracks_source_limit(&self) -> bool {
        self.audio_tracks.take.is_some() && self.audio_tracks.source_limit
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

    /// Start a take that follows `anchor`, from `origin` on the studio clock.
    fn start_audio_tracks(&mut self, anchor: (OutputKind, Tick), origin: Tick) {
        let recording = match anchor.0 {
            OutputKind::Recording => self.output(OutputKind::Recording).map(|r| PathBuf::from(r.target)),
            OutputKind::Streaming | OutputKind::Replay => None,
        };
        self.start_audio_tracks_for(anchor, origin, recording);
    }

    /// Start a take that follows `anchor`, from `origin` on the studio clock, beside the
    /// recording at `recording` or in the recording folder. The take's own thread makes its
    /// folder, so this thread does not touch the disk.
    fn start_audio_tracks_for(&mut self, anchor: (OutputKind, Tick), origin: Tick, recording: Option<PathBuf>) {
        // Each take holds its buffer until its files are complete; a new one waits for room, and
        // the next tick starts it from then.
        if self.audio_tracks.finishing.len() >= MAX_FINISHING {
            log::debug!("audio tracks: waiting for {} takes to finish before the next starts", self.audio_tracks.finishing.len());
            return;
        }
        let (parent, stem) = take_place(recording.as_deref(), &recording_folder(&self.profile.output.folder));
        let specs = self.audio_track_specs();
        self.audio_tracks.source_limit = self.track_sources().1;
        let outputs = self.output_bytes_per_second();
        let settings = self.profile.audio_tracks.clone();
        let tracks = specs.len() as u64;
        let cfg = TakeConfig {
            disk: DiskConfig {
                dir: parent,
                folder_stem: Some(stem),
                timing: self.audio_tracks_timing(anchor, origin),
                segment_bytes: u64::from(settings.segment_gib) << 30,
                reserve_bytes: space::reserve(outputs),
                warn_bytes: space::warning(outputs, tracks),
                faults: self.audio_tracks_faults(),
            },
            buffer_mib: settings.buffer_mib,
        };
        match Take::start_from(cfg, specs.clone(), Some(origin)) {
            Ok(take) => {
                // The taps go to the mixer at once: each holds 683 ms of blocks, far longer than
                // the take's threads take to start, and blocks are placed by their time.
                self.send_taps(&specs);
                self.audio_tracks.listed = Some((self.engine.revision(), settings.include_program));
                self.audio_tracks.take_sizes = Some((settings.buffer_mib, settings.segment_gib));
                self.audio_tracks.last = Some(Arc::clone(take.status()));
                self.audio_tracks.take = Some(take);
                self.audio_tracks.anchor = Some(anchor);
                self.audio_tracks.reported = false;
            }
            Err(e) => {
                self.remove_taps();
                self.notices.push(Notice::new("Audio tracks could not start: {error}", vec![("error", e.to_string())], true));
                self.audio_tracks.reported = true;
            }
        }
    }

    /// Where a take from `origin` sits on the timeline of the output at `anchor`.
    fn audio_tracks_timing(&self, anchor: (OutputKind, Tick), origin: Tick) -> Timing {
        let offset = origin.0.saturating_sub(anchor.1.0).max(0);
        // The wall-clock time of `origin`, which is now or a moment ago.
        let ago = self.clock.now().0.saturating_sub(origin.0).max(0) / (FRAME_TICKS * 48);
        let at = chrono::Local::now() - chrono::Duration::milliseconds(ago);
        Timing {
            anchor: match anchor.0 {
                OutputKind::Recording => "recording".into(),
                OutputKind::Streaming | OutputKind::Replay => "stream".into(),
            },
            origin_tick: Some(origin.0),
            offset_frames: u64::try_from(offset / FRAME_TICKS).unwrap_or(0),
            started_unix_ms: Some(at.timestamp_millis()),
            date: at.format("%Y-%m-%d").to_string(),
            time: at.format("%H:%M:%S").to_string(),
        }
    }

    /// Follow sources added, removed or switched on or off during the take. An unchanged
    /// collection costs a comparison of two numbers.
    fn update_audio_tracks(&mut self) {
        let listed = (self.engine.revision(), self.profile.audio_tracks.include_program);
        if self.audio_tracks.pending.is_none() && !self.audio_tracks.replaced && self.audio_tracks.listed == Some(listed) {
            return;
        }
        self.audio_tracks.listed = Some(listed);
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
        let (now, source_limit) = self.track_sources();
        self.audio_tracks.source_limit = source_limit;
        let program = self.profile.audio_tracks.include_program == self.audio_tracks.program.is_some();
        if !replace && now == self.audio_tracks.recorded && program {
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
            self.remove_taps();
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

    /// Stop the take, ending its files now. Returns at once; its threads finish the files.
    pub(crate) fn stop_audio_tracks(&mut self) {
        self.stop_audio_tracks_at(self.clock.now());
    }

    /// Stop the take, ending its files at `end` on the studio clock: every track still recorded
    /// ends there, and audio stamped later is left out. Returns at once.
    fn stop_audio_tracks_at(&mut self, end: Tick) {
        let Some(take) = self.audio_tracks.take.take() else { return };
        self.remove_taps();
        self.audio_tracks.anchor = None;
        // The take ends after the mixer has applied the removal, so no block is stored after
        // the collect thread's last look at the taps. A mixer without a thread applies nothing.
        if self.mixer.running() {
            take.stop_after(self.mixer.taps_applied_counter(), self.audio_tracks.generation, Some(end));
        } else {
            take.stop_at(end);
        }
        self.audio_tracks.stops += 1;
        self.audio_tracks.stop_order.push((Arc::clone(take.status()), self.audio_tracks.stops));
        self.audio_tracks.finishing.push((take, false));
    }

    /// After a take was handed on: when the next one did not start, the ended take opens as any
    /// other does.
    fn keep_handoff_if_started(&mut self, handed: bool) {
        if handed && self.audio_tracks.take.is_none() {
            self.audio_tracks.handed_on.pop();
        }
    }

    /// Stop the take at `end` because the next one starts there: its session opens nothing when
    /// it ends, since the outputs go on.
    fn hand_on_audio_tracks_at(&mut self, end: Tick) {
        self.stop_audio_tracks_at(end);
        if let Some((t, _)) = self.audio_tracks.finishing.last() {
            self.audio_tracks.handed_on.push(Arc::clone(t.status()));
        }
    }

    /// Drop the takes whose files are complete, open the session of one that ended as the "after
    /// a take" setting says, and report repairs and openings that ended.
    fn finish_audio_tracks(&mut self) {
        // A take that failed while it was finishing its files is reported like an early stop.
        let mut failed = Vec::new();
        // Each ended take's session, with whether the take ended because the next one started.
        let mut sessions = Vec::new();
        let handed_on = &mut self.audio_tracks.handed_on;
        let stop_order = &mut self.audio_tracks.stop_order;
        self.audio_tracks.finishing.retain(|(t, reported)| {
            let done = t.finished();
            if !done {
                return true;
            }
            if !reported && t.status().state() == Some(State::Stopped) {
                failed.push(Arc::clone(t.status()));
            }
            let next = handed_on.iter().position(|h| Arc::ptr_eq(h, t.status())).map(|i| handed_on.swap_remove(i)).is_some();
            let place = stop_order.iter().position(|(h, _)| Arc::ptr_eq(h, t.status())).map_or(0, |i| stop_order.swap_remove(i).1);
            if let Some(s) = t.status().session() {
                sessions.push((s.to_path_buf(), next, place));
            }
            false
        });
        sessions.sort_by_key(|s| s.2);
        for status in failed {
            self.notices.push(stop_notice(&status));
        }
        // Each take that ended since the last tick opens, in the order they ended; the last one is
        // the session `audio_tracks.open_last` opens. A take that ended because the next one
        // started opens nothing, since the outputs go on, and so does any take while another one
        // runs.
        let after = self.profile.audio_tracks.after_stop;
        for (session, next, place) in sessions {
            // A take that finishes after a newer one has ended neither replaces nor follows it. A
            // repair started after the take stopped keeps its session as the last one, and the
            // take still opens.
            let newest = place >= self.audio_tracks.newest_take_stop;
            if newest {
                self.audio_tracks.newest_take_stop = place;
            }
            if place >= self.audio_tracks.last_session_place {
                self.audio_tracks.last_session = Some(session.clone());
                self.audio_tracks.last_session_place = place;
            }
            if after != AfterStop::Nothing
                && newest
                && !next
                && self.audio_tracks.take.is_none()
                && let Err(e) = self.start_opening(session, after == AfterStop::OpenEditor)
            {
                self.notices.push(Notice::new("The audio tracks could not be opened: {error}", vec![("error", e)], true));
            }
        }
        while let Some(i) = self.audio_tracks.repairs.iter().position(|(h, _)| h.is_finished()) {
            let (handle, place) = self.audio_tracks.repairs.swap_remove(i);
            let Ok(out) = handle.join() else { continue };
            for (dir, e) in &out.failed {
                log::warn!("audio tracks: repair: {}: {e}", dir.display());
            }
            // A repair that finishes after a newer take or repair has ended does not replace that
            // one's session.
            if let Some(session) = out.repaired.iter().rev().find_map(|r| r.session.clone())
                && place >= self.audio_tracks.last_session_place
            {
                self.audio_tracks.last_session = Some(session);
                self.audio_tracks.last_session_place = place;
            }
            self.notices.extend(repair_notices(&out));
            self.audio_tracks.last_repair = Some(repair_json(&out));
        }
        while let Some(i) = self.audio_tracks.opening.iter().position(JoinHandle::is_finished) {
            let notice = match self.audio_tracks.opening.swap_remove(i).join() {
                Ok(Ok(opened)) => opened_notice(&opened),
                Ok(Err(e)) => Some(Notice::new("The audio tracks could not be opened: {error}", vec![("error", e)], true)),
                Err(_) => {
                    Some(Notice::new("The audio tracks could not be opened: {error}", vec![("error", "the opening stopped on a fault".into())], true))
                }
            };
            self.notices.extend(notice);
        }
    }

    /// Open `session` in the audio editor (`editor`) or show its folder, on a thread of its own.
    /// An error naming the cause when [`MAX_OPENING`] openings are running or the thread could
    /// not start.
    fn start_opening(&mut self, session: PathBuf, editor: bool) -> Result<(), String> {
        if self.audio_tracks.opening.len() >= MAX_OPENING {
            log::warn!("audio tracks: {MAX_OPENING} openings are running; {} is not opened", session.display());
            return Err(format!("{MAX_OPENING} openings of audio tracks are already running"));
        }
        let launcher: Arc<dyn Launcher> = self.audio_tracks.launcher.clone().unwrap_or_else(|| Arc::new(SystemLauncher));
        let editor_path = self.profile.audio_tracks.editor_path.clone();
        let spawned = std::thread::Builder::new().name("audio-tracks-open".into()).spawn(move || {
            if editor { audio_editor::open_session(&*launcher, &session, &editor_path) } else { audio_editor::show_folder(&*launcher, &session) }
        });
        match spawned {
            Ok(h) => {
                self.audio_tracks.opening.push(h);
                Ok(())
            }
            Err(e) => {
                log::warn!("audio tracks: the opening thread could not start: {e}");
                Err(format!("the opening thread could not start: {e}"))
            }
        }
    }

    /// Open the session of the last take in the audio editor, or its folder when the editor
    /// cannot be started, for `audio_tracks.open_last`. Returns at once; what happened comes as a
    /// notice.
    pub(crate) fn open_last_audio_tracks(&mut self) -> Result<Value, crate::StudioError> {
        let err = crate::StudioError::Output;
        let Some(session) = self.audio_tracks.last_session.clone() else {
            let running = self.audio_tracks.take.is_some() || !self.audio_tracks.finishing.is_empty();
            return Err(err(if running {
                "the take of audio tracks is still being written; its session opens once it ends".into()
            } else {
                "no take of audio tracks has ended since StudioCraft started".into()
            }));
        };
        self.start_opening(session.clone(), true).map_err(err)?;
        Ok(json!({"started": true, "session": session.display().to_string()}))
    }

    /// Mark the running take now, for `audio_tracks.marker`: the marker is named `name`, or
    /// "Marker N" for the take's Nth marker. Returns its name and its time in the take.
    pub(crate) fn add_audio_tracks_marker(&mut self, name: Option<String>) -> Result<Value, crate::StudioError> {
        let err = crate::StudioError::Output;
        let Some(take) = &self.audio_tracks.take else {
            return Err(err("no take of audio tracks is running".into()));
        };
        let name = name.unwrap_or_else(|| format!("Marker {}", take.markers().kept().saturating_add(1)));
        match self.mark_audio_tracks(self.clock.now(), &name) {
            Some(Ok(kept)) => Ok(json!({"name": name, "seconds": kept.seconds()})),
            Some(Err(e)) => Err(err(e)),
            None => Err(err("no take of audio tracks is running".into())),
        }
    }

    /// Mark the running take at `now` on the studio clock with a marker named `name`. Returns
    /// where the take holds it, an error naming the cause when the take did not keep it, or
    /// `None` when no take runs.
    pub(crate) fn mark_audio_tracks(&self, now: Tick, name: &str) -> Option<Result<InTake, String>> {
        let markers = self.audio_tracks.take.as_ref()?.markers();
        let at = markers.frame(now);
        Some(
            markers
                .add_numbered(at, name)
                .map(|number| InTake { markers: markers.clone(), number, at, name: name.to_string() })
                .map_err(|e| format!("the marker was not kept: {e}")),
        )
    }

    /// Wait for the openings started here and take their notices as the next tick would, for
    /// tests.
    #[cfg(test)]
    pub(crate) fn audio_tracks_opened(&mut self) {
        while self.audio_tracks.opening.iter().any(|h| !h.is_finished()) {
            std::thread::yield_now();
        }
        self.finish_audio_tracks();
    }

    fn report_audio_tracks_stop(&mut self) {
        if self.audio_tracks.reported {
            return;
        }
        self.audio_tracks.reported = true;
        let Some(status) = &self.audio_tracks.last else { return };
        let notice = stop_notice(status);
        self.notices.push(notice);
    }

    /// Repair takes in the recording folder that ended without finishing, on a background
    /// thread, when audio tracks are on.
    pub(crate) fn repair_audio_tracks_at_launch(&mut self) {
        if !self.profile.audio_tracks.enabled || !self.audio_tracks.repairs.is_empty() {
            return;
        }
        let folder = recording_folder(&self.profile.output.folder);
        let spawned = std::thread::Builder::new().name("audio-tracks-repair".into()).spawn(move || recover::repair_each(&folder));
        match spawned {
            Ok(h) => self.push_repair(h),
            Err(e) => log::warn!("audio tracks: the repair thread could not start: {e}"),
        }
    }

    /// Keep a started repair with its place after the takes stopped and the repairs started
    /// before it.
    pub(crate) fn push_repair(&mut self, h: JoinHandle<Outcome>) {
        self.audio_tracks.stops += 1;
        self.audio_tracks.repairs.push((h, self.audio_tracks.stops));
    }

    /// Repair the takes in `folder` on a background thread, for `audio_tracks.repair`. Returns
    /// at once; the result comes as notices and as the status's `last_repair`, and the status
    /// shows `repairing` until then. A take another repair holds is left to that repair.
    ///
    /// `folder` must be the recording folder or a folder inside it, compared with the links of
    /// both followed; the repair runs on that resolved path.
    pub(crate) fn repair_audio_tracks(&mut self, folder: &str) -> Result<Value, crate::StudioError> {
        if self.audio_tracks.repairs.len() >= MAX_REPAIRS {
            return Err(crate::StudioError::Output(format!("{MAX_REPAIRS} repairs of audio tracks are already running")));
        }
        let path = repair_folder(folder, &recording_folder(&self.profile.output.folder))?;
        let for_thread = path.clone();
        #[cfg(test)]
        let gate = self.audio_tracks.repair_gate.take();
        let spawned = std::thread::Builder::new().name("audio-tracks-repair".into()).spawn(move || {
            #[cfg(test)]
            if let Some(gate) = gate {
                let _ = gate.recv();
            }
            if for_thread.is_dir() {
                recover::repair_each(&for_thread)
            } else {
                let e = std::io::Error::new(std::io::ErrorKind::NotFound, "not a folder");
                Outcome { repaired: Vec::new(), failed: vec![(for_thread, e)] }
            }
        });
        match spawned {
            Ok(h) => self.push_repair(h),
            Err(e) => return Err(crate::StudioError::Output(format!("the repair could not start: {e}"))),
        }
        Ok(json!({"started": true, "folder": folder}))
    }

    /// Wait for the repairs started here and take their results as the next tick would, for
    /// tests.
    #[cfg(test)]
    pub(crate) fn audio_tracks_repair_result(&mut self) -> Option<Value> {
        while self.audio_tracks.repairs.iter().any(|(h, _)| !h.is_finished()) {
            std::thread::yield_now();
        }
        self.finish_audio_tracks();
        self.audio_tracks.last_repair.clone()
    }

    /// The audio tracks part of the studio's state, read from atomics.
    pub fn audio_tracks_json(&self) -> Value {
        let s = &self.profile.audio_tracks;
        let mut v = json!({
            "enabled": s.enabled, "when": s.when.id(), "include_program": s.include_program, "buffer_mib": s.buffer_mib,
            "segment_gib": s.segment_gib, "active": self.audio_tracks.take.is_some(), "finishing": self.audio_tracks.finishing.len(),
            "repairing": !self.audio_tracks.repairs.is_empty(), "after_stop": s.after_stop.id(), "editor_path": s.editor_path,
            "last_session": self.audio_tracks.last_session.as_ref().map(|p| p.display().to_string()),
        });
        if let (Some(o), Some(r)) = (v.as_object_mut(), &self.audio_tracks.last_repair) {
            o.insert("last_repair".into(), r.clone());
        }
        if let (Some(o), Some((kind, _))) = (v.as_object_mut(), self.audio_tracks.anchor) {
            o.insert("anchor".into(), json!(if kind == OutputKind::Recording { "recording" } else { "stream" }));
        }
        if let (Some(o), Some((buffer, segment)), true) = (v.as_object_mut(), self.audio_tracks.take_sizes, self.audio_tracks.take.is_some()) {
            // The running take keeps the sizes it started with.
            o.insert("take_buffer_mib".into(), json!(buffer));
            o.insert("take_segment_gib".into(), json!(segment));
        }
        if let (Some(o), Some(st)) = (v.as_object_mut(), &self.audio_tracks.last) {
            let state = match st.state() {
                Some(State::Recording) => "recording",
                Some(State::Finishing) => "finishing",
                Some(State::Done) => "done",
                Some(State::Stopped) => "stopped",
                None => "idle",
            };
            o.insert("state".into(), json!(state));
            if let Some(dir) = st.dir() {
                o.insert("folder".into(), json!(dir.display().to_string()));
            }
            o.insert("tracks".into(), json!(st.tracks()));
            o.insert("seconds".into(), json!(st.seconds()));
            o.insert("bytes".into(), json!(st.bytes()));
            o.insert("dropped_seconds".into(), json!(st.dropped_seconds()));
            o.insert("low_space".into(), json!(st.low_space()));
            o.insert("track_limit".into(), json!(st.track_limit()));
            o.insert("source_limit".into(), json!(self.audio_tracks_source_limit()));
            if let Some(r) = st.reason() {
                o.insert("stopped_at".into(), json!(clock_text(st.stopped_at_seconds())));
                o.insert("reason".into(), json!(r.text()));
            }
        }
        v
    }

    /// The status of the last take, for the status bar; `None` before the first.
    pub fn audio_tracks_status(&self) -> Option<&Arc<studiocraft_audio_tracks::Status>> {
        self.audio_tracks.last.as_ref()
    }

    /// The session file `audio_tracks.open_last` opens: the last take's that ended with one, or
    /// the last one a repair wrote.
    pub fn audio_tracks_last_session(&self) -> Option<&Path> {
        self.audio_tracks.last_session.as_deref()
    }

    /// Whether a take runs, so `audio_tracks.marker` can mark it.
    pub fn audio_tracks_running(&self) -> bool {
        self.audio_tracks.take.is_some()
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

    /// Stop the take and wait for its files, when the app closes, for at most [`CLOSE_WAIT`] in
    /// all. A take still writing then is left to finish on its own threads. When the last take
    /// finished with its session, that session is opened as the "after a take" setting says.
    /// Openings still starting, that one included, are waited for for at most [`OPEN_WAIT`] more.
    pub(crate) fn finish_audio_tracks_blocking(&mut self) {
        self.stop_audio_tracks();
        let deadline = Instant::now() + CLOSE_WAIT;
        let mut last = None;
        let handed_on = std::mem::take(&mut self.audio_tracks.handed_on);
        self.audio_tracks.stop_order.clear();
        for (t, _) in self.audio_tracks.finishing.drain(..) {
            let status = Arc::clone(t.status());
            let finished = t.join_until(deadline);
            if !finished {
                log::warn!("audio tracks: a take was still writing when the app closed; the next launch repairs it if it does not finish");
            }
            // Only the last take opens: one that ended for the next one, or one that did not
            // finish, opens nothing, and neither does an earlier take in its place.
            let next = handed_on.iter().any(|h| Arc::ptr_eq(h, &status));
            last = status.session().filter(|_| finished && !next).map(Path::to_path_buf);
            if let Some(s) = status.session().filter(|_| finished) {
                self.audio_tracks.last_session = Some(s.to_path_buf());
            }
        }
        let after = self.profile.audio_tracks.after_stop;
        if let Some(session) = last
            && after != AfterStop::Nothing
            && let Err(e) = self.start_opening(session, after == AfterStop::OpenEditor)
        {
            log::warn!("audio tracks: the take that ended as the app closed could not be opened: {e}");
        }
        // Openings that are still starting, this one and earlier ones, have their own time, so a
        // take that used up `deadline` still opens.
        let opened_by = Instant::now() + OPEN_WAIT;
        for h in self.audio_tracks.opening.drain(..) {
            while !h.is_finished() && Instant::now() < opened_by {
                std::thread::sleep(Duration::from_millis(10));
            }
            if h.is_finished() {
                if let Ok(Err(e)) = h.join() {
                    log::warn!("audio tracks: a session could not be opened as the app closed: {e}");
                }
            } else {
                log::warn!("audio tracks: the audio editor or file manager was still starting when the app closed");
            }
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
        a.take.is_none() && a.finishing.is_empty() && a.slots.is_empty() && a.program.is_none() && a.repairs.is_empty() && a.generation == 0
    }
}

/// Where a take's folder goes and the stem of its name: beside the recording at `recording`,
/// named after it, else in `folder`, named for the date and time, while only the stream runs. The
/// take's disk thread makes `<stem> Audio`, or `<stem> (n) Audio` when that name is taken.
fn take_place(recording: Option<&Path>, folder: &Path) -> (PathBuf, String) {
    match recording {
        Some(r) => (
            r.parent().map(Path::to_path_buf).unwrap_or_else(|| folder.to_path_buf()),
            r.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "Take".into()),
        ),
        None => (folder.to_path_buf(), format!("Stream {}", chrono::Local::now().format("%Y-%m-%d %H-%M-%S"))),
    }
}

/// `folder` with its links followed, when it is a folder and lies inside `recording` (the
/// recording folder, its links followed too) or is that folder. Takes are written only there.
fn repair_folder(folder: &str, recording: &Path) -> Result<PathBuf, crate::StudioError> {
    let err = crate::StudioError::Output;
    // One metadata read, so a folder that is not there is an error and not a started repair.
    let path = std::fs::canonicalize(folder).map_err(|_| err(format!("{folder} is not a folder")))?;
    if !path.is_dir() {
        return Err(err(format!("{folder} is not a folder")));
    }
    let root = std::fs::canonicalize(recording)
        .map_err(|e| err(format!("the recording folder {} could not be read, so nothing is repaired: {e}", recording.display())))?;
    if !path.starts_with(&root) {
        return Err(err(format!("{folder} is not inside the recording folder {}; only takes there are repaired", recording.display())));
    }
    Ok(path)
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

/// What a repair did, for the status: each take repaired with its files and their seconds, and
/// each take that could not be repaired with why.
fn repair_json(out: &Outcome) -> Value {
    let takes: Vec<Value> = out
        .repaired
        .iter()
        .map(|r| {
            let files: Vec<Value> = r.files.iter().map(|(f, n)| json!({"file": f, "seconds": *n as f64 / 48_000.0})).collect();
            let session = r.session.as_ref().map(|p| p.display().to_string());
            json!({"folder": r.dir.display().to_string(), "files": files, "session": session})
        })
        .collect();
    let failed: Vec<Value> = out.failed.iter().map(|(d, e)| json!({"folder": d.display().to_string(), "error": e.to_string()})).collect();
    json!({"repaired": takes, "failed": failed})
}

/// The notice for an opening that ended: the session opened in the editor, the folder shown with
/// the reason the editor did not start, or the folder shown as asked.
fn opened_notice(opened: &Opened) -> Option<Notice> {
    match opened {
        Opened::Editor { session, .. } => {
            Some(Notice::new("Audio tracks opened in the audio editor: {session}", vec![("session", session.display().to_string())], false))
        }
        Opened::Folder { folder, editor_error: Some(why) } => Some(Notice::new(
            "The audio editor did not start ({error}); the folder {folder} is shown instead",
            vec![("error", why.clone()), ("folder", folder.display().to_string())],
            true,
        )),
        Opened::Folder { folder, editor_error: None } => {
            Some(Notice::new("Audio tracks saved in {folder}", vec![("folder", folder.display().to_string())], false))
        }
    }
}

/// Takes a repair notice names at most; it counts the rest.
const LISTED_TAKES: usize = 3;

/// The first [`LISTED_TAKES`] of `items` joined by `sep`, and how many were left out.
fn listed(items: Vec<String>, sep: &str) -> (String, usize) {
    let more = items.len().saturating_sub(LISTED_TAKES);
    (items.into_iter().take(LISTED_TAKES).collect::<Vec<_>>().join(sep), more)
}

/// The notices for a repair made at launch: one for the takes repaired, and one error for the
/// takes that could not be. Each names at most [`LISTED_TAKES`] takes and counts the rest, since
/// the status bar shows only the latest notice.
fn repair_notices(out: &Outcome) -> Vec<Notice> {
    let mut notices = Vec::new();
    if !out.repaired.is_empty() {
        let (folders, more) = listed(out.repaired.iter().map(|r| r.dir.display().to_string()).collect(), ", ");
        notices.push(if more == 0 {
            Notice::new("Audio tracks repaired after the app closed during a take: {folders}", vec![("folders", folders)], false)
        } else {
            Notice::new(
                "Audio tracks repaired after the app closed during a take: {folders} and {more} more",
                vec![("folders", folders), ("more", more.to_string())],
                false,
            )
        });
    }
    if !out.failed.is_empty() {
        let (takes, more) = listed(out.failed.iter().map(|(dir, e)| format!("{} ({e})", dir.display())).collect(), "; ");
        // The error is shown last, so it also counts the takes that were repaired.
        let repaired = out.repaired.len();
        let mut args = vec![("takes", takes)];
        if more > 0 {
            args.push(("more", more.to_string()));
        }
        if repaired > 0 {
            args.push(("count", repaired.to_string()));
        }
        let template = match (more > 0, repaired > 0) {
            (false, false) => "Audio tracks could not be repaired: {takes}",
            (true, false) => "Audio tracks could not be repaired: {takes} and {more} more",
            (false, true) => "Audio tracks could not be repaired: {takes}. Takes repaired: {count}",
            (true, true) => "Audio tracks could not be repaired: {takes} and {more} more. Takes repaired: {count}",
        };
        notices.push(Notice::new(template, args, true));
    }
    notices
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn take_folders_sit_beside_the_recording_and_are_named_after_it() {
        let base = PathBuf::from("Videos");
        let rec = base.join("Shows").join("2026-11-17 10-00-00.mkv");
        assert_eq!(take_place(Some(&rec), &base), (base.join("Shows"), "2026-11-17 10-00-00".to_string()));
        let (parent, stem) = take_place(None, &base);
        assert!(parent == base && stem.starts_with("Stream "), "{stem}");
        assert_eq!(clock_text(3725.9), "01:02:05");
        assert_eq!(clock_text(f64::NAN), "00:00:00");
    }
    #[test]
    fn a_take_that_launch_repair_cannot_fix_is_shown_to_the_user() {
        let (a, b) = (PathBuf::from("A Audio/Audio Files"), PathBuf::from("B Audio/Audio Files"));
        let out = Outcome {
            repaired: Vec::new(),
            failed: vec![(a.clone(), std::io::Error::other("Mic.wav is missing")), (b.clone(), std::io::Error::other("Desk.wav is missing"))],
        };
        let notices = repair_notices(&out);
        assert_eq!(notices.len(), 1, "every failed take is in one notice: {notices:?}");
        let n = &notices[0];
        assert!(n.error);
        assert_eq!(n.template, "Audio tracks could not be repaired: {takes}");
        let takes = format!("{} (Mic.wav is missing); {} (Desk.wav is missing)", a.display(), b.display());
        assert_eq!(n.args, vec![("takes", takes)]);
        // Many takes: the first three are named and the rest counted, in both notices.
        let dirs: Vec<PathBuf> = (1..=5).map(|i| PathBuf::from(format!("T{i}"))).collect();
        let out = Outcome {
            repaired: dirs.iter().map(|d| recover::Repaired { dir: d.clone(), files: Vec::new(), synced: Vec::new(), session: None }).collect(),
            failed: dirs.iter().map(|d| (d.clone(), std::io::Error::other("bad"))).collect(),
        };
        let notices = repair_notices(&out);
        assert_eq!(notices.len(), 2, "{notices:?}");
        assert_eq!(notices[0].template, "Audio tracks repaired after the app closed during a take: {folders} and {more} more");
        assert_eq!(notices[0].args, vec![("folders", "T1, T2, T3".to_string()), ("more", "2".to_string())]);
        // The error, shown last, also counts the takes repaired.
        assert_eq!(notices[1].template, "Audio tracks could not be repaired: {takes} and {more} more. Takes repaired: {count}");
        assert_eq!(
            notices[1].args,
            vec![("takes", "T1 (bad); T2 (bad); T3 (bad)".to_string()), ("more", "2".to_string()), ("count", "5".to_string())]
        );
        let one = Outcome {
            repaired: vec![recover::Repaired { dir: dirs[0].clone(), files: Vec::new(), synced: Vec::new(), session: None }],
            failed: vec![(dirs[1].clone(), std::io::Error::other("bad"))],
        };
        let notices = repair_notices(&one);
        assert_eq!(notices[1].template, "Audio tracks could not be repaired: {takes}. Takes repaired: {count}");
        assert_eq!(notices[1].args, vec![("takes", "T2 (bad)".to_string()), ("count", "1".to_string())]);
        assert!(repair_notices(&Outcome::default()).is_empty());
    }
}
