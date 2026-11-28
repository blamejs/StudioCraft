//! The go-live check: what would stop the stream from starting, or spoil it, listed before Go Live.
//!
//! [`Studio::golive_json`] lists one item per stream destination that is on, the stream's video
//! encoder, the free space in the recording folder, the sources that are not working and live
//! captions. Each item is `pass`, `problem` or `unknown`, with an English sentence and, for a
//! problem, the id of the command that fixes it. A destination is `pass` only while it is live, or
//! when it was live on an earlier stream of this session with the same server and key; one that
//! has not connected yet is `unknown`.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError, sync_channel};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};
use studiocraft_model::{Destination, KeyRef, Profile, SourceId};
use studiocraft_outputs::Target;
use studiocraft_plugin::{Availability, DestinationState, DestinationStatus, OutputStatus};

use crate::outputs::recording_folder;
use crate::{CaptionState, Notice, Studio};

/// The free space in the recording folder below which the check reports a problem: 10 GB, a value
/// StudioCraft chose (about 70 minutes of a recording at 20 Mb/s).
pub const MIN_FREE_BYTES: u64 = 10_000_000_000;

/// Most sources the check lists one by one; the others are counted in `more_sources`.
pub const MAX_SOURCE_ITEMS: usize = 32;

/// Longest error text, in characters, a sentence quotes from a source or a destination.
const MAX_ERROR_CHARS: usize = 300;

/// How long a check waits for the free space before it reports it as still being read.
const SPACE_WAIT: Duration = Duration::from_millis(250);

/// How often [`Studio::tick`] notes the running stream's destinations.
const DEST_POLL: Duration = Duration::from_secs(1);

/// Most destinations [`GoLive`] remembers from earlier streams.
const MAX_REACHED: usize = 64;

/// The kind of the Live Captions source.
const CAPTIONS_KIND: &str = "captions";

/// An item's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Check {
    Pass,
    Problem,
    /// Nothing shows yet whether it works, such as a destination that has not connected.
    Unknown,
}

impl Check {
    fn id(self) -> &'static str {
        match self {
            Check::Pass => "pass",
            Check::Problem => "problem",
            Check::Unknown => "unknown",
        }
    }
}

/// One line of the check.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    /// `destinations`, `destination:<id>`, `encoder`, `recording_space`, `sources`,
    /// `source:<id>` or `captions`.
    pub id: String,
    pub state: Check,
    /// The sentence in English, with `{name}` placeholders the interface translates.
    pub template: &'static str,
    pub args: Vec<(&'static str, String)>,
    /// The command that fixes a problem; `None` for an item that passes or is unknown, and for a
    /// problem no command fixes.
    pub fix: Option<&'static str>,
    /// Values for the interface: the destination's id, the source's id, the free bytes.
    pub extra: Vec<(&'static str, Value)>,
}

impl Item {
    fn new(id: impl Into<String>, state: Check, template: &'static str, args: Vec<(&'static str, String)>, fix: Option<&'static str>) -> Item {
        let fix = if state == Check::Problem { fix } else { None };
        Item { id: id.into(), state, template, args, fix, extra: Vec::new() }
    }

    fn with(mut self, key: &'static str, value: Value) -> Item {
        self.extra.push((key, value));
        self
    }

    fn json(&self) -> Value {
        let args: Map<String, Value> = self.args.iter().map(|(k, v)| ((*k).to_string(), Value::String(v.clone()))).collect();
        let mut o = Map::new();
        o.insert("id".into(), json!(self.id));
        o.insert("state".into(), json!(self.state.id()));
        o.insert("text".into(), json!(Notice::new(self.template, self.args.clone(), false).text()));
        o.insert("template".into(), json!(self.template));
        o.insert("args".into(), Value::Object(args));
        o.insert("fix".into(), json!(self.fix));
        for (k, v) in &self.extra {
            o.insert((*k).to_string(), v.clone());
        }
        Value::Object(o)
    }
}

/// How a destination ended up on an earlier stream.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Reach {
    Live,
    /// It was retrying or had failed, with the error.
    Failed(String),
}

/// What the free space query gave.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Space {
    Free(u64),
    Failed(String),
    /// The query has not ended yet, for example on a network folder that does not answer.
    Reading,
}

/// Reads the bytes free on the disk holding a folder.
pub(crate) type ReadSpace = Arc<dyn Fn(&Path) -> std::io::Result<u64> + Send + Sync>;

/// Most free space queries that may run at once, each for another folder.
const MAX_QUERIES: usize = 4;

/// A free space query for one folder: still running, or ended with an answer not yet asked for.
enum Query {
    Running(Receiver<std::io::Result<u64>>),
    Done(std::io::Result<u64>),
}

/// The free space query, on a thread of its own so a folder that does not answer does not hold
/// the caller. At most one query runs for a folder, and at most [`MAX_QUERIES`] are kept in all.
pub(crate) struct SpaceProbe {
    read: ReadSpace,
    wait: Duration,
    running: Vec<(PathBuf, Query)>,
}

impl SpaceProbe {
    fn new(read: ReadSpace, wait: Duration) -> SpaceProbe {
        SpaceProbe { read, wait, running: Vec::new() }
    }

    /// The free space on the disk holding `folder`, or on the nearest folder above it that exists.
    pub(crate) fn query(&mut self, folder: &Path) -> Space {
        // Queries for other folders keep their answers until their folder is asked for again.
        for (_, q) in &mut self.running {
            if let Query::Running(rx) = q {
                match rx.try_recv() {
                    Ok(r) => *q = Query::Done(r),
                    Err(TryRecvError::Disconnected) => *q = Query::Done(Err(std::io::Error::other("the free space could not be read"))),
                    Err(TryRecvError::Empty) => {}
                }
            }
        }
        if let Some(at) = self.running.iter().position(|(asked, _)| asked == folder) {
            let (asked, q) = self.running.swap_remove(at);
            let rx = match q {
                Query::Done(r) => return space_of(r),
                Query::Running(rx) => rx,
            };
            return match rx.recv_timeout(self.wait) {
                Ok(r) => space_of(r),
                Err(RecvTimeoutError::Timeout) => {
                    self.running.push((asked, Query::Running(rx)));
                    Space::Reading
                }
                Err(RecvTimeoutError::Disconnected) => Space::Failed("the free space could not be read".into()),
            };
        }
        if self.running.len() >= MAX_QUERIES {
            // An answer nobody asked for again makes room before a running query does.
            match self.running.iter().position(|(_, q)| matches!(q, Query::Done(_))) {
                Some(at) => drop(self.running.remove(at)),
                None => return Space::Reading,
            }
        }
        let (tx, rx) = sync_channel(1);
        let (read, path) = (Arc::clone(&self.read), folder.to_path_buf());
        let spawned = std::thread::Builder::new().name("free space".into()).spawn(move || {
            let _ = tx.send(free_space(read.as_ref(), &path));
        });
        if let Err(e) = spawned {
            return Space::Failed(format!("no thread to read the free space: {e}"));
        }
        match rx.recv_timeout(self.wait) {
            Ok(r) => space_of(r),
            Err(RecvTimeoutError::Timeout) => {
                self.running.push((folder.to_path_buf(), Query::Running(rx)));
                Space::Reading
            }
            Err(RecvTimeoutError::Disconnected) => Space::Failed("the free space could not be read".into()),
        }
    }
}

fn space_of(r: std::io::Result<u64>) -> Space {
    match r {
        Ok(b) => Space::Free(b),
        Err(e) => Space::Failed(e.to_string()),
    }
}

/// Most folders [`free_space`] goes up from a folder that does not exist yet.
const MAX_UP: usize = 64;

/// The free space for `folder`: a recording creates a missing folder, so a missing one is measured
/// on the nearest folder above it that exists. A file in its place is an error.
fn free_space(read: &(dyn Fn(&Path) -> std::io::Result<u64> + Send + Sync), folder: &Path) -> std::io::Result<u64> {
    for p in folder.ancestors().take(MAX_UP) {
        // A relative folder ends in the current folder.
        let p = if p.as_os_str().is_empty() { Path::new(".") } else { p };
        match std::fs::metadata(p) {
            Ok(m) if m.is_dir() => return read(p),
            Ok(_) => return Err(std::io::Error::other(format!("{} is not a folder", p.display()))),
            // A path below a file is not found on Windows and not a folder on Unix; the file is
            // reported when the walk reaches it.
            Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) => {}
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(std::io::ErrorKind::NotFound, format!("no part of {} exists", folder.display())))
}

/// What the check keeps between calls.
pub(crate) struct GoLive {
    /// How each destination ended up on the last stream that sent to it, by id, with the
    /// fingerprint of its server and key then.
    reached: HashMap<String, (u64, Reach)>,
    /// When [`Studio::tick`] last noted the stream's destinations.
    polled: Option<Instant>,
    space: SpaceProbe,
}

impl Default for GoLive {
    fn default() -> GoLive {
        GoLive { reached: HashMap::new(), polled: None, space: SpaceProbe::new(Arc::new(studiocraft_audio_tracks::space::available), SPACE_WAIT) }
    }
}

impl GoLive {
    /// Note the outcome of each destination of the running stream, which started with `prints`,
    /// each destination's fingerprint by id.
    pub(crate) fn record(&mut self, states: &[DestinationState], prints: &HashMap<String, u64>) {
        for d in states {
            let Some(&print) = prints.get(&d.id) else { continue };
            let reach = match &d.status {
                DestinationStatus::Live => Reach::Live,
                DestinationStatus::Retrying { failure, .. }
                | DestinationStatus::WaitingForNetwork { failure, .. }
                | DestinationStatus::Failed(failure) => Reach::Failed(failure.detail.clone()),
                DestinationStatus::Connecting | DestinationStatus::Stopped => continue,
            };
            if self.reached.len() >= MAX_REACHED && !self.reached.contains_key(&d.id) {
                self.reached.retain(|id, _| prints.contains_key(id));
                if self.reached.len() >= MAX_REACHED {
                    continue;
                }
            }
            self.reached.insert(d.id.clone(), (print, reach));
        }
    }

    #[cfg(test)]
    pub(crate) fn set_space_reader(&mut self, read: ReadSpace, wait: Duration) {
        self.space = SpaceProbe::new(read, wait);
    }
}

/// The key destination `d` of profile `p` sends, as a start picks it: `""` for none, the profile's
/// key even when empty (the server's address may end with the stream's name), and `None` for a
/// held key not set for this session.
pub(crate) fn destination_key<'a>(p: &'a Profile, held: &'a HashMap<String, String>, d: &Destination) -> Option<&'a str> {
    match &d.key {
        KeyRef::None => Some(""),
        KeyRef::Profile => Some(p.stream.key.as_str()),
        KeyRef::Held(name) => held.get(name).map(String::as_str),
    }
}

/// A fingerprint of a destination's server and key, so a destination that was live is not
/// counted as live after either changes. It is kept in memory only.
pub(crate) fn print(server: &str, key: &str) -> u64 {
    let mut h = DefaultHasher::new();
    server.trim().hash(&mut h);
    key.hash(&mut h);
    h.finish()
}

/// The fingerprint of each destination of `p` whose key is there, by id.
pub(crate) fn prints(p: &Profile, held: &HashMap<String, String>) -> HashMap<String, u64> {
    p.stream.destinations.iter().filter_map(|d| destination_key(p, held, d).map(|k| (d.id.clone(), print(&d.server, k)))).collect()
}

/// `text` cut to [`MAX_ERROR_CHARS`] characters.
fn clip(text: &str) -> String {
    match text.char_indices().nth(MAX_ERROR_CHARS) {
        Some((i, _)) => format!("{}…", &text[..i]),
        None => text.to_string(),
    }
}

/// Bytes as gigabytes with one decimal.
fn gigabytes(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1e9)
}

/// What the check reads from the studio.
pub(crate) struct Input<'a> {
    pub profile: &'a Profile,
    pub held: &'a HashMap<String, String>,
    /// The destinations of the running stream, when it runs with this profile.
    pub running: Option<&'a [DestinationState]>,
    /// The fingerprint of each running destination's server and key, by id.
    pub prints: &'a HashMap<String, u64>,
    pub reached: &'a HashMap<String, (u64, Reach)>,
    /// The stream's encoder: its name and availability, or why it cannot start.
    pub encoder: Result<(String, Availability), String>,
    /// Whether the studio draws with a graphics device, which every stream needs.
    pub video: bool,
    pub folder: &'a Path,
    pub space: Space,
    /// Each source that is not working: id, name, and why.
    pub broken: Vec<(SourceId, String, String)>,
    /// Sources whose plug-in is not installed: id, name and kind.
    pub missing: Vec<(SourceId, String, String)>,
    pub captions: CaptionState,
    /// Whether the scene collection has a Live Captions source.
    pub caption_source: bool,
    /// Whether this build recognizes speech.
    pub speech: bool,
}

/// The check's items, and how many sources were left out of the list.
pub(crate) fn items(i: &Input) -> (Vec<Item>, usize) {
    let mut out = Vec::new();
    destinations(i, &mut out);
    encoder(i, &mut out);
    space(i, &mut out);
    let more = sources(i, &mut out);
    captions(i, &mut out);
    (out, more)
}

fn destinations(i: &Input, out: &mut Vec<Item>) {
    const FIX: Option<&str> = Some("view.streaming");
    let s = &i.profile.stream;
    if let Err(e) = s.check() {
        out.push(Item::new("destinations", Check::Problem, "The stream destinations cannot be used: {error}", vec![("error", e)], FIX));
        return;
    }
    let on: Vec<&Destination> = s.destinations.iter().filter(|d| d.enabled).collect();
    if on.is_empty() {
        out.push(Item::new("destinations", Check::Problem, "No stream destination is turned on", vec![], FIX));
        return;
    }
    for d in on {
        let name = if d.name.trim().is_empty() { d.id.clone() } else { d.name.clone() };
        let item = |state, template, mut args: Vec<(&'static str, String)>| {
            args.insert(0, ("name", name.clone()));
            Item::new(format!("destination:{}", d.id), state, template, args, FIX).with("destination", json!(d.id))
        };
        let key = destination_key(i.profile, i.held, d);
        // A destination of the running stream sends with the server and key it started with, so
        // a change to them shows from its next start.
        let sending = i.running.and_then(|r| r.iter().find(|r| r.id == d.id));
        let current = key.is_some_and(|k| i.prints.get(&d.id) == Some(&print(&d.server, k)));
        if sending.is_some() && !current {
            out.push(item(
                Check::Unknown,
                "Destination {name} was changed after the stream started, and sends the change from its next start",
                vec![],
            ));
            continue;
        }
        if d.server.trim().is_empty() {
            out.push(item(Check::Problem, "Destination {name} has no server", vec![]));
            continue;
        }
        let Some(key) = key else {
            out.push(item(Check::Problem, "The stream key of destination {name} is not entered for this session", vec![]));
            continue;
        };
        let target = Target { id: d.id.clone(), server: d.server.clone(), key: key.to_string(), enabled: true, encoder: None };
        if let Err(e) = target.check() {
            // Without a key, the address must end with the stream's name; the key is named as
            // missing only when the address works with one.
            let keyless = key.trim().is_empty() && matches!(d.key, KeyRef::Profile);
            let error = if keyless { Target { key: "key".into(), ..target }.check().err() } else { Some(e) };
            out.push(match error {
                None => item(Check::Problem, "Destination {name} has no stream key", vec![]),
                Some(e) => item(Check::Problem, "Destination {name} cannot be used: {error}", vec![("error", clip(&e))]),
            });
            continue;
        }
        if i.running.is_some() {
            out.push(match sending.map(|r| &r.status) {
                Some(DestinationStatus::Live) => item(Check::Pass, "Destination {name} is live", vec![]),
                Some(DestinationStatus::Connecting) => item(Check::Unknown, "Destination {name} is connecting", vec![]),
                Some(DestinationStatus::Retrying { failure, .. }) => {
                    item(Check::Problem, "Destination {name} is reconnecting: {error}", vec![("error", clip(&failure.detail))])
                }
                Some(DestinationStatus::WaitingForNetwork { failure, .. }) => {
                    item(Check::Problem, "Destination {name} is waiting for the network: {error}", vec![("error", clip(&failure.detail))])
                }
                Some(DestinationStatus::Failed(failure)) => {
                    item(Check::Problem, "Destination {name} failed: {error}", vec![("error", clip(&failure.detail))])
                }
                Some(DestinationStatus::Stopped) => item(Check::Problem, "Destination {name} is turned on but not sending", vec![]),
                None => item(Check::Unknown, "Destination {name} is not part of the running stream", vec![]),
            });
            continue;
        }
        out.push(match i.reached.get(&d.id) {
            Some((p, Reach::Live)) if *p == print(&d.server, key) => item(Check::Pass, "Destination {name} was live on the last stream", vec![]),
            Some((p, Reach::Failed(error))) if *p == print(&d.server, key) => {
                item(Check::Problem, "Destination {name} failed on the last stream: {error}", vec![("error", clip(error))])
            }
            _ => item(Check::Unknown, "Destination {name} has not connected yet", vec![]),
        });
    }
}

fn encoder(i: &Input, out: &mut Vec<Item>) {
    const FIX: Option<&str> = Some("app.settings");
    if !i.video {
        out.push(Item::new("encoder", Check::Problem, "Nothing can be streamed without the graphics device", vec![], None));
        return;
    }
    out.push(match &i.encoder {
        Ok((name, Availability::Ready)) => {
            Item::new("encoder", Check::Pass, "The stream encodes with {encoder}", vec![("encoder", name.clone())], FIX)
        }
        Ok((name, _)) => {
            Item::new("encoder", Check::Unknown, "The encoder {encoder} is tested when the stream starts", vec![("encoder", name.clone())], FIX)
        }
        Err(e) => Item::new("encoder", Check::Problem, "The stream cannot start: {error}", vec![("error", e.clone())], FIX),
    });
}

fn space(i: &Input, out: &mut Vec<Item>) {
    const FIX: Option<&str> = Some("app.settings");
    let folder = i.folder.display().to_string();
    out.push(match &i.space {
        Space::Free(b) if *b < MIN_FREE_BYTES => Item::new(
            "recording_space",
            Check::Problem,
            "Only {free} is free for recordings in {folder}",
            vec![("free", gigabytes(*b)), ("folder", folder)],
            FIX,
        )
        .with("free_bytes", json!(b)),
        Space::Free(b) => Item::new(
            "recording_space",
            Check::Pass,
            "{free} is free for recordings in {folder}",
            vec![("free", gigabytes(*b)), ("folder", folder)],
            FIX,
        )
        .with("free_bytes", json!(b)),
        Space::Failed(e) => Item::new(
            "recording_space",
            Check::Problem,
            "The free space in the recording folder {folder} could not be read: {error}",
            vec![("folder", folder), ("error", clip(e))],
            FIX,
        ),
        Space::Reading => {
            Item::new("recording_space", Check::Unknown, "The free space in {folder} is still being read", vec![("folder", folder)], FIX)
        }
    });
}

/// The source items; returns how many were left out past [`MAX_SOURCE_ITEMS`].
fn sources(i: &Input, out: &mut Vec<Item>) -> usize {
    let total = i.missing.len() + i.broken.len();
    if total == 0 {
        out.push(Item::new("sources", Check::Pass, "Every source is working", vec![], None));
        return 0;
    }
    let missing = i.missing.iter().map(|(id, name, kind)| {
        Item::new(
            format!("source:{}", id.0),
            Check::Problem,
            "The source {name} needs a plug-in that is not installed: {kind}",
            vec![("name", name.clone()), ("kind", kind.clone())],
            None,
        )
        .with("source", json!(id.0))
    });
    let broken = i.broken.iter().map(|(id, name, error)| {
        Item::new(
            format!("source:{}", id.0),
            Check::Problem,
            "The source {name} is not working: {error}",
            vec![("name", name.clone()), ("error", clip(error))],
            Some("selection.properties"),
        )
        .with("source", json!(id.0))
    });
    out.extend(missing.chain(broken).take(MAX_SOURCE_ITEMS));
    total.saturating_sub(MAX_SOURCE_ITEMS)
}

fn captions(i: &Input, out: &mut Vec<Item>) {
    const FIX: Option<&str> = Some("captions.toggle");
    let item = |state, template, args| Item::new("captions", state, template, args, FIX);
    out.push(match &i.captions {
        CaptionState::Running => item(Check::Pass, "Live captions are running", vec![]),
        CaptionState::Loading => item(Check::Unknown, "Live captions are starting", vec![]),
        _ if !i.caption_source => item(Check::Pass, "Live captions are not used", vec![]),
        _ if !i.speech => {
            Item::new("captions", Check::Problem, "A Live Captions source is in the scenes, and this build has no speech recognition", vec![], None)
        }
        CaptionState::Failed(e) => item(Check::Problem, "Live captions stopped: {error}", vec![("error", clip(e))]),
        CaptionState::Stopping | CaptionState::Stopped => {
            item(Check::Problem, "Live captions are off, and a Live Captions source is in the scenes", vec![])
        }
    });
}

impl Studio {
    /// The go-live check: each item with its state and the command that fixes a problem.
    pub fn golive_json(&mut self) -> Value {
        let profile = &self.profile;
        let running = self.outputs.stream_destinations_of(profile.name.as_str());
        let encoder = crate::outputs::stream_encoder(self.engine.registry(), profile).map_err(|e| e.to_string());
        let folder = recording_folder(&profile.output.folder);
        let space = self.golive.space.query(&folder);
        let registry = self.engine.registry();
        let mut broken = Vec::new();
        let mut missing = Vec::new();
        for s in &self.engine.collection().sources {
            if registry.source(&s.kind).is_err() {
                missing.push((s.id, s.name.clone(), s.kind.clone()));
            } else if let Some(e) = self.sources.error(s.id) {
                broken.push((s.id, s.name.clone(), e));
            } else if self.sources.io(s.id).is_none() {
                broken.push((s.id, s.name.clone(), "it did not start".into()));
            }
        }
        let input = Input {
            profile,
            held: self.outputs.held_keys(),
            running: running.as_deref(),
            prints: self.outputs.stream_prints(),
            reached: &self.golive.reached,
            encoder,
            video: self.video.is_some(),
            folder: &folder,
            space,
            broken,
            missing,
            captions: self.captions_state(),
            caption_source: self.engine.collection().sources.iter().any(|s| s.kind == CAPTIONS_KIND),
            speech: craft_speech::available(),
        };
        let (items, more) = items(&input);
        let count = |c: Check| items.iter().filter(|it| it.state == c).count();
        json!({
            "ready": items.iter().all(|it| it.state == Check::Pass),
            "problems": count(Check::Problem),
            "unknown": count(Check::Unknown),
            "more_sources": more,
            "items": items.iter().map(Item::json).collect::<Vec<_>>(),
        })
    }

    /// Note how the running stream's destinations are doing, at most once a [`DEST_POLL`], and at
    /// once when the stream has failed, before it is stopped.
    pub(crate) fn note_destinations(&mut self) {
        let Some(status) = self.outputs.stream_status() else { return };
        if !matches!(status, OutputStatus::Failed(_)) && self.golive.polled.is_some_and(|t| t.elapsed() < DEST_POLL) {
            return;
        }
        self.golive.polled = Some(Instant::now());
        self.record_destinations();
    }

    /// Note how the running stream's destinations are doing now; a stop calls this first.
    pub(crate) fn record_destinations(&mut self) {
        let states = self.stream_destinations();
        self.golive.record(&states, self.outputs.stream_prints());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;
    use studiocraft_plugin::{Cause, Failure};

    fn dest(id: &str, server: &str, key: KeyRef, enabled: bool) -> Destination {
        Destination { id: id.into(), enabled, server: server.into(), key, ..Destination::default() }
    }

    fn state(id: &str, status: DestinationStatus) -> DestinationState {
        DestinationState { id: id.into(), target: String::new(), status, bytes: 0, dropped: 0, reconnects: 0, next_try: None, max_tries: 10 }
    }

    struct Fixture {
        profile: Profile,
        held: HashMap<String, String>,
        /// The fingerprints the running stream started with: those of the profile as made.
        prints: HashMap<String, u64>,
        reached: HashMap<String, (u64, Reach)>,
    }

    impl Fixture {
        fn new(destinations: Vec<Destination>) -> Fixture {
            let mut profile = Profile::default();
            profile.stream.destinations = destinations;
            profile.stream.key = "profile-key".into();
            let held = HashMap::new();
            let prints = prints(&profile, &held);
            Fixture { profile, held, prints, reached: HashMap::new() }
        }

        fn input<'a>(&'a self, running: Option<&'a [DestinationState]>) -> Input<'a> {
            Input {
                profile: &self.profile,
                held: &self.held,
                running,
                prints: &self.prints,
                reached: &self.reached,
                encoder: Ok(("Built-in H.264".into(), Availability::Ready)),
                video: true,
                folder: Path::new("/videos"),
                space: Space::Free(MIN_FREE_BYTES),
                broken: vec![],
                missing: vec![],
                captions: CaptionState::Stopped,
                caption_source: false,
                speech: true,
            }
        }
    }

    fn find<'a>(items: &'a [Item], id: &str) -> &'a Item {
        items.iter().find(|i| i.id == id).unwrap_or_else(|| panic!("no item {id} in {items:?}"))
    }

    #[test]
    fn a_destination_that_never_connected_is_unknown_and_never_passes() {
        let f = Fixture::new(vec![dest("main", "rtmp://a/live", KeyRef::Profile, true)]);
        let list = items_of(&f);
        let d = find(&list, "destination:main");
        assert_eq!(d.state, Check::Unknown, "{d:?}");
        assert_eq!(d.fix, None);
        assert_eq!(d.extra, vec![("destination", json!("main"))]);
        // Connecting on a running stream is still unknown.
        let running = [state("main", DestinationStatus::Connecting)];
        let (list, _) = items(&f.input(Some(&running)));
        assert_eq!(find(&list, "destination:main").state, Check::Unknown);
    }

    #[test]
    fn a_destination_passes_while_live_and_after_it_was_live_with_the_same_server_and_key() {
        let mut f = Fixture::new(vec![dest("main", "rtmp://a/live", KeyRef::Profile, true)]);
        let running = [state("main", DestinationStatus::Live)];
        let (list, _) = items(&f.input(Some(&running)));
        assert_eq!(find(&list, "destination:main").state, Check::Pass);

        let mut g = GoLive::default();
        g.record(&running, &prints(&f.profile, &f.held));
        f.reached = g.reached.clone();
        let list = items_of(&f);
        let d = find(&list, "destination:main");
        assert_eq!((d.state, d.template), (Check::Pass, "Destination {name} was live on the last stream"));
        // A new key or a new server makes it unknown again, also while the stream still sends
        // with the old one.
        f.profile.stream.key = "other".into();
        assert_eq!(find(&items_of(&f), "destination:main").state, Check::Unknown);
        let (list, _) = items(&f.input(Some(&running)));
        let d = find(&list, "destination:main");
        assert_eq!((d.state, d.fix), (Check::Unknown, None), "{d:?}");
        // So does a key cleared while the stream sends with the old one.
        f.profile.stream.key = String::new();
        let (list, _) = items(&f.input(Some(&running)));
        assert_eq!(find(&list, "destination:main").state, Check::Unknown);
        assert_eq!(find(&items_of(&f), "destination:main").state, Check::Problem, "with no stream it cannot start");
        // An address a start refuses is a problem before any stream.
        f.profile.stream.key = "profile-key".into();
        f.profile.stream.destinations[0].server = "not-a-url".into();
        let list = items_of(&f);
        let d = find(&list, "destination:main");
        assert_eq!((d.state, d.template), (Check::Problem, "Destination {name} cannot be used: {error}"), "{d:?}");
        f.profile.stream.destinations[0].server = "rtmp://b/live".into();
        assert_eq!(find(&items_of(&f), "destination:main").state, Check::Unknown);
        f.profile.stream.destinations[0].server = " rtmp://a/live ".into();
        assert_eq!(find(&items_of(&f), "destination:main").state, Check::Pass, "spaces around the server are not a change");
    }

    fn items_of(f: &Fixture) -> Vec<Item> {
        items(&f.input(None)).0
    }

    #[test]
    fn a_destination_that_failed_or_is_not_sending_is_a_problem_fixed_in_the_streaming_window() {
        let mut f = Fixture::new(vec![
            dest("a", "rtmp://a/live/name", KeyRef::None, true),
            dest("b", "rtmp://b/live/name", KeyRef::None, true),
            dest("c", "rtmp://c/live/name", KeyRef::None, true),
            dest("d", "rtmp://d/live/name", KeyRef::None, true),
            dest("e", "rtmp://e/live/name", KeyRef::None, true),
        ]);
        let running = [
            state("a", DestinationStatus::Retrying { attempt: 2, failure: Failure::new(Cause::Refused, "refused") }),
            state("b", DestinationStatus::Failed(Failure::new(Cause::RejectedPublish, "bad key"))),
            state("c", DestinationStatus::Stopped),
            state("e", DestinationStatus::WaitingForNetwork { attempt: 2, failure: Failure::new(Cause::NoRoute, "no route") }),
        ];
        let (list, _) = items(&f.input(Some(&running)));
        assert_eq!(find(&list, "destination:e").template, "Destination {name} is waiting for the network: {error}");
        for id in ["a", "b", "c", "e"] {
            let d = find(&list, &format!("destination:{id}"));
            assert_eq!((d.state, d.fix), (Check::Problem, Some("view.streaming")), "{d:?}");
        }
        assert_eq!(find(&list, "destination:d").state, Check::Unknown, "added after the stream started");
        let mut g = GoLive::default();
        g.record(&running, &prints(&f.profile, &f.held));
        f.reached = g.reached;
        let after = items_of(&f);
        assert_eq!(find(&after, "destination:a").state, Check::Problem, "it was retrying when last seen");
        assert_eq!(find(&after, "destination:e").state, Check::Problem, "it was waiting for the network when last seen");
        assert_eq!(find(&after, "destination:b").args, vec![("name", "b".into()), ("error", "bad key".into())]);
        assert_eq!(find(&after, "destination:c").state, Check::Unknown, "a stopped destination says nothing about its connection");
    }

    #[test]
    fn a_missing_server_or_key_is_a_problem_and_a_destination_that_is_off_is_not_listed() {
        let mut f = Fixture::new(vec![
            dest("noserver", " ", KeyRef::None, true),
            dest("nokey", "rtmp://a/live", KeyRef::Profile, true),
            dest("held", "rtmp://a/live", KeyRef::Held("h".into()), true),
            dest("off", "", KeyRef::Profile, false),
        ]);
        f.profile.stream.key.clear();
        f.profile.stream.destinations[0].name = "Own server".into();
        let items = items_of(&f);
        let d = find(&items, "destination:noserver");
        assert_eq!((d.state, d.template), (Check::Problem, "Destination {name} has no server"));
        assert_eq!(d.args, vec![("name", "Own server".into())], "the name is shown when there is one");
        assert_eq!(find(&items, "destination:nokey").template, "Destination {name} has no stream key");
        assert_eq!(find(&items, "destination:held").template, "The stream key of destination {name} is not entered for this session");
        assert!(items.iter().all(|i| i.id != "destination:off"));
        f.held.insert("h".into(), "secret".into());
        assert_eq!(find(&items_of(&f), "destination:held").state, Check::Unknown);
        // An address that ends with the stream's name needs no key, as a start reads it.
        f.profile.stream.destinations[1].server = "rtmp://a/live/name".into();
        assert_eq!(find(&items_of(&f), "destination:nokey").state, Check::Unknown);
        // A bad address is named rather than the missing key, since a key would not fix it.
        f.profile.stream.destinations[1].server = "http://a/live".into();
        assert_eq!(find(&items_of(&f), "destination:nokey").template, "Destination {name} cannot be used: {error}");
    }

    #[test]
    fn no_destination_or_a_bad_list_is_one_problem() {
        let f = Fixture::new(vec![dest("off", "rtmp://a/live", KeyRef::None, false)]);
        let items = items_of(&f);
        let d = find(&items, "destinations");
        assert_eq!((d.state, d.template, d.fix), (Check::Problem, "No stream destination is turned on", Some("view.streaming")));
        let f = Fixture::new(vec![dest("x", "rtmp://a/live", KeyRef::None, true), dest("x", "rtmp://b/live", KeyRef::None, true)]);
        let items = items_of(&f);
        assert_eq!(find(&items, "destinations").template, "The stream destinations cannot be used: {error}");
        assert!(items.iter().all(|i| !i.id.starts_with("destination:")));
    }

    #[test]
    fn the_encoder_passes_only_when_it_can_run() {
        let f = Fixture::new(vec![]);
        let mut i = f.input(None);
        assert_eq!(find(&items(&i).0, "encoder").state, Check::Pass);
        i.encoder = Ok(("Hardware".into(), Availability::Unknown));
        assert_eq!(find(&items(&i).0, "encoder").state, Check::Unknown);
        i.encoder = Err("there is no encoder `nope`".into());
        let e = items(&i).0;
        let e = find(&e, "encoder");
        assert_eq!((e.state, e.fix), (Check::Problem, Some("app.settings")));
        // Without the graphics device nothing is streamed, whatever the encoder.
        i.encoder = Ok(("Built-in H.264".into(), Availability::Ready));
        i.video = false;
        let e = items(&i).0;
        let e = find(&e, "encoder");
        assert_eq!((e.state, e.fix, e.template), (Check::Problem, None, "Nothing can be streamed without the graphics device"));
    }

    #[test]
    fn the_recording_space_is_a_problem_below_ten_gigabytes_or_when_unreadable() {
        let f = Fixture::new(vec![]);
        let mut i = f.input(None);
        let pass = items(&i).0;
        assert_eq!(find(&pass, "recording_space").state, Check::Pass, "exactly the limit passes");
        assert_eq!(find(&pass, "recording_space").extra, vec![("free_bytes", json!(MIN_FREE_BYTES))]);
        i.space = Space::Free(MIN_FREE_BYTES - 1);
        let low = items(&i).0;
        let low = find(&low, "recording_space");
        assert_eq!((low.state, low.fix), (Check::Problem, Some("app.settings")));
        assert_eq!(low.args[0], ("free", "10.0 GB".into()));
        i.space = Space::Failed("denied".into());
        assert_eq!(find(&items(&i).0, "recording_space").state, Check::Problem);
        i.space = Space::Reading;
        assert_eq!(find(&items(&i).0, "recording_space").state, Check::Unknown);
    }

    #[test]
    fn sources_that_do_not_work_are_listed_up_to_the_limit() {
        let f = Fixture::new(vec![]);
        let mut i = f.input(None);
        let ok = items(&i).0;
        assert_eq!(find(&ok, "sources").state, Check::Pass);
        let long = "x".repeat(MAX_ERROR_CHARS + 50);
        i.broken = (1..=40).map(|n| (SourceId(n), format!("Window {n}"), long.clone())).collect();
        i.missing = vec![(SourceId(99), "Old".into(), "gone_kind".into())];
        let (list, more) = items(&i);
        let sources: Vec<&Item> = list.iter().filter(|it| it.id.starts_with("source:")).collect();
        assert_eq!((sources.len(), more), (MAX_SOURCE_ITEMS, 9));
        assert_eq!((sources[0].id.as_str(), sources[0].fix), ("source:99", None), "no command installs a plug-in");
        assert_eq!(sources[1].fix, Some("selection.properties"));
        assert_eq!(sources[1].extra, vec![("source", json!(1))]);
        assert_eq!(sources[1].args[1].1.chars().count(), MAX_ERROR_CHARS + 1, "the error is cut, with an ellipsis");
        assert!(list.iter().all(|it| it.id != "sources"));
    }

    #[test]
    fn captions_that_are_off_are_a_problem_only_with_a_live_captions_source() {
        let f = Fixture::new(vec![]);
        let mut i = f.input(None);
        assert_eq!(find(&items(&i).0, "captions").template, "Live captions are not used");
        i.caption_source = true;
        let off = items(&i).0;
        let off = find(&off, "captions");
        assert_eq!((off.state, off.fix), (Check::Problem, Some("captions.toggle")));
        i.captions = CaptionState::Failed("no model".into());
        assert_eq!(find(&items(&i).0, "captions").template, "Live captions stopped: {error}");
        i.captions = CaptionState::Loading;
        assert_eq!(find(&items(&i).0, "captions").state, Check::Unknown);
        i.captions = CaptionState::Running;
        assert_eq!(find(&items(&i).0, "captions").state, Check::Pass);
        i.captions = CaptionState::Stopped;
        i.speech = false;
        let none = items(&i).0;
        assert_eq!((find(&none, "captions").state, find(&none, "captions").fix), (Check::Problem, None));
    }

    #[test]
    fn remembered_destinations_are_bounded() {
        let mut g = GoLive::default();
        let ids: Vec<String> = (0..MAX_REACHED + 10).map(|n| format!("d{n}")).collect();
        let prints: HashMap<String, u64> = ids.iter().map(|id| (id.clone(), 1)).collect();
        let states: Vec<DestinationState> = ids.iter().map(|id| state(id, DestinationStatus::Live)).collect();
        g.record(&states, &prints);
        assert_eq!(g.reached.len(), MAX_REACHED);
        // Destinations of the next stream replace the ones it does not have.
        let next: HashMap<String, u64> = [("new".to_string(), 2)].into();
        g.record(&[state("new", DestinationStatus::Live)], &next);
        assert_eq!(g.reached.len(), 1);
        assert_eq!(g.reached.get("new"), Some(&(2, Reach::Live)));
    }

    #[test]
    fn the_free_space_is_read_on_its_own_thread_and_a_slow_folder_reads_as_still_being_read() {
        let dir = std::env::temp_dir();
        let (release_tx, release_rx) = channel::<()>();
        let release_rx = std::sync::Mutex::new(release_rx);
        let read: ReadSpace = Arc::new(move |_p: &Path| {
            let _ = release_rx.lock().map(|r| r.recv());
            Ok(42)
        });
        let mut probe = SpaceProbe::new(read, Duration::ZERO);
        assert_eq!(probe.query(&dir), Space::Reading);
        assert_eq!(probe.query(&dir), Space::Reading, "a second check does not start a second query");
        release_tx.send(()).unwrap();
        probe.wait = ANSWER;
        assert_eq!(probe.query(&dir), Space::Free(42));
    }

    /// A wait with no time limit: `recv_timeout` falls back to `recv` when the deadline does not
    /// fit, so a test waits for the query's answer itself.
    const ANSWER: Duration = Duration::MAX;

    #[test]
    fn the_stream_encoder_must_be_a_built_in_h264_encoder() {
        let ffmpeg: studiocraft_sources::FfmpegSetting = Arc::new(std::sync::RwLock::new(String::new()));
        let registry = crate::builtin_registry(&ffmpeg, &Arc::new(crate::CaptionBoard::new())).unwrap();
        let mut p = Profile::default();
        let check = |p: &Profile| crate::outputs::stream_encoder(&registry, p).map(|(_, a)| a).map_err(|e| e.to_string());
        assert_eq!(check(&p), Ok(Availability::Ready), "the default built-in H.264");
        p.stream.video_encoder = "hardware".into();
        assert_eq!(check(&p), Ok(Availability::Unknown));
        for (id, said) in [("nvenc_native_hevc", "H.264"), ("x264", "ffmpeg"), ("nope", "nope")] {
            p.stream.video_encoder = id.into();
            let e = check(&p).unwrap_err();
            assert!(e.contains(said), "{id}: {e}");
        }
    }

    #[test]
    fn an_answer_for_another_folder_is_kept_until_that_folder_is_checked_again() {
        let read: ReadSpace = Arc::new(|_p: &Path| Ok(5));
        let mut probe = SpaceProbe::new(read, ANSWER);
        let (a, b) = (PathBuf::from("a-folder"), std::env::temp_dir());
        let (tx, rx) = sync_channel(1);
        probe.running.push((a.clone(), Query::Running(rx)));
        tx.send(Ok(9)).unwrap();
        assert_eq!(probe.query(&b), Space::Free(5));
        probe.wait = Duration::ZERO;
        assert_eq!(probe.query(&a), Space::Free(9), "the answer that came while another folder was checked");
    }

    #[test]
    fn a_folder_that_does_not_answer_does_not_hold_the_check_of_another() {
        let slow = std::env::temp_dir().join(format!("studiocraft-golive-slow-{}", std::process::id()));
        std::fs::create_dir_all(&slow).unwrap();
        let (release_tx, release_rx) = channel::<()>();
        let release_rx = std::sync::Mutex::new(release_rx);
        let (entered_tx, entered_rx) = channel::<()>();
        let held = slow.clone();
        let read: ReadSpace = Arc::new(move |p: &Path| {
            if p == held {
                // A missing folder below `slow` is measured on `slow`, so its query waits here too.
                let _ = release_rx.lock().map(|r| {
                    let _ = entered_tx.send(());
                    r.recv()
                });
            }
            Ok(7)
        });
        let mut probe = SpaceProbe::new(read, Duration::ZERO);
        // A query for a missing folder below `slow` is the first to wait for the release.
        assert_eq!(probe.query(&slow.join("missing-first")), Space::Reading);
        entered_rx.recv().unwrap();
        assert_eq!(probe.query(&slow), Space::Reading);
        probe.wait = ANSWER;
        assert_eq!(probe.query(&std::env::temp_dir()), Space::Free(7), "another folder is read at once");
        // Queries for other folders stop at the limit while the slow one runs.
        probe.wait = Duration::ZERO;
        let others: Vec<PathBuf> = (0..MAX_QUERIES + 2).map(|n| slow.join(format!("missing-{n}"))).collect();
        let _ = others.iter().map(|o| probe.query(o)).count();
        assert!(probe.running.len() <= MAX_QUERIES, "{}", probe.running.len());
        // Dropping the sender releases every query that waits, the missing folders' included.
        drop(release_tx);
        probe.wait = ANSWER;
        assert_eq!(probe.query(&slow), Space::Free(7), "every waiting query is released");
        std::fs::remove_dir(&slow).unwrap();
    }

    #[test]
    fn a_missing_folder_is_measured_above_it_and_a_failing_reader_is_an_error() {
        let base = std::env::temp_dir().join(format!("studiocraft-golive-{}", std::process::id()));
        let temp = std::env::temp_dir();
        let read: ReadSpace = Arc::new(move |p: &Path| Ok(if p == temp { 1 } else { 2 }));
        let missing = base.join("not").join("here");
        assert_eq!(free_space(read.as_ref(), &missing).ok(), Some(1), "measured on the folder that exists");
        let relative: ReadSpace = Arc::new(|p: &Path| Ok(if p == Path::new(".") { 3 } else { 4 }));
        let name = format!("studiocraft-golive-missing-{}", std::process::id());
        assert_eq!(free_space(relative.as_ref(), Path::new(&name)).ok(), Some(3), "a relative folder is measured on the current folder");
        std::fs::write(&base, b"x").unwrap();
        let e = free_space(read.as_ref(), &base.join("inside")).unwrap_err();
        assert!(e.to_string().contains("is not a folder"), "{e}");
        std::fs::remove_file(&base).unwrap();
        let panics: ReadSpace = Arc::new(|_p: &Path| panic!("a reader that panics"));
        let mut probe = SpaceProbe::new(panics, ANSWER);
        assert!(matches!(probe.query(&std::env::temp_dir()), Space::Failed(_)));
        let fails: ReadSpace = Arc::new(|_p: &Path| Err(std::io::Error::other("denied")));
        let mut probe = SpaceProbe::new(fails, ANSWER);
        assert_eq!(probe.query(&std::env::temp_dir()), Space::Failed("denied".into()));
    }
}
