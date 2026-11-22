//! What happened in the program during the session: scene switches, stream reconnects, pauses and
//! resumes, for `session.events`.
//!
//! The studio's own thread writes each event as it carries out the change, into a ring that holds
//! the last [`CAPACITY`] events. The ring's memory is reserved when the first event arrives, and a
//! full ring drops its oldest event, so a write never waits and never grows the ring. Names are
//! cut to [`MAX_NAME_CHARS`] characters, so each event stays small.

use std::collections::{HashMap, VecDeque};

use std::time::Duration;

use serde_json::{Value, json};
use studiocraft_media::Tick;
use studiocraft_media::clock::tick_of;
use studiocraft_model::SceneId;
use studiocraft_plugin::{DestinationState, DestinationStatus};

/// How many events the ring keeps, StudioCraft's own value.
pub(crate) const CAPACITY: usize = 1_000;

/// The longest scene name or destination id an event keeps, in characters.
pub(crate) const MAX_NAME_CHARS: usize = 256;

/// What happened.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Kind {
    /// The program switched from one scene to another. A scene that was removed has no name.
    Scene { from: (SceneId, Option<String>), to: (SceneId, Option<String>) },
    /// A stream destination lost its connection `lost` times since the last look, and tries
    /// again; `count` is its lost connections in this stream so far.
    Reconnect { destination: String, count: u32, lost: u32 },
    /// The program was paused.
    Pause,
    /// The program resumed.
    Resume,
}

/// One event, with when it happened.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Event {
    /// Counts up from 1 over the session; a reader passes the last one it read as `since`.
    pub seq: u64,
    /// The studio clock's time, in milliseconds since the studio started.
    pub time_ms: u64,
    /// How far into the running recording and stream it happened, in seconds.
    pub recording_s: Option<f64>,
    pub stream_s: Option<f64>,
    pub kind: Kind,
}

/// The last [`CAPACITY`] events of the session.
#[derive(Default)]
pub(crate) struct Events {
    ring: VecDeque<Event>,
    /// The seq of the newest event, 0 before the first.
    last: u64,
}

/// `name` cut to [`MAX_NAME_CHARS`] characters.
fn cut(name: &str) -> String {
    match name.char_indices().nth(MAX_NAME_CHARS) {
        Some((at, _)) => name[..at].to_string(),
        None => name.to_string(),
    }
}

impl Events {
    /// Keep `kind`, dropping the oldest event when the ring is full.
    pub(crate) fn push(&mut self, time_ms: u64, recording_s: Option<f64>, stream_s: Option<f64>, kind: Kind) {
        let kind = match kind {
            Kind::Scene { from, to } => Kind::Scene { from: (from.0, from.1.as_deref().map(cut)), to: (to.0, to.1.as_deref().map(cut)) },
            Kind::Reconnect { destination, count, lost } => Kind::Reconnect { destination: cut(&destination), count, lost },
            other => other,
        };
        if self.ring.capacity() == 0 {
            self.ring.reserve_exact(CAPACITY);
        }
        if self.ring.len() >= CAPACITY {
            self.ring.pop_front();
        }
        self.last = self.last.saturating_add(1);
        self.ring.push_back(Event { seq: self.last, time_ms, recording_s, stream_s, kind });
    }

    /// The events after `since` (all kept events without it), oldest first.
    pub(crate) fn after(&self, since: u64) -> impl Iterator<Item = &Event> {
        // The seqs in the ring count up by one, so the first one after `since` is found by its
        // distance from the oldest.
        let oldest = self.ring.front().map_or(0, |e| e.seq);
        let skip = usize::try_from(since.saturating_add(1).saturating_sub(oldest)).unwrap_or(usize::MAX);
        self.ring.iter().skip(skip)
    }

    /// What `session.events` returns for `since`: the events after it, the seq of the newest
    /// event, and how many events after `since` the ring no longer holds.
    pub(crate) fn json(&self, since: Option<u64>) -> Value {
        let since = since.unwrap_or(0);
        let oldest = self.ring.front().map_or(self.last.saturating_add(1), |e| e.seq);
        let dropped = oldest.saturating_sub(since.saturating_add(1)).min(self.last.saturating_sub(since));
        let events: Vec<Value> = self.after(since).map(event_json).collect();
        json!({"events": events, "last": self.last, "dropped": dropped})
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.ring.len()
    }
}

fn scene_json(scene: &(SceneId, Option<String>)) -> Value {
    json!({"id": scene.0.0, "name": scene.1})
}

fn event_json(e: &Event) -> Value {
    let mut v = json!({"seq": e.seq, "time_ms": e.time_ms, "recording_s": e.recording_s, "stream_s": e.stream_s});
    match &e.kind {
        Kind::Scene { from, to } => {
            v["kind"] = json!("scene");
            v["from"] = scene_json(from);
            v["to"] = scene_json(to);
        }
        Kind::Reconnect { destination, count, lost } => {
            v["kind"] = json!("reconnect");
            v["destination"] = json!(destination);
            v["count"] = json!(count);
            v["lost"] = json!(lost);
        }
        Kind::Pause => v["kind"] = json!("pause"),
        Kind::Resume => v["kind"] = json!("resume"),
    }
    v
}

/// The reconnect counts of the running stream's destinations as last seen, to tell which ones
/// reconnected since.
#[derive(Default)]
pub(crate) struct ReconnectWatch {
    /// The stream they belong to; a new stream counts from 0 again.
    stream: Option<u64>,
    /// The sum of the counts.
    total: u64,
    seen: HashMap<String, u32>,
}

impl ReconnectWatch {
    /// A [`Kind::Reconnect`] for each destination of stream `stream` (`None` when no stream runs)
    /// whose reconnect count went up since the last call. `total` is the sum of the stream's
    /// counts now; `destinations` is called only when it differs from the last call's.
    pub(crate) fn observe(&mut self, stream: Option<u64>, total: u64, destinations: impl FnOnce() -> Vec<DestinationState>) -> Vec<Kind> {
        if stream != self.stream {
            self.stream = stream;
            self.total = 0;
            self.seen.clear();
        }
        if total == self.total {
            return Vec::new();
        }
        self.total = total;
        let mut out = Vec::new();
        for d in destinations() {
            let before = self.seen.get(&d.id).copied().unwrap_or(0);
            if d.reconnects > before {
                out.push(Kind::Reconnect { destination: d.id.clone(), count: d.reconnects, lost: d.reconnects - before });
            }
            if d.reconnects != before {
                self.seen.insert(d.id, d.reconnects);
            }
        }
        out
    }
}

/// The shortest time between two notices that the same destination lost its connection,
/// StudioCraft's own value. The session events keep every loss.
pub(crate) const LOSS_NOTICE_EVERY: Duration = Duration::from_secs(60);

/// Which lost connections and reconnects of the running stream's destinations get a notice: a
/// loss at most once per [`LOSS_NOTICE_EVERY`] for each destination, and a reconnect only after a
/// loss that got one.
#[derive(Default)]
pub(crate) struct ReconnectNotices {
    /// The stream they belong to; a new stream starts with no notices shown.
    stream: Option<u64>,
    /// The sum of the stream's recoveries as last seen.
    recoveries: u64,
    /// Each destination's last loss notice, on the studio clock, and whether its reconnect is
    /// still to be announced.
    shown: HashMap<String, (Tick, bool)>,
}

impl ReconnectNotices {
    /// Forget what was shown when `stream` is not the stream of the last call.
    pub(crate) fn follow(&mut self, stream: Option<u64>) {
        if stream != self.stream {
            self.stream = stream;
            self.recoveries = 0;
            self.shown.clear();
        }
    }

    /// Whether the loss of destination `id`'s connection at `now` gets a notice.
    pub(crate) fn lost(&mut self, id: &str, now: Tick) -> bool {
        let every = tick_of(LOSS_NOTICE_EVERY).0;
        if self.shown.get(id).is_some_and(|(at, _)| now.0.saturating_sub(at.0) < every) {
            return false;
        }
        self.shown.insert(id.to_string(), (now, true));
        true
    }

    /// The destinations whose reconnect gets a notice, once each: the live ones whose loss got a
    /// notice that no reconnect notice has followed. `total` is the sum of the stream's
    /// recoveries now; `destinations` is called only when it differs from the last call's and a
    /// reconnect is still to be announced.
    pub(crate) fn recovered(&mut self, total: u64, destinations: impl FnOnce() -> Vec<DestinationState>) -> Vec<String> {
        if total == self.recoveries {
            return Vec::new();
        }
        self.recoveries = total;
        if !self.shown.values().any(|(_, pending)| *pending) {
            return Vec::new();
        }
        let mut out = Vec::new();
        for d in destinations() {
            if d.status == DestinationStatus::Live
                && let Some((_, pending)) = self.shown.get_mut(&d.id)
                && std::mem::take(pending)
            {
                out.push(d.id);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dest(id: &str, reconnects: u32) -> DestinationState {
        DestinationState { id: id.into(), target: String::new(), status: DestinationStatus::Live, bytes: 0, dropped: 0, reconnects }
    }

    #[test]
    fn the_ring_keeps_the_last_thousand_events_and_counts_what_it_dropped() {
        let mut ev = Events::default();
        assert_eq!(ev.json(None), json!({"events": [], "last": 0, "dropped": 0}));
        for i in 0..(CAPACITY as u64 + 5) {
            ev.push(i, None, None, Kind::Pause);
        }
        assert_eq!(ev.len(), CAPACITY);
        assert_eq!(ev.ring.capacity(), CAPACITY, "the ring does not grow past its reserved room");
        let all = ev.json(None);
        assert_eq!(all["last"], json!(1_005));
        assert_eq!(all["dropped"], json!(5), "the five oldest events fell out");
        assert_eq!(all["events"].as_array().map(Vec::len), Some(CAPACITY));
        assert_eq!(all["events"][0]["seq"], json!(6));
        let tail = ev.json(Some(1_003));
        assert_eq!(tail["events"].as_array().map(|a| a.iter().map(|e| e["seq"].clone()).collect::<Vec<_>>()), Some(vec![json!(1_004), json!(1_005)]));
        assert_eq!(tail["dropped"], json!(0));
        assert_eq!(ev.json(Some(3))["dropped"], json!(2), "events 4 and 5 were not read and are gone");
        assert_eq!(ev.json(Some(1_005))["events"], json!([]));
        assert_eq!(ev.json(Some(u64::MAX)), json!({"events": [], "last": 1_005, "dropped": 0}), "a seq past the newest reads nothing");
    }

    #[test]
    fn each_kind_reads_back_with_its_details_and_long_names_are_cut() {
        let mut ev = Events::default();
        let long = "é".repeat(MAX_NAME_CHARS + 10);
        ev.push(10, Some(1.5), None, Kind::Scene { from: (SceneId(1), None), to: (SceneId(2), Some(long.clone())) });
        ev.push(20, None, Some(2.0), Kind::Reconnect { destination: "main".into(), count: 3, lost: 2 });
        ev.push(30, None, None, Kind::Pause);
        ev.push(40, None, None, Kind::Resume);
        let v = ev.json(None);
        let e = &v["events"];
        assert_eq!(e[0]["kind"], json!("scene"));
        assert_eq!(e[0]["from"], json!({"id": 1, "name": null}), "a removed scene has no name");
        assert_eq!(e[0]["to"]["name"].as_str().map(|n| n.chars().count()), Some(MAX_NAME_CHARS));
        assert_eq!((e[0]["time_ms"].clone(), e[0]["recording_s"].clone(), e[0]["stream_s"].clone()), (json!(10), json!(1.5), Value::Null));
        assert_eq!(e[1]["kind"], json!("reconnect"));
        assert_eq!((e[1]["destination"].clone(), e[1]["count"].clone(), e[1]["lost"].clone()), (json!("main"), json!(3), json!(2)));
        assert_eq!(e[1]["stream_s"], json!(2.0));
        assert_eq!((e[2]["kind"].clone(), e[3]["kind"].clone()), (json!("pause"), json!("resume")));
    }

    #[test]
    fn a_reconnect_is_seen_once_per_new_count_and_a_new_stream_starts_from_zero() {
        let mut w = ReconnectWatch::default();
        let mut reads = 0;
        let mut look = |w: &mut ReconnectWatch, stream: Option<u64>, ds: &[DestinationState]| {
            let total = ds.iter().map(|d| u64::from(d.reconnects)).sum();
            w.observe(stream, total, || {
                reads += 1;
                ds.to_vec()
            })
        };
        let r = |d: &str, count, lost| Kind::Reconnect { destination: d.into(), count, lost };
        assert!(look(&mut w, None, &[]).is_empty());
        assert!(look(&mut w, Some(1), &[dest("a", 0), dest("b", 0)]).is_empty());
        assert_eq!(look(&mut w, Some(1), &[dest("a", 1), dest("b", 0)]), [r("a", 1, 1)]);
        assert!(look(&mut w, Some(1), &[dest("a", 1), dest("b", 0)]).is_empty(), "the same count is not seen twice");
        // Two connections lost between looks are one event that says so.
        assert_eq!(look(&mut w, Some(1), &[dest("a", 3), dest("b", 1)]), [r("a", 3, 2), r("b", 1, 1)]);
        // A new stream counts from 0 again, even when a destination already reached the count
        // the last stream ended with.
        assert_eq!(look(&mut w, Some(2), &[dest("a", 0), dest("b", 1)]), [r("b", 1, 1)]);
        assert_eq!(look(&mut w, Some(2), &[dest("a", 1), dest("b", 1)]), [r("a", 1, 1)]);
        assert!(look(&mut w, None, &[]).is_empty(), "no stream runs");
        assert_eq!(reads, 4, "the states are read only when the sum moved");
    }

    #[test]
    fn a_reconnect_gives_one_loss_and_one_recovery_notice() {
        let mut n = ReconnectNotices::default();
        let at = |s: u64| tick_of(Duration::from_secs(s));
        let with = |id: &str, status| DestinationState { status, ..dest(id, 1) };
        let retrying = || DestinationStatus::Retrying { attempt: 1, failure: studiocraft_plugin::Failure::new(studiocraft_plugin::Cause::Lost, "x") };
        n.follow(Some(1));
        assert!(n.lost("a", at(10)));
        // Not live yet: no notice, and the next change of the sum looks again.
        assert!(n.recovered(1, || vec![with("a", retrying())]).is_empty());
        assert_eq!(n.recovered(2, || vec![with("a", DestinationStatus::Live)]), ["a"]);
        assert!(n.recovered(3, || vec![with("a", DestinationStatus::Live)]).is_empty(), "one recovery notice per loss notice");
        // The same sum reads no states.
        assert!(n.recovered(3, || panic!("read the states")).is_empty());
    }

    #[test]
    fn flapping_gives_one_loss_notice_a_minute() {
        let mut n = ReconnectNotices::default();
        let at = |s: u64| tick_of(Duration::from_secs(s));
        let live = |id: &str| DestinationState { status: DestinationStatus::Live, ..dest(id, 1) };
        n.follow(Some(1));
        assert!(n.lost("a", at(0)));
        assert_eq!(n.recovered(1, || vec![live("a")]), ["a"]);
        // Lost and back again within the minute: neither gets a notice.
        assert!(!n.lost("a", at(30)));
        assert!(n.recovered(2, || vec![live("a")]).is_empty());
        // Another destination has its own minute.
        assert!(n.lost("b", at(31)));
        assert!(n.lost("a", at(60)), "a minute after the last loss notice");
        assert_eq!(n.recovered(3, || vec![live("a"), live("b")]), ["a", "b"]);
        // A new stream starts with no notices shown.
        n.follow(Some(2));
        assert!(n.lost("a", at(61)));
    }
}
