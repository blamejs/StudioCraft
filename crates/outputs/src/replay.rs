//! The replay buffer: the built-in encoders run as for a recording ([`crate::native`]), and the
//! newest packets stay in memory, so the last seconds can be saved to a Matroska file while the
//! buffer keeps running.
//!
//! - The ring keeps the packets in file order. It drops the oldest group of pictures (a keyframe
//!   and the frames up to the next one) once the groups after it still span the length asked for,
//!   so a saved clip starts on a keyframe and lasts that length, or up to one keyframe interval
//!   more.
//! - The packets' bytes stay within the memory limit: the oldest groups go first, and when one
//!   group alone is larger than the limit, the ring holds nothing until the next keyframe.
//! - A save asks the ring thread for its packets through one queue and gets them back through
//!   another, so neither thread waits on a lock. The clip shares the packets with the ring, so
//!   taking it copies no video; while a save is written, the packets it holds stay in memory even
//!   after the ring has dropped them.
//! - A clip keeps the sound packet just before its first keyframe, as a recording keeps the sound
//!   encoder's delay, and its times count from that keyframe.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufWriter, Seek, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::{Duration, Instant};

use craft_media::matroska::{MkvWriter, MuxOptions, TrackSpec};
use crossbeam_queue::ArrayQueue;
use studiocraft_media::audio::PooledBlock;
use studiocraft_media::queue::Fifo;
use studiocraft_media::stats::EncoderStats;
use studiocraft_media::video::PooledFrame;
use studiocraft_model::{Field, FieldKind, ReplaySettings, Settings, SettingsSchema};
use studiocraft_plugin::{EncoderKind, Output, OutputInstance, OutputJob, OutputStatus, Plugin, PluginError, ReplayClip};

use crate::native::{DISK_WAIT, Item, Native, PacketQueue, QUEUE_BYTES, QUEUE_SPAN_NS, Sink, Track, writing_app};
use crate::writer::IDLE;

/// Most saves waiting for the ring thread at once. The studio also writes at most this many
/// replays at once.
pub const MAX_SAVES: usize = 8;
/// How long a save waits for the ring thread to hand over its packets.
const ANSWER_WAIT: Duration = Duration::from_secs(5);

/// One packet in the ring.
#[derive(Clone)]
pub(crate) struct Held {
    pub(crate) track: Track,
    /// The packet's place in file order ([`Item::order_ns`]).
    pub(crate) order_ns: i64,
    pub(crate) pts_ns: i64,
    pub(crate) keyframe: bool,
    pub(crate) data: Arc<Vec<u8>>,
}

impl Held {
    fn starts_group(&self) -> bool {
        self.track == Track::Video && self.keyframe
    }
}

/// The newest packets, in file order, bounded by a length and a number of bytes.
pub(crate) struct Ring {
    items: VecDeque<Held>,
    bytes: usize,
    max_bytes: usize,
    span_ns: i64,
    /// Video keyframes in `items`.
    keyframes: usize,
}

/// The packets a save writes: the ring's packets from the sound packet before its first keyframe.
pub(crate) struct Clip {
    pub(crate) items: Vec<Held>,
    /// The first keyframe's presentation time; the clip's times count from it.
    pub(crate) base_ns: i64,
}

impl Ring {
    pub(crate) fn new(span_ns: i64, max_bytes: usize) -> Ring {
        Ring { items: VecDeque::new(), bytes: 0, max_bytes, span_ns, keyframes: 0 }
    }

    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }

    pub(crate) fn push(&mut self, h: Held) {
        if self.keyframes == 0 && !h.starts_group() {
            // Nothing before the first keyframe can be shown. Of the sound, only the newest
            // packet is kept, the one a clip starts with.
            if h.track == Track::Video {
                return;
            }
            self.clear();
        }
        if h.starts_group() {
            self.keyframes += 1;
        }
        self.bytes = self.bytes.saturating_add(h.data.len());
        self.items.push_back(h);
        self.trim();
    }

    fn clear(&mut self) {
        self.items.clear();
        self.bytes = 0;
        self.keyframes = 0;
    }

    /// The index of the second keyframe in the ring.
    fn second_keyframe(&self) -> Option<usize> {
        self.items.iter().enumerate().filter(|(_, h)| h.starts_group()).nth(1).map(|(i, _)| i)
    }

    fn trim(&mut self) {
        loop {
            let over = self.bytes > self.max_bytes;
            let newest = self.items.back().map_or(i64::MIN, |h| h.order_ns);
            match self.second_keyframe() {
                Some(k) if over || self.items.get(k).is_some_and(|h| newest.saturating_sub(h.order_ns) >= self.span_ns) => self.drop_before(k),
                None if over => self.clear(),
                _ => return,
            }
        }
    }

    /// Drop the packets before index `k`, except the last sound packet among them.
    fn drop_before(&mut self, k: usize) {
        let mut before = None;
        for _ in 0..k {
            let Some(h) = self.items.pop_front() else { break };
            self.bytes = self.bytes.saturating_sub(h.data.len());
            if h.starts_group() {
                self.keyframes = self.keyframes.saturating_sub(1);
            }
            if h.track == Track::Audio {
                before = Some(h);
            }
        }
        if let Some(h) = before {
            self.bytes = self.bytes.saturating_add(h.data.len());
            self.items.push_front(h);
        }
    }

    /// Nanoseconds from the first keyframe to the newest packet; `None` while there is no
    /// keyframe.
    pub(crate) fn held_ns(&self) -> Option<i64> {
        let first = self.items.iter().find(|h| h.starts_group())?;
        Some(self.items.back().map_or(0, |h| h.order_ns.saturating_sub(first.order_ns)))
    }

    /// The packets a save writes now; `None` while there is no keyframe.
    pub(crate) fn snapshot(&self) -> Option<Clip> {
        let first = self.items.iter().position(Held::starts_group)?;
        let base_ns = self.items.get(first)?.pts_ns;
        let start = match first.checked_sub(1).and_then(|i| self.items.get(i)) {
            Some(h) if h.track == Track::Audio => first - 1,
            _ => first,
        };
        Some(Clip { items: self.items.range(start..).cloned().collect(), base_ns })
    }
}

/// An answer to one save: the clip, or `None` when the ring held no keyframe.
type Answer = Arc<ArrayQueue<Option<Clip>>>;

/// What the ring thread shares with the output and its saves.
struct Requests {
    asked: ArrayQueue<Answer>,
    /// Nanoseconds a clip taken now spans, or -1 while the ring holds no keyframe.
    held_ns: AtomicI64,
    /// The ring thread has answered its last request.
    ended: AtomicBool,
}

impl Requests {
    fn new() -> Requests {
        Requests { asked: ArrayQueue::new(MAX_SAVES), held_ns: AtomicI64::new(-1), ended: AtomicBool::new(false) }
    }
}

/// The third thread's end of the replay buffer: it keeps the packets and answers saves.
struct RingSink {
    ring: Ring,
    requests: Arc<Requests>,
    stats: Arc<EncoderStats>,
}

impl Sink for RingSink {
    fn write(&mut self, item: Item) -> Result<(), String> {
        let Item { track, order_ns, pts_ns, keyframe, data } = item;
        self.ring.push(Held { track, order_ns, pts_ns, keyframe, data: Arc::new(data) });
        self.stats.bytes.store(self.ring.bytes() as u64, Ordering::Relaxed);
        self.requests.held_ns.store(self.ring.held_ns().unwrap_or(-1), Ordering::Release);
        Ok(())
    }
    fn poll(&mut self) {
        while let Some(answer) = self.requests.asked.pop() {
            // The queue was made for one answer and gets only this one.
            let _ = answer.push(self.ring.snapshot());
        }
    }
    fn finish(self: Box<Self>) -> Result<(), String> {
        // Dropping answers the saves still waiting.
        Ok(())
    }
}

impl Drop for RingSink {
    fn drop(&mut self) {
        self.poll();
        self.requests.ended.store(true, Ordering::SeqCst);
    }
}

/// A running replay buffer.
pub struct ReplayBuffer {
    native: Native,
    requests: Arc<Requests>,
    tracks: Vec<TrackSpec>,
}

impl ReplayBuffer {
    /// Open the built-in encoders for `job` and keep the last `seconds` of what they encode, in at
    /// most `max_bytes` bytes.
    pub fn start(job: &OutputJob, seconds: u32, max_bytes: usize) -> Result<ReplayBuffer, PluginError> {
        let requests = Arc::new(Requests::new());
        let mut kept = Vec::new();
        let ring = Ring::new(i64::from(seconds).saturating_mul(1_000_000_000), max_bytes);
        let (req, stats) = (Arc::clone(&requests), Arc::clone(&job.stats));
        let open = |tracks: Vec<TrackSpec>| -> Result<Box<dyn Sink>, PluginError> {
            kept.clone_from(&tracks);
            Ok(Box::new(RingSink { ring, requests: req, stats }))
        };
        let native = Native::start_with(job, open, PacketQueue::new(QUEUE_BYTES, QUEUE_SPAN_NS), DISK_WAIT)?;
        Ok(ReplayBuffer { native, requests, tracks: kept })
    }
}

impl OutputInstance for ReplayBuffer {
    fn video(&self) -> Arc<Fifo<Arc<PooledFrame>>> {
        self.native.video()
    }
    fn audio(&self) -> Arc<Fifo<PooledBlock>> {
        self.native.audio()
    }
    fn status(&self) -> OutputStatus {
        self.native.status()
    }
    fn target(&self) -> String {
        String::new()
    }
    fn stop(&mut self) -> Result<(), PluginError> {
        self.native.stop()
    }
    fn replay(&self) -> Result<Box<dyn ReplayClip>, PluginError> {
        if self.requests.ended.load(Ordering::SeqCst) {
            return Err(PluginError::Failed("the replay buffer has stopped".into()));
        }
        if self.requests.held_ns.load(Ordering::Acquire) < 0 {
            return Err(PluginError::Failed("the replay buffer holds nothing to save yet".into()));
        }
        let answer: Answer = Arc::new(ArrayQueue::new(1));
        self.requests
            .asked
            .push(Arc::clone(&answer))
            .map_err(|_| PluginError::Failed(format!("{MAX_SAVES} replays are already being saved; try again when one is done")))?;
        Ok(Box::new(PendingClip { answer, requests: Arc::clone(&self.requests), tracks: self.tracks.clone() }))
    }
}

/// A save the ring thread has been asked for.
struct PendingClip {
    answer: Answer,
    requests: Arc<Requests>,
    tracks: Vec<TrackSpec>,
}

impl PendingClip {
    /// The ring thread's answer.
    fn clip(&self) -> Result<Clip, PluginError> {
        let nothing = || PluginError::Failed("the replay buffer holds nothing to save yet".into());
        let deadline = Instant::now() + ANSWER_WAIT;
        loop {
            if let Some(answer) = self.answer.pop() {
                return answer.ok_or_else(nothing);
            }
            if self.requests.ended.load(Ordering::SeqCst) {
                // The ring thread answers every request it took before it marks the end.
                return self
                    .answer
                    .pop()
                    .ok_or_else(|| PluginError::Failed("the replay buffer stopped before the replay was taken".into()))?
                    .ok_or_else(nothing);
            }
            if Instant::now() >= deadline {
                return Err(PluginError::Failed("the replay buffer did not hand over its video in time".into()));
            }
            std::thread::sleep(IDLE);
        }
    }
}

impl ReplayClip for PendingClip {
    fn write(self: Box<Self>, path: &Path) -> Result<(), PluginError> {
        let clip = self.clip()?;
        let failed = |e: String| PluginError::Failed(format!("the replay could not be written to {}: {e}", path.display()));
        let file = File::create(path).map_err(|e| failed(e.to_string()))?;
        let written = mux_clip(BufWriter::with_capacity(1 << 20, file), self.tracks, &clip)
            .and_then(|w| w.into_inner().map_err(|e| e.error().to_string()))
            .and_then(|f| f.sync_all().map_err(|e| e.to_string()));
        written.map_err(|e| {
            // A file that is not whole is not left behind.
            let _ = std::fs::remove_file(path);
            failed(e)
        })
    }
}

/// Write `clip` as a Matroska file to `w`, with its times counted from its first keyframe.
pub(crate) fn mux_clip<W: Write + Seek>(w: W, tracks: Vec<TrackSpec>, clip: &Clip) -> Result<W, String> {
    let opts = MuxOptions { writing_app: writing_app(), ..MuxOptions::default() };
    let mut mux = MkvWriter::new(w, tracks, opts).map_err(|e| e.to_string())?;
    for h in &clip.items {
        mux.write_frame(h.track as usize, h.pts_ns.saturating_sub(clip.base_ns), h.keyframe, &h.data, None).map_err(|e| e.to_string())?;
    }
    let mut w = mux.finish().map_err(|e| e.to_string())?;
    w.flush().map_err(|e| e.to_string())?;
    Ok(w)
}

/// The value of the whole-number setting `key`, or `default` when it is not set. A value that
/// is not a whole number in `range` is an error.
fn whole(settings: &Settings, key: &str, range: std::ops::RangeInclusive<u32>, default: u32) -> Result<u32, PluginError> {
    let Some(v) = settings.get(key) else { return Ok(default) };
    v.as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .filter(|n| range.contains(n))
        .ok_or_else(|| PluginError::Failed(format!("the replay buffer's `{key}` must be a whole number from {} to {}", range.start(), range.end())))
}

/// The replay buffer output.
pub struct Replay;

impl Plugin for Replay {
    fn id(&self) -> &'static str {
        "replay"
    }
    fn name(&self) -> &'static str {
        "Replay buffer"
    }
    fn schema(&self) -> SettingsSchema {
        let d = ReplaySettings::default();
        let int = |r: std::ops::RangeInclusive<u32>| FieldKind::Int { min: i64::from(*r.start()), max: i64::from(*r.end()) };
        SettingsSchema::new(vec![
            Field::new("seconds", "Replay length (seconds)", int(ReplaySettings::SECONDS), d.seconds),
            Field::new("memory_mb", "Replay memory limit (MB)", int(ReplaySettings::MEMORY_MB), d.memory_mb),
        ])
    }
}

impl Output for Replay {
    fn start(&self, settings: &Settings, job: OutputJob) -> Result<Box<dyn OutputInstance>, PluginError> {
        if job.video_encoder.kind() != EncoderKind::Video || job.video_encoder.ffmpeg().is_some() {
            return Err(PluginError::Failed(format!("the replay buffer runs on the built-in H.264 encoder, not {}", job.video_encoder.name())));
        }
        let d = ReplaySettings::default();
        let seconds = whole(settings, "seconds", ReplaySettings::SECONDS, d.seconds)?;
        let mb = whole(settings, "memory_mb", ReplaySettings::MEMORY_MB, d.memory_mb)?;
        let max_bytes = usize::try_from(u64::from(mb) << 20).unwrap_or(usize::MAX);
        Ok(Box::new(ReplayBuffer::start(&job, seconds, max_bytes)?))
    }
}

#[cfg(test)]
mod tests;
