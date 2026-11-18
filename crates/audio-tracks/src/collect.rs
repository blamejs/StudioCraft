//! The collect side: blocks from the taps, placed by their timestamps into 1 MiB chunks for the
//! disk thread.
//!
//! [`Collector::poll`] runs on the collect thread. Placing a block does not allocate, touch the
//! filesystem or wait: when no free chunk is left, the block is dropped and the dropout is logged,
//! and the disk thread later writes silence for it so every file keeps time. A gap in a tap's
//! timestamps (the mixer's queue dropped its oldest blocks) is handled the same way. Two steps
//! allocate, once a take each: finding the first block of a take that has no start time, and the
//! track list a source change sends.
//!
//! Take frame 0 is the take's start time when it has one ([`Collector::set_start`]): the part of
//! a block before it is cut off, and a track whose first block comes later begins with silence.
//! With an end time ([`Collector::set_end`]), the part of a block after it is cut off and every
//! track still recorded ends there.

use std::sync::Arc;

use crossbeam_queue::ArrayQueue;
use studiocraft_media::Tick;
use studiocraft_media::audio::{BLOCK_FRAMES, BLOCK_SAMPLES, CHANNELS, PooledBlock, SAMPLE_RATE};
use studiocraft_media::tap::TapSlot;
use studiocraft_model::SourceId;

use crate::wav::FRAME_BYTES;

/// Bytes of one chunk: 128 blocks of stereo 32-bit float.
pub const CHUNK_BYTES: usize = 1 << 20;
/// Blocks a chunk holds.
pub const CHUNK_BLOCKS: usize = CHUNK_BYTES / BLOCK_BYTES;
/// Bytes of one block on disk.
pub const BLOCK_BYTES: usize = BLOCK_SAMPLES * 4;
/// Ticks of one 48 kHz frame.
pub const FRAME_TICKS: i64 = craft_time::TICKS_PER_SECOND / SAMPLE_RATE as i64;
/// Ticks of one block.
pub const BLOCK_TICKS: i64 = FRAME_TICKS * BLOCK_FRAMES as i64;
/// Seconds of audio within which a source added back continues its file.
pub const REJOIN_SECONDS: u64 = 60;
/// Dropouts kept in memory for [`Collector::dropouts`]; later ones are only counted there. The
/// journal lists every dropout ([`Collector::dropout_queue`]).
pub const DROPOUT_LOG: usize = 256;
/// Dropouts waiting for the disk thread to journal them. Adjacent losses of a track are one
/// entry, so a full queue needs this many separate dropouts within one pass of the disk thread.
pub const DROPOUT_QUEUE: usize = 4096;
/// Chunks the pool holds at least for each track being recorded: one being filled and one with
/// the disk thread.
pub const CHUNKS_PER_TRACK: usize = 2;

/// Chunks in the pool for a buffer setting of `chunks` and `tracks` tracks recorded at once: the
/// setting, or [`CHUNKS_PER_TRACK`] for each track when that is more.
pub fn pool_chunks(chunks: usize, tracks: usize) -> usize {
    chunks.max(1).max(tracks.saturating_mul(CHUNKS_PER_TRACK))
}

/// Audio for one track, and where in the file it starts.
pub struct Chunk {
    pub track: usize,
    /// The file frame the first sample is at.
    pub first_frame: u64,
    /// Stereo frames of 32-bit float, little-endian; capacity [`CHUNK_BYTES`].
    pub data: Vec<u8>,
}

impl Chunk {
    pub fn frames(&self) -> u64 {
        self.data.len() as u64 / FRAME_BYTES
    }
}

/// Audio lost before it reached the disk thread; the file holds silence there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dropout {
    pub track: usize,
    /// The file frame the silence starts at.
    pub at: u64,
    pub frames: u64,
}

/// What the collect thread sends to the disk thread.
pub enum ToDisk {
    /// A new track; its file opens with its first chunk.
    Track {
        track: usize,
        name: String,
    },
    Chunk(Chunk),
    /// The end of the take, after every chunk and every dropout.
    Finish {
        dropped_frames: u64,
        /// Frames of lost audio left out of the dropout queue because it was full.
        unlisted: u64,
        /// The frame each track ended at, by track; each file is padded with silence to it.
        ends: Vec<u64>,
        /// The take is thrown away: its files, journal and folder are removed.
        discard: bool,
    },
}

/// A source to record, or the program mix (`source` `None`).
#[derive(Clone)]
pub struct TrackSpec {
    pub source: Option<SourceId>,
    /// The name at the time the track starts; the file keeps it.
    pub name: String,
    pub slot: Arc<TapSlot>,
}

struct Track {
    source: Option<SourceId>,
    slot: Option<Arc<TapSlot>>,
    /// The next file frame expected.
    next: u64,
    /// When the slot went away, as a file frame; a rejoin within [`REJOIN_SECONDS`] continues.
    left_at: Option<u64>,
    /// Whether a later timestamp than `next` is lost audio: true once the track has audio, false
    /// again after a rejoin, whose gap is the time the source was away.
    contiguous: bool,
    chunk: Option<Chunk>,
    /// The track's source belonged to a collection that was replaced: a source of the new
    /// collection with the same id starts its own track.
    closed: bool,
    /// The blocks the tap had lost (dropped from its queue or not claimed) when last looked at,
    /// so a gap before the track's first block that the tap lost counts as lost audio.
    lost_seen: u64,
    /// The track's dropout still growing, sent to the disk thread when it ends.
    open: Option<Dropout>,
}

impl Track {
    /// Read from `slot` from now on. A new tap's losses all count, including those before the
    /// collector got the list that names it.
    fn attach(&mut self, slot: Arc<TapSlot>) {
        if !self.slot.as_ref().is_some_and(|s| Arc::ptr_eq(s, &slot)) {
            self.lost_seen = 0;
        }
        self.slot = Some(slot);
    }
}

/// Blocks a tap has lost so far.
fn lost_blocks(slot: &TapSlot) -> u64 {
    slot.dropped() + slot.skipped()
}

/// Places blocks from the taps into chunks.
pub struct Collector {
    tracks: Vec<Track>,
    /// Free chunks; the disk thread puts each back after writing it.
    free: Arc<ArrayQueue<Vec<u8>>>,
    out: Arc<ArrayQueue<ToDisk>>,
    /// The timestamp of take frame 0: the take's start, or without one the first block any tap
    /// gave.
    origin: Option<Tick>,
    /// The furthest frame any track's audio has reached.
    now: u64,
    /// The furthest frame the clock has reached while no tap gave blocks, for the rejoin limit
    /// only: the files never run past their last block because of it.
    clock: u64,
    /// The take frame the take ends at, once its end time is known.
    end: Option<u64>,
    dropouts: Vec<Dropout>,
    dropped_frames: u64,
    /// Dropouts for the disk thread to journal.
    dropq: Arc<ArrayQueue<Dropout>>,
    /// Frames of lost audio that found no room in `dropq`.
    unlisted: u64,
    /// The take is thrown away at its end.
    discard: bool,
    /// Messages to the disk thread that did not fit in its queue and wait for room.
    held: Option<ToDisk>,
    /// Track announcements that found no room; they go to the disk thread before any later chunk.
    announce: std::collections::VecDeque<ToDisk>,
    used_names: std::collections::HashSet<String>,
    /// A source was left without a track because the take already made
    /// [`crate::MAX_TAKE_TRACKS`].
    full: bool,
    /// Chunks made for the pool so far.
    made: usize,
}

impl Collector {
    /// Whether a source was left without a track because the take already made
    /// [`crate::MAX_TAKE_TRACKS`].
    pub fn full(&self) -> bool {
        self.full
    }

    /// The dropouts for the disk thread to journal, each sent once it has ended, adjacent losses
    /// of a track as one.
    pub fn dropout_queue(&self) -> Arc<ArrayQueue<Dropout>> {
        Arc::clone(&self.dropq)
    }

    /// Throw the take away at its end: the disk thread removes its files, journal and folder.
    pub fn set_discard(&mut self) {
        self.discard = true;
    }

    /// A collector with `chunks` free chunks made now and a queue of `queue` messages to the disk
    /// thread. None when the memory for the chunks cannot be reserved; the chunks made so far are
    /// freed. The pool grows when tracks start, to [`CHUNKS_PER_TRACK`] for each track recorded,
    /// up to [`crate::MAX_TRACKS`] tracks.
    pub fn new(chunks: usize, queue: usize) -> Option<(Collector, Arc<ArrayQueue<Vec<u8>>>, Arc<ArrayQueue<ToDisk>>)> {
        let free = Arc::new(ArrayQueue::new(pool_chunks(chunks, crate::MAX_TRACKS)));
        for _ in 0..chunks.max(1) {
            let mut chunk = Vec::new();
            chunk.try_reserve_exact(CHUNK_BYTES).ok()?;
            let _ = free.push(chunk);
        }
        let out = Arc::new(ArrayQueue::new(queue.max(4)));
        let c = Collector {
            tracks: Vec::new(),
            free: Arc::clone(&free),
            out: Arc::clone(&out),
            origin: None,
            now: 0,
            clock: 0,
            end: None,
            dropouts: Vec::with_capacity(DROPOUT_LOG),
            dropped_frames: 0,
            dropq: Arc::new(ArrayQueue::new(DROPOUT_QUEUE)),
            unlisted: 0,
            discard: false,
            held: None,
            announce: std::collections::VecDeque::new(),
            used_names: std::collections::HashSet::new(),
            full: false,
            made: chunks.max(1),
        };
        Some((c, free, out))
    }

    /// Make chunks until the pool holds [`CHUNKS_PER_TRACK`] for each track being recorded. A
    /// chunk whose memory cannot be reserved is not made; the track's blocks are then dropped and
    /// logged as when the disk is slow.
    fn grow(&mut self) {
        let live = self.tracks.iter().filter(|t| t.slot.is_some()).count();
        let want = pool_chunks(self.made, live).min(self.free.capacity());
        while self.made < want {
            let mut chunk = Vec::new();
            if chunk.try_reserve_exact(CHUNK_BYTES).is_err() {
                return;
            }
            let _ = self.free.push(chunk);
            self.made += 1;
        }
    }

    /// Record `specs` from now on: new sources become tracks, sources no longer listed stop, and a
    /// source listed again within [`REJOIN_SECONDS`] continues its file. Runs off the hot path.
    pub fn set_tracks(&mut self, specs: Vec<TrackSpec>) {
        let mut left = Vec::new();
        let at = self.now.max(self.clock);
        for (i, t) in self.tracks.iter_mut().enumerate() {
            if t.slot.is_some() && !specs.iter().any(|s| s.source == t.source) {
                t.slot = None;
                t.left_at = Some(t.next.max(at));
                left.push(i);
            }
        }
        // A track that left gives its partly filled chunk to the disk now, so the chunk is free
        // again for the tracks still recorded.
        for i in left {
            self.send_chunk(i);
        }
        let rejoin = REJOIN_SECONDS * u64::from(SAMPLE_RATE);
        for spec in specs {
            let now = self.now.max(self.clock);
            let made = self.tracks.len();
            let same = self.tracks.iter_mut().rev().find(|t| t.source == spec.source && !t.closed);
            match same {
                Some(t) if t.slot.is_some() => t.attach(spec.slot),
                Some(t) if t.left_at.is_some_and(|left| now.saturating_sub(left) <= rejoin) => {
                    t.attach(spec.slot);
                    t.left_at = None;
                    t.contiguous = false;
                }
                // A take makes at most `MAX_TAKE_TRACKS` files, so sources that keep changing
                // during a long take cannot grow its memory, files and work without end.
                _ if made >= crate::MAX_TAKE_TRACKS => self.full = true,
                _ => {
                    let track = self.tracks.len();
                    let base = crate::names::sanitize(&spec.name, track + 1);
                    let name = crate::names::unique(&base, &mut self.used_names);
                    self.send(ToDisk::Track { track, name });
                    self.tracks.push(Track {
                        source: spec.source,
                        slot: Some(spec.slot),
                        next: 0,
                        left_at: None,
                        contiguous: false,
                        chunk: None,
                        closed: false,
                        lost_seen: 0,
                        open: None,
                    });
                }
            }
        }
        self.grow();
    }

    /// Record `specs` from now on, from a collection that replaced the one recorded so far: every
    /// source track ends, and no source of the new collection continues one, even with the same
    /// id. The program track goes on.
    pub fn replace_sources(&mut self, specs: Vec<TrackSpec>) {
        let mut left = Vec::new();
        let at = self.now.max(self.clock);
        for (i, t) in self.tracks.iter_mut().enumerate().filter(|(_, t)| t.source.is_some()) {
            if t.slot.take().is_some() {
                t.left_at = Some(t.next.max(at));
                left.push(i);
            }
            t.closed = true;
        }
        for i in left {
            self.send_chunk(i);
        }
        self.set_tracks(specs);
    }

    /// Take every block waiting in the taps. Returns the blocks taken.
    pub fn poll(&mut self) -> usize {
        self.retry_held();
        if self.origin.is_none() {
            self.find_origin();
            // No tap had a block: the blocks stored since are taken once there is a frame 0.
            if self.origin.is_none() {
                return 0;
            }
        }
        let mut taken = 0;
        for i in 0..self.tracks.len() {
            loop {
                let Some(block) = self.tracks.get(i).and_then(|t| t.slot.as_ref()).and_then(|s| s.pop()) else { break };
                self.place(i, &block);
                taken += 1;
            }
        }
        taken
    }

    /// Send every chunk being filled, for the end of a take.
    pub fn flush(&mut self) {
        for i in 0..self.tracks.len() {
            self.send_chunk(i);
        }
        self.retry_held();
    }

    /// Whether every message has gone to the disk thread's queue.
    pub fn drained(&self) -> bool {
        self.held.is_none() && self.announce.is_empty()
    }

    /// Frames of audio lost before the disk, over every track.
    pub fn dropped_frames(&self) -> u64 {
        self.dropped_frames
    }

    /// The first [`DROPOUT_LOG`] dropouts.
    pub fn dropouts(&self) -> &[Dropout] {
        &self.dropouts
    }

    /// The furthest take frame any track's audio has reached.
    pub fn position(&self) -> u64 {
        self.now
    }

    /// Let time pass up to `at` on the mixer's clock for the rejoin limit, while no tap gives
    /// blocks. It does not move [`Collector::position`] or where the files end. Nothing moves
    /// before frame 0 is known, or backwards.
    pub fn advance_to(&mut self, at: Tick) {
        if let Some(frame) = self.frame_of(at) {
            self.clock = self.clock.max(frame);
        }
    }

    /// The take frame of `at`, or none before frame 0 is known or before frame 0.
    fn frame_of(&self, at: Tick) -> Option<u64> {
        let origin = self.origin?;
        u64::try_from(at.0.saturating_sub(origin.0).div_euclid(FRAME_TICKS)).ok()
    }

    /// Tracks made so far, the program mix included.
    pub fn tracks(&self) -> usize {
        self.tracks.len()
    }

    /// Make `at` on the mixer's clock take frame 0, before the first block: audio stamped earlier
    /// is cut off, and a track whose first block comes later starts with silence from `at`.
    pub fn set_start(&mut self, at: Tick) {
        self.origin = Some(at);
    }

    /// End the take at `at` on the mixer's clock: audio stamped later is cut off, and every track
    /// still recorded ends there, silent where its source gave no audio.
    pub fn set_end(&mut self, at: Tick) {
        let origin = self.origin.unwrap_or(at);
        let end = u64::try_from(at.0.saturating_sub(origin.0).div_euclid(FRAME_TICKS)).unwrap_or(0);
        self.end = Some(end);
        // Audio placed past the end and still in memory is cut too.
        for t in &mut self.tracks {
            if t.next <= end {
                continue;
            }
            if let Some(c) = t.chunk.as_mut().filter(|c| !c.data.is_empty()) {
                let keep = end.saturating_sub(c.first_frame).min(c.frames());
                c.data.truncate((keep * FRAME_BYTES) as usize);
                t.next = end.max(c.first_frame);
            }
        }
        self.now = self.tracks.iter().map(|t| t.next).max().unwrap_or(0);
    }

    /// For a take with no start time: frame 0 is the earliest first block of the taps that have
    /// one now.
    fn find_origin(&mut self) {
        let mut first: Vec<(usize, PooledBlock)> = Vec::new();
        for (i, t) in self.tracks.iter().enumerate() {
            if let Some(b) = t.slot.as_ref().and_then(|s| s.pop()) {
                first.push((i, b));
            }
        }
        self.origin = first.iter().map(|(_, b)| b.pts).min();
        for (i, b) in first {
            self.place(i, &b);
        }
    }

    fn place(&mut self, i: usize, block: &PooledBlock) {
        let Some(origin) = self.origin else { return };
        // A tap loses whole blocks, so its losses count in whole blocks.
        let whole_frames = (block.samples.len() / CHANNELS) as u64;
        let mut samples: &[f32] = &block.samples;
        let mut frame = (block.pts.0 - origin.0).div_euclid(FRAME_TICKS);
        if frame < 0 {
            // The part before frame 0 is cut off.
            let skip = usize::try_from(frame.unsigned_abs()).unwrap_or(usize::MAX).saturating_mul(CHANNELS);
            samples = samples.get(skip..).unwrap_or_default();
            frame = 0;
        }
        let frame = frame as u64;
        if let Some(end) = self.end {
            // The part after the end is cut off.
            let room = usize::try_from(end.saturating_sub(frame)).unwrap_or(usize::MAX).saturating_mul(CHANNELS);
            samples = samples.get(..room.min(samples.len())).unwrap_or_default();
        }
        if samples.is_empty() {
            return;
        }
        let Some(t) = self.tracks.get_mut(i) else { return };
        if frame < t.next {
            // Older than what the file already holds.
            return;
        }
        let block_frames = (samples.len() / CHANNELS) as u64;
        // A gap in a track that has audio: the tap's queue lost blocks, which the file holds as
        // silence, so it counts as lost audio. Before the track's first block, a gap is lost
        // audio only as far as the tap counted blocks lost; the rest is time the source gave
        // nothing.
        let lost_now = t.slot.as_deref().map_or(t.lost_seen, lost_blocks);
        let tap_lost = lost_now.saturating_sub(t.lost_seen).saturating_mul(whole_frames);
        t.lost_seen = lost_now;
        let gap = frame.saturating_sub(t.next);
        let lost = match t.contiguous {
            true => gap,
            false => gap.min(tap_lost),
        };
        let lost = (lost > 0).then(|| Dropout { track: i, at: frame - lost, frames: lost });
        t.contiguous = true;
        if let Some(d) = lost {
            self.log_dropout(d);
        }
        let Some(t) = self.tracks.get_mut(i) else { return };
        // A gap: the chunk so far goes, and the next one starts after the gap.
        if frame > t.next && t.chunk.as_ref().is_some_and(|c| !c.data.is_empty()) {
            self.send_chunk(i);
        }
        let Some(t) = self.tracks.get_mut(i) else { return };
        if t.chunk.is_none() {
            match self.free.pop() {
                Some(mut data) => {
                    data.clear();
                    t.chunk = Some(Chunk { track: i, first_frame: frame, data });
                }
                None => {
                    t.next = frame + block_frames;
                    self.now = self.now.max(t.next);
                    self.log_dropout(Dropout { track: i, at: frame, frames: block_frames });
                    return;
                }
            }
        }
        let Some(t) = self.tracks.get_mut(i) else { return };
        if let Some(c) = &mut t.chunk {
            if c.data.is_empty() {
                c.first_frame = frame;
            }
            let start = c.data.len();
            c.data.resize(start + samples.len() * 4, 0);
            if let Some(dst) = c.data.get_mut(start..) {
                for (d, s) in dst.as_chunks_mut::<4>().0.iter_mut().zip(samples) {
                    *d = s.to_le_bytes();
                }
            }
        }
        t.next = frame + block_frames;
        self.now = self.now.max(t.next);
        // Audio arrived, so a dropout before it has ended.
        self.send_dropout(i);
        let Some(t) = self.tracks.get(i) else { return };
        if t.chunk.as_ref().is_some_and(|c| c.data.len() + BLOCK_BYTES > CHUNK_BYTES) {
            self.send_chunk(i);
        }
    }

    fn log_dropout(&mut self, d: Dropout) {
        self.dropped_frames += d.frames;
        // The journal's entry: adjacent dropouts of one track grow one entry until it ends.
        if let Some(t) = self.tracks.get_mut(d.track) {
            match &mut t.open {
                Some(open) if open.at + open.frames == d.at => open.frames += d.frames,
                _ => {
                    self.send_dropout(d.track);
                    if let Some(t) = self.tracks.get_mut(d.track) {
                        t.open = Some(d);
                    }
                }
            }
        }
        // Adjacent dropouts of one track are one entry.
        if let Some(last) = self.dropouts.iter_mut().rev().find(|l| l.track == d.track)
            && last.at + last.frames == d.at
        {
            last.frames += d.frames;
            return;
        }
        if self.dropouts.len() < DROPOUT_LOG {
            self.dropouts.push(d);
        }
    }

    /// Send track `i`'s dropout that has ended to the disk thread's dropout queue.
    fn send_dropout(&mut self, i: usize) {
        let Some(d) = self.tracks.get_mut(i).and_then(|t| t.open.take()) else { return };
        if self.dropq.push(d).is_err() {
            self.unlisted += d.frames;
        }
    }

    /// Send the end of the take once every chunk has gone. False while the queue is full.
    pub fn finish(&mut self) -> bool {
        // Each chunk stays with its track until the queue has room for it, so audio already
        // collected is written even when the disk is behind at the stop.
        self.retry_held();
        for i in 0..self.tracks.len() {
            if !self.drained() {
                return false;
            }
            self.send_chunk(i);
        }
        self.retry_held();
        if !self.drained() {
            return false;
        }
        // Every dropout goes to the journal before the end.
        for i in 0..self.tracks.len() {
            self.send_dropout(i);
        }
        // A track still recorded ends with the take, at its end time or else at the last audio
        // of any track, silent where its source gave no audio; a track whose source left ends
        // where it left. With an end time no track runs past it, even audio already sent.
        let end = self.end.unwrap_or(self.now);
        let ends = self
            .tracks
            .iter()
            .map(|t| match (t.left_at, self.end) {
                (Some(left), _) => t.next.max(left).min(end),
                (None, Some(end)) => end,
                (None, None) => t.next.max(end),
            })
            .collect();
        let msg = ToDisk::Finish { dropped_frames: self.dropped_frames, unlisted: self.unlisted, ends, discard: self.discard };
        match self.out.push(msg) {
            Ok(()) => true,
            Err(_) => false,
        }
    }

    fn send_chunk(&mut self, i: usize) {
        let Some(chunk) = self.tracks.get_mut(i).and_then(|t| t.chunk.take()) else { return };
        if chunk.data.is_empty() {
            let _ = self.free.push(chunk.data);
            return;
        }
        self.send(ToDisk::Chunk(chunk));
    }

    fn send(&mut self, msg: ToDisk) {
        self.retry_held();
        if !self.drained() {
            match msg {
                ToDisk::Track { .. } => self.announce.push_back(msg),
                _ => self.lose(msg),
            }
            return;
        }
        if let Err(msg) = self.out.push(msg) {
            self.held = Some(msg);
        }
    }

    /// The held message, then the waiting announcements, in order, while the queue has room.
    fn retry_held(&mut self) {
        if let Some(msg) = self.held.take()
            && let Err(msg) = self.out.push(msg)
        {
            self.held = Some(msg);
            return;
        }
        while let Some(msg) = self.announce.pop_front() {
            if let Err(msg) = self.out.push(msg) {
                self.announce.push_front(msg);
                return;
            }
        }
    }

    /// A message with no room anywhere: a chunk becomes a dropout and goes back to the pool.
    /// Track announcements never come here; they wait in `announce`. The take's disk queue has
    /// room for every chunk of the pool and every track a take can make, so this happens only to
    /// a collector made with a smaller queue.
    fn lose(&mut self, msg: ToDisk) {
        if let ToDisk::Chunk(c) = msg {
            self.log_dropout(Dropout { track: c.track, at: c.first_frame, frames: c.frames() });
            let _ = self.free.push(c.data);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn feed(slot: &TapSlot, block: i64, value: f32) {
        if let Some(mut b) = slot.claim() {
            b.samples.fill(value);
            b.pts = Tick(1_000 + block * BLOCK_TICKS);
            slot.push(b);
        }
    }

    fn spec(source: u64, name: &str) -> TrackSpec {
        TrackSpec { source: Some(SourceId(source)), name: name.into(), slot: Arc::new(TapSlot::new()) }
    }

    fn drain(out: &ArrayQueue<ToDisk>, free: &ArrayQueue<Vec<u8>>) -> Vec<(usize, u64, u64)> {
        let mut got = Vec::new();
        while let Some(m) = out.pop() {
            if let ToDisk::Chunk(c) = m {
                got.push((c.track, c.first_frame, c.frames()));
                let _ = free.push(c.data);
            }
        }
        got
    }

    #[test]
    fn a_chunk_is_exactly_128_blocks() {
        assert_eq!(CHUNK_BLOCKS, 128);
        assert_eq!(FRAME_TICKS, 5_292_000);
        // 130 blocks in a row: one full chunk goes out without its buffer growing, and the
        // rest starts the next one.
        let (mut c, free, out) = Collector::new(4, 64).unwrap();
        let a = spec(1, "A");
        c.set_tracks(vec![a.clone()]);
        for n in 0..130 {
            feed(&a.slot, n, 0.1);
            c.poll();
        }
        let f = BLOCK_FRAMES as u64;
        let mut sizes = Vec::new();
        while let Some(m) = out.pop() {
            if let ToDisk::Chunk(ch) = m {
                assert_eq!(ch.data.capacity(), CHUNK_BYTES, "the chunk's buffer did not grow");
                sizes.push((ch.first_frame, ch.frames()));
                let _ = free.push(ch.data);
            }
        }
        assert_eq!(sizes, vec![(0, 128 * f)]);
        c.flush();
        assert_eq!(drain(&out, &free), vec![(128 * f, 2 * f)].into_iter().map(|(a, b)| (0, a, b)).collect::<Vec<_>>());
    }

    #[test]
    fn blocks_are_placed_by_their_timestamps_and_gaps_start_a_new_chunk() {
        let (mut c, free, out) = Collector::new(8, 64).unwrap();
        let (a, b) = (spec(1, "A"), spec(2, "B"));
        c.set_tracks(vec![a.clone(), b.clone()]);
        // B's first block comes later than A's, and A loses blocks 3 and 4 in its tap.
        feed(&a.slot, 0, 0.1);
        feed(&b.slot, 1, 0.2);
        c.poll();
        for n in [1, 2, 5, 6] {
            feed(&a.slot, n, 0.1);
        }
        feed(&b.slot, 2, 0.2);
        c.poll();
        c.flush();
        let f = BLOCK_FRAMES as u64;
        assert_eq!(drain(&out, &free), vec![(0, 0, 3 * f), (0, 5 * f, 2 * f), (1, f, 2 * f)]);
        // A's gap is lost audio; B starting later than A is not.
        assert_eq!(c.dropped_frames(), 2 * f);
        assert_eq!(c.dropouts(), &[Dropout { track: 0, at: 3 * f, frames: 2 * f }]);
    }

    #[test]
    fn blocks_the_tap_lost_before_the_first_one_collected_are_lost_audio() {
        let (mut c, free, out) = Collector::new(8, 64).unwrap();
        let (a, b) = (spec(1, "A"), spec(2, "B"));
        c.set_tracks(vec![a.clone(), b.clone()]);
        feed(&a.slot, 0, 0.1);
        c.poll();
        // B's tap fills before the collector reads it: 34 blocks from 0 keep the last 32.
        for n in 0..34 {
            feed(&b.slot, n, 0.2);
        }
        assert_eq!(b.slot.dropped(), 2);
        c.poll();
        let f = BLOCK_FRAMES as u64;
        assert_eq!(c.dropouts(), &[Dropout { track: 1, at: 0, frames: 2 * f }]);
        assert_eq!(c.dropped_frames(), 2 * f);
        c.flush();
        drain(&out, &free);
    }

    #[test]
    fn a_source_that_leaves_gives_its_chunk_back_at_once() {
        let (mut c, free, out) = Collector::new(2, 64).unwrap();
        let (a, b) = (spec(1, "A"), spec(2, "B"));
        c.set_tracks(vec![a.clone()]);
        feed(&a.slot, 0, 0.1);
        c.poll();
        // A leaves with a partly filled chunk; B then needs the other one and A's.
        c.set_tracks(vec![b.clone()]);
        assert_eq!(drain(&out, &free).len(), 1, "A's chunk went to the disk");
        let c2 = spec(3, "C");
        c.set_tracks(vec![b.clone(), c2.clone()]);
        feed(&b.slot, 1, 0.2);
        feed(&c2.slot, 1, 0.3);
        c.poll();
        assert_eq!(c.dropped_frames(), 0, "both tracks found a chunk");
        c.flush();
        drain(&out, &free);
    }

    #[test]
    fn every_recorded_track_has_two_chunks_whatever_the_buffer_setting() {
        // 16 chunks set and 20 tracks: each track fills one chunk while its last one is with the
        // disk, here never given back.
        let (mut c, free, out) = Collector::new(16, 256).unwrap();
        let mut specs: Vec<TrackSpec> = (0..20).map(|n| spec(n, "S")).collect();
        c.set_tracks(specs.clone());
        let feed_all = |c: &mut Collector, specs: &[TrackSpec], blocks: std::ops::Range<i64>| {
            for n in blocks {
                for s in specs {
                    feed(&s.slot, n, 0.1);
                }
                c.poll();
            }
        };
        let full = CHUNK_BLOCKS as i64;
        feed_all(&mut c, &specs, 0..full + 1);
        assert_eq!(c.dropped_frames(), 0, "every track found a chunk");
        // A track added during the take gets its chunks too.
        specs.push(spec(99, "Late"));
        c.set_tracks(specs.clone());
        feed_all(&mut c, &specs, full + 1..full + 3);
        assert_eq!(c.dropped_frames(), 0, "the added track found a chunk");
        c.flush();
        drain(&out, &free);
    }

    #[test]
    fn blocks_a_new_tap_lost_before_its_track_was_listed_are_lost_audio() {
        let (mut c, free, out) = Collector::new(8, 64).unwrap();
        let (a, b) = (spec(1, "A"), spec(2, "B"));
        c.set_tracks(vec![a.clone()]);
        feed(&a.slot, 0, 0.1);
        c.poll();
        // The mixer has B's tap and fills it before the collector gets the list naming B.
        for n in 0..34 {
            feed(&b.slot, n, 0.2);
        }
        c.set_tracks(vec![a.clone(), b.clone()]);
        c.poll();
        let f = BLOCK_FRAMES as u64;
        assert_eq!(c.dropouts(), &[Dropout { track: 1, at: 0, frames: 2 * f }]);
        c.flush();
        drain(&out, &free);
    }

    #[test]
    fn chunks_wait_at_the_end_of_a_take_until_the_disk_queue_has_room() {
        let (mut c, free, out) = Collector::new(8, 2).unwrap();
        let (a, b, d) = (spec(1, "A"), spec(2, "B"), spec(3, "D"));
        c.set_tracks(vec![a.clone(), b.clone(), d.clone()]);
        // The three announcements fill the queue's two places and one waits.
        for (n, s) in [&a, &b, &d].into_iter().enumerate() {
            feed(&s.slot, 0, n as f32);
        }
        c.poll();
        let mut chunks = 0;
        let mut finished = false;
        for _ in 0..100 {
            if c.finish() {
                finished = true;
                break;
            }
            while let Some(m) = out.pop() {
                if let ToDisk::Chunk(ch) = m {
                    chunks += 1;
                    let _ = free.push(ch.data);
                }
            }
        }
        while let Some(m) = out.pop() {
            if let ToDisk::Chunk(_) = m {
                chunks += 1;
            }
        }
        assert!(finished);
        assert_eq!((chunks, c.dropped_frames()), (3, 0), "every track's audio went to the disk");
    }

    #[test]
    fn a_take_makes_a_bounded_number_of_tracks() {
        let (mut c, _free, _out) = Collector::new(2, 4096).unwrap();
        for n in 0..crate::MAX_TAKE_TRACKS as u64 + 10 {
            c.replace_sources(vec![spec(n, "S")]);
        }
        assert_eq!(c.tracks(), crate::MAX_TAKE_TRACKS);
        assert!(c.full());
    }

    #[test]
    fn with_no_free_chunk_blocks_are_dropped_and_logged_exactly() {
        let (mut c, free, out) = Collector::new(2, 64).unwrap();
        let a = spec(1, "A");
        c.set_tracks(vec![a.clone()]);
        for n in 0..300 {
            feed(&a.slot, n, 0.5);
            c.poll();
        }
        let f = BLOCK_FRAMES as u64;
        assert_eq!(c.dropped_frames(), 44 * f);
        assert_eq!(c.dropouts(), &[Dropout { track: 0, at: 256 * f, frames: 44 * f }], "one entry for the whole run");
        assert_eq!(drain(&out, &free), vec![(0, 0, 128 * f), (0, 128 * f, 128 * f)]);
        // With chunks back, recording goes on where it is.
        feed(&a.slot, 300, 0.5);
        c.poll();
        c.flush();
        assert_eq!(drain(&out, &free), vec![(0, 300 * f, f)]);
    }

    #[test]
    fn a_track_added_while_the_disk_queue_is_full_keeps_its_name_and_comes_before_its_audio() {
        let (mut c, free, out) = Collector::new(8, 4).unwrap();
        let (a, b) = (spec(1, "A"), spec(2, "B"));
        c.set_tracks(vec![a.clone()]);
        // The announcement of A and three chunks fill the queue; the fourth chunk waits.
        for n in 0..4 * CHUNK_BLOCKS as i64 {
            feed(&a.slot, n, 0.1);
            c.poll();
        }
        assert!(!c.drained());
        c.set_tracks(vec![a.clone(), b.clone()]);
        let mut seen = Vec::new();
        let take = |seen: &mut Vec<String>| {
            while let Some(m) = out.pop() {
                seen.push(match m {
                    ToDisk::Track { track, name } => format!("track {track} {name}"),
                    ToDisk::Chunk(ch) => {
                        let s = format!("chunk {}", ch.track);
                        let _ = free.push(ch.data);
                        s
                    }
                    ToDisk::Finish { .. } => "finish".into(),
                });
            }
        };
        take(&mut seen);
        let at = 4 * CHUNK_BLOCKS as i64;
        feed(&b.slot, at, 0.2);
        c.poll();
        assert!(c.finish());
        take(&mut seen);
        let want = ["track 0 A", "chunk 0", "chunk 0", "chunk 0", "chunk 0", "track 1 B", "chunk 1", "finish"];
        assert_eq!(seen, want);
    }

    #[test]
    fn the_rejoin_limit_follows_the_clock_while_no_tap_gives_blocks() {
        let (mut c, _free, _out) = Collector::new(4, 64).unwrap();
        let a = spec(1, "Mic");
        c.set_tracks(vec![a.clone()]);
        feed(&a.slot, 0, 0.5);
        c.poll();
        c.set_tracks(vec![]);
        // Nothing is recorded for 30 s, then 61 s; only the clock tells the time.
        c.advance_to(Tick(1_000 + 30 * craft_time::TICKS_PER_SECOND));
        c.set_tracks(vec![spec(1, "Mic")]);
        assert_eq!(c.tracks(), 1, "back within a minute: the same track");
        c.set_tracks(vec![]);
        c.advance_to(Tick(1_000 + 92 * craft_time::TICKS_PER_SECOND));
        c.set_tracks(vec![spec(1, "Mic")]);
        assert_eq!(c.tracks(), 2, "back after more than a minute: a new track");
        c.advance_to(Tick(0));
        c.set_tracks(vec![]);
        c.set_tracks(vec![spec(1, "Mic")]);
        assert_eq!(c.tracks(), 2, "the clock never goes back: the track that left at 92 s continues");
    }

    #[test]
    fn the_clock_does_not_move_where_the_files_end() {
        let (mut c, free, out) = Collector::new(4, 64).unwrap();
        let (a, b) = (spec(1, "Mic"), spec(2, "Desk"));
        c.set_tracks(vec![a.clone(), b.clone()]);
        feed(&a.slot, 0, 0.5);
        c.poll();
        // The list of a change is sent at a clock time past the last block the mixer made.
        c.advance_to(Tick(1_000 + 30 * craft_time::TICKS_PER_SECOND));
        c.set_tracks(vec![a.clone()]);
        assert_eq!(c.position(), BLOCK_FRAMES as u64);
        assert!(c.finish());
        let f = BLOCK_FRAMES as u64;
        let ends: Vec<Vec<u64>> = std::iter::from_fn(|| out.pop())
            .filter_map(|m| match m {
                ToDisk::Finish { ends, .. } => Some(ends),
                ToDisk::Chunk(ch) => {
                    let _ = free.push(ch.data);
                    None
                }
                ToDisk::Track { .. } => None,
            })
            .collect();
        assert_eq!(ends, [vec![f, f]], "both files end at the last block, not at the clock");
    }

    #[test]
    fn a_start_time_cuts_audio_before_it_and_an_end_time_cuts_audio_after_it() {
        let (mut c, free, out) = Collector::new(4, 64).unwrap();
        let (a, b) = (spec(1, "Mic"), spec(2, "Quiet"));
        c.set_tracks(vec![a.clone(), b.clone()]);
        let f = BLOCK_FRAMES as u64;
        // Frame 0 is 100 frames into block 0, and the end is 200 frames into block 2.
        c.set_start(Tick(1_000 + 100 * FRAME_TICKS));
        c.set_end(Tick(1_000 + 2 * BLOCK_TICKS + 200 * FRAME_TICKS));
        for n in 0..5 {
            feed(&a.slot, n, 0.25);
        }
        c.poll();
        assert!(c.finish());
        let mut chunks = Vec::new();
        let mut ends = Vec::new();
        while let Some(m) = out.pop() {
            match m {
                ToDisk::Chunk(ch) => {
                    chunks.push((ch.track, ch.first_frame, ch.frames()));
                    let _ = free.push(ch.data);
                }
                ToDisk::Finish { ends: e, .. } => ends = e,
                ToDisk::Track { .. } => {}
            }
        }
        let end = 2 * f + 200 - 100;
        assert_eq!(chunks, [(0, 0, end)], "the audio between the start and the end, from frame 0");
        assert_eq!(ends, [end, end], "a track with no audio ends at the end time too");
        assert_eq!(c.dropped_frames(), 0, "audio cut at the start or end is not lost audio");
    }

    #[test]
    fn chunks_lost_one_after_another_are_one_dropout() {
        let (mut c, free, out) = Collector::new(16, 4).unwrap();
        let a = spec(1, "A");
        c.set_tracks(vec![a.clone()]);
        // The announcement and three chunks fill the queue, the fourth waits, and the fifth and
        // sixth find no room.
        for n in 0..6 * CHUNK_BLOCKS as i64 {
            feed(&a.slot, n, 0.1);
            c.poll();
        }
        let f = BLOCK_FRAMES as u64;
        let chunk = CHUNK_BLOCKS as u64 * f;
        assert_eq!(c.dropouts(), &[Dropout { track: 0, at: 4 * chunk, frames: 2 * chunk }]);
        drain(&out, &free);
    }

    #[test]
    fn every_dropout_goes_to_the_disk_queue_once_it_ends_past_the_memory_list() {
        let (mut c, free, out) = Collector::new(16, 256).unwrap();
        let a = spec(1, "A");
        c.set_tracks(vec![a.clone()]);
        let gaps = DROPOUT_LOG as i64 + 44;
        // Every other block is lost in the tap: each gap is a dropout of its own.
        for n in 0..=gaps {
            feed(&a.slot, 2 * n, 0.1);
            c.poll();
            drain(&out, &free);
        }
        let q = c.dropout_queue();
        assert_eq!(c.dropouts().len(), DROPOUT_LOG, "the memory list stops at its limit");
        assert_eq!(q.len(), gaps as usize, "the disk queue has every dropout that ended");
        let f = BLOCK_FRAMES as u64;
        assert_eq!(q.pop(), Some(Dropout { track: 0, at: f, frames: f }));
    }

    #[test]
    fn a_source_added_back_within_a_minute_continues_and_later_gets_a_new_track() {
        let (mut c, free, out) = Collector::new(16, 64).unwrap();
        let a = spec(1, "Mic");
        c.set_tracks(vec![a.clone()]);
        feed(&a.slot, 0, 0.5);
        c.poll();
        c.set_tracks(vec![]);
        let a2 = spec(1, "Renamed");
        c.set_tracks(vec![a2.clone()]);
        assert_eq!(c.tracks(), 1, "added back at once: the same track");
        feed(&a2.slot, 3, 0.5);
        c.poll();
        c.set_tracks(vec![]);
        // Another track moves time on past a minute.
        let clock = spec(9, "Clock");
        c.set_tracks(vec![clock.clone()]);
        let minute = (REJOIN_SECONDS * u64::from(SAMPLE_RATE)).div_ceil(BLOCK_FRAMES as u64) as i64;
        feed(&clock.slot, 4 + minute + 1, 0.0);
        c.poll();
        let a3 = spec(1, "Mic");
        c.set_tracks(vec![clock.clone(), a3.clone()]);
        assert_eq!(c.tracks(), 3, "after a minute away: a new track");
        let names: Vec<String> = std::iter::from_fn(|| out.pop())
            .filter_map(|m| match m {
                ToDisk::Track { name, .. } => Some(name),
                ToDisk::Chunk(ch) => {
                    let _ = free.push(ch.data);
                    None
                }
                ToDisk::Finish { .. } => None,
            })
            .collect();
        assert_eq!(names, ["Mic", "Clock", "Mic 2"], "a track keeps the name it started with");
    }
}
