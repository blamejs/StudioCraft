//! Audio tracks: each recorded source, and the program mix, written to its own 48 kHz stereo
//! 32-bit float WAV file while a recording or the stream runs, for editing afterwards.
//!
//! Threads and queues of a take:
//!
//! | Thread  | Does                                                    | Waits on                    |
//! |---------|---------------------------------------------------------|-----------------------------|
//! | mixer   | stores each tapped input's block in its tap (in `engine`) | nothing                   |
//! | collect | moves blocks from the taps into 1 MiB chunks            | `park_timeout` of 10 ms     |
//! | disk    | writes chunks, headers every 1 s, syncs every 5 s       | `park_timeout` of 10 ms     |
//!
//! Each tap holds 32 blocks (683 ms) and drops its oldest when the collect thread falls behind.
//! The chunk pool is the buffer setting in MiB, each chunk 128 blocks of one track, made when the
//! take starts; when it runs out the collect thread drops blocks and logs the dropout, and the
//! disk thread writes silence in their place. Nothing here blocks the mixer, the outputs or the
//! thread that starts and stops the take, and a take that is off makes no thread and no pool.
//!
//! A take writes `take.journal` beside its files; [`recover`] repairs the files of a take that
//! ended without its journal's last line.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod collect;
pub mod disk;
pub mod journal;
pub mod names;
pub mod recover;
pub mod space;
pub mod wav;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::thread::{JoinHandle, Thread};
use std::time::{Duration, Instant};

use crossbeam_queue::ArrayQueue;
use studiocraft_media::Tick;

pub use collect::{Collector, Dropout, TrackSpec};
pub use disk::{DiskConfig, Faults, Gate, StopReason};

/// A list of tracks for the collect thread, with the clock time it was sent at (none for the
/// first list, sent before any block), and whether it comes from a collection that replaced the
/// one recorded so far ([`Collector::replace_sources`]).
type TrackList = (Vec<TrackSpec>, Option<Tick>, bool);

/// How long the collect and disk threads sleep when there is nothing to do.
pub const IDLE: Duration = Duration::from_millis(10);
/// How often the disk thread rewrites the headers.
pub const HEADER_EVERY: Duration = Duration::from_secs(1);
/// How often the disk thread syncs the files and the journal and checks the free space.
pub const SYNC_EVERY: Duration = Duration::from_secs(5);
/// Tracks a take records at most, the program mix included: 64 taps hold about 17 MiB of
/// preallocated blocks.
pub const MAX_TRACKS: usize = 64;
/// Tracks one take makes at most, counting every source that left or was replaced during it.
pub const MAX_TAKE_TRACKS: usize = 4 * MAX_TRACKS;
/// Messages the disk queue holds beyond the chunks.
const QUEUE_SPARE: usize = 64;
/// How many idle waits the collect thread gives the mixer to take the taps away at a stop
/// ([`Take::stop_after`]) before it ends the take anyway: 5 s.
const STOP_WAITS: u32 = 500;

/// The mixer's count of tap sets applied, and the count at which the taps are gone.
type StopAfter = (Arc<AtomicU64>, u64);

/// What a take is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum State {
    Recording = 1,
    /// Stop was asked for; the last chunks are being written.
    Finishing = 2,
    /// Every file is complete.
    Done = 3,
    /// Stopped early ([`Status::reason`]).
    Stopped = 4,
}

/// The status of a take, for any thread to read without waiting.
#[derive(Default)]
pub struct Status {
    state: AtomicU8,
    reason: AtomicU8,
    /// Frames into the take the collect thread has reached.
    frames: AtomicU64,
    /// Frames into the take at which it stopped early.
    stopped_at: AtomicU64,
    bytes: AtomicU64,
    dropped_frames: AtomicU64,
    tracks: AtomicU64,
    free_bytes: AtomicU64,
    low_space: AtomicBool,
    track_limit: AtomicBool,
    ready: AtomicBool,
}

impl Status {
    /// Whether the collect thread has made its chunk pool and reads the taps: before that, a tap
    /// the mixer filled would drop the take's first audio.
    pub fn ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    pub fn state(&self) -> Option<State> {
        match self.state.load(Ordering::Acquire) {
            1 => Some(State::Recording),
            2 => Some(State::Finishing),
            3 => Some(State::Done),
            4 => Some(State::Stopped),
            _ => None,
        }
    }

    pub fn reason(&self) -> Option<StopReason> {
        StopReason::from_u8(self.reason.load(Ordering::Acquire))
    }

    /// Seconds of audio the take holds.
    pub fn seconds(&self) -> f64 {
        self.frames.load(Ordering::Relaxed) as f64 / f64::from(studiocraft_media::audio::SAMPLE_RATE)
    }

    /// Seconds into the take at which it stopped early.
    pub fn stopped_at_seconds(&self) -> f64 {
        self.stopped_at.load(Ordering::Relaxed) as f64 / f64::from(studiocraft_media::audio::SAMPLE_RATE)
    }

    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    pub fn dropped_seconds(&self) -> f64 {
        self.dropped_frames.load(Ordering::Relaxed) as f64 / f64::from(studiocraft_media::audio::SAMPLE_RATE)
    }

    pub fn dropped_frames(&self) -> u64 {
        self.dropped_frames.load(Ordering::Relaxed)
    }

    pub fn tracks(&self) -> u64 {
        self.tracks.load(Ordering::Relaxed)
    }

    pub fn free_bytes(&self) -> u64 {
        self.free_bytes.load(Ordering::Relaxed)
    }

    pub fn low_space(&self) -> bool {
        self.low_space.load(Ordering::Relaxed)
    }

    /// Whether a source was left without a file because the take already made
    /// [`MAX_TAKE_TRACKS`] files.
    pub fn track_limit(&self) -> bool {
        self.track_limit.load(Ordering::Relaxed)
    }

    fn stop(&self, reason: StopReason) {
        self.stopped_at.store(self.frames.load(Ordering::Relaxed), Ordering::Relaxed);
        self.reason.store(reason as u8, Ordering::Release);
        self.state.store(State::Stopped as u8, Ordering::Release);
    }
}

/// A take's settings.
#[derive(Clone)]
pub struct TakeConfig {
    pub disk: DiskConfig,
    /// Memory for audio waiting for the disk, in MiB: one chunk per MiB.
    pub buffer_mib: u32,
}

/// A take in progress: its collect and disk threads.
pub struct Take {
    status: Arc<Status>,
    stop: Arc<AtomicBool>,
    tracks: Arc<ArrayQueue<TrackList>>,
    after: Arc<ArrayQueue<StopAfter>>,
    collect: Option<JoinHandle<()>>,
    disk: Option<JoinHandle<()>>,
    collect_thread: Thread,
    dir: PathBuf,
}

impl Take {
    /// Start a take recording `specs`. The chunk pool is made on the collect thread, so this
    /// returns once the two threads exist.
    pub fn start(cfg: TakeConfig, specs: Vec<TrackSpec>) -> std::io::Result<Take> {
        Take::start_from(cfg, specs, None)
    }

    /// [`Take::start`], with the take's files starting at `at` on the mixer's clock when it is
    /// given: a track's first block that comes later is placed after silence from `at`.
    pub fn start_from(cfg: TakeConfig, specs: Vec<TrackSpec>, at: Option<Tick>) -> std::io::Result<Take> {
        let status = Arc::new(Status::default());
        status.state.store(State::Recording as u8, Ordering::Release);
        let stop = Arc::new(AtomicBool::new(false));
        let tracks: Arc<ArrayQueue<TrackList>> = Arc::new(ArrayQueue::new(8));
        let _ = tracks.push((specs, None, false));
        let after: Arc<ArrayQueue<StopAfter>> = Arc::new(ArrayQueue::new(1));
        let chunks = cfg.buffer_mib.max(1) as usize;
        let (disk_tx, disk_rx) = std::sync::mpsc::sync_channel::<(Arc<ArrayQueue<collect::ToDisk>>, Arc<ArrayQueue<Vec<u8>>>, Thread)>(1);
        let dir = cfg.disk.dir.clone();

        let c_status = Arc::clone(&status);
        let c_stop = Arc::clone(&stop);
        let c_tracks = Arc::clone(&tracks);
        let c_after = Arc::clone(&after);
        let collect = std::thread::Builder::new().name("audio-tracks-collect".into()).spawn(move || {
            let (mut collector, free, out) = Collector::new(chunks, chunks + QUEUE_SPARE);
            if let Some(at) = at {
                collector.set_start(at);
            }
            if disk_tx.send((Arc::clone(&out), free, std::thread::current())).is_err() {
                return;
            }
            c_status.ready.store(true, Ordering::Release);
            collect_loop(collector, &c_status, &c_stop, &c_tracks, &c_after);
        })?;
        let collect_thread = collect.thread().clone();

        let d_status = Arc::clone(&status);
        let d_stop = Arc::clone(&stop);
        let disk_cfg = cfg.disk;
        let disk = std::thread::Builder::new().name("audio-tracks-disk".into()).spawn(move || {
            let Ok((queue, free, collect_thread)) = disk_rx.recv() else { return };
            disk_loop(disk_cfg, queue, free, &d_status, &d_stop);
            collect_thread.unpark();
        });
        let disk = match disk {
            Ok(d) => d,
            Err(e) => {
                stop.store(true, Ordering::Release);
                status.stop(StopReason::CreateFailed);
                collect_thread.unpark();
                return Err(e);
            }
        };
        Ok(Take { status, stop, tracks, after, collect: Some(collect), disk: Some(disk), collect_thread, dir })
    }

    pub fn status(&self) -> &Arc<Status> {
        &self.status
    }

    /// The take's `Audio Files` folder.
    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }

    /// Record `specs` from `at`, the time on the clock the mixer stamps blocks with. False when
    /// the collect thread has not taken the last lists yet; send them again later.
    pub fn set_tracks(&self, specs: Vec<TrackSpec>, at: Tick) -> bool {
        self.send_tracks(specs, at, false)
    }

    /// Record `specs` from `at`, from a collection that replaced the one recorded so far: every
    /// source's file ends, and each source of the new collection gets a new file. False as for
    /// [`Take::set_tracks`].
    pub fn replace_sources(&self, specs: Vec<TrackSpec>, at: Tick) -> bool {
        self.send_tracks(specs, at, true)
    }

    fn send_tracks(&self, specs: Vec<TrackSpec>, at: Tick, replace: bool) -> bool {
        let ok = self.tracks.push((specs, Some(at), replace)).is_ok();
        self.collect_thread.unpark();
        ok
    }

    /// Ask the take to stop. Returns at once; [`Take::finished`] says when the files are complete.
    pub fn stop(&self) {
        // Only a take still recording becomes Finishing; a take the disk thread stopped stays
        // Stopped, so the collect thread does not wait for a disk thread that has ended.
        let _ = self.status.state.compare_exchange(State::Recording as u8, State::Finishing as u8, Ordering::AcqRel, Ordering::Acquire);
        self.stop.store(true, Ordering::Release);
        self.collect_thread.unpark();
    }

    /// Ask the take to stop once `applied` reaches `generation`: the mixer has taken the taps
    /// away, so it stores no more blocks for the collect thread to miss. The collect thread waits
    /// for that, taking blocks as it waits, for up to 5 s. Returns at once.
    pub fn stop_after(&self, applied: Arc<AtomicU64>, generation: u64) {
        let _ = self.after.force_push((applied, generation));
        self.stop();
    }

    /// Whether both threads have ended.
    pub fn finished(&self) -> bool {
        self.collect.as_ref().is_none_or(|h| h.is_finished()) && self.disk.as_ref().is_none_or(|h| h.is_finished())
    }

    /// Wait for both threads, for tests, the command line and the end of the app.
    pub fn join(mut self) -> Arc<Status> {
        self.stop();
        if let Some(h) = self.collect.take() {
            let _ = h.join();
        }
        if let Some(h) = self.disk.take() {
            let _ = h.join();
        }
        Arc::clone(&self.status)
    }
}

impl Drop for Take {
    fn drop(&mut self) {
        // The threads finish the files on their own; nothing waits for them here.
        self.stop.store(true, Ordering::Release);
        self.collect_thread.unpark();
    }
}

fn collect_loop(mut c: Collector, status: &Status, stop: &AtomicBool, tracks: &ArrayQueue<TrackList>, after: &ArrayQueue<StopAfter>) {
    loop {
        while let Some((list, at, replace)) = tracks.pop() {
            // The blocks already stored come first, so the clock moves the position only past them.
            c.poll();
            if let Some(at) = at {
                c.advance_to(at);
            }
            if replace {
                c.replace_sources(list);
            } else {
                c.set_tracks(list);
            }
            status.tracks.store(c.tracks() as u64, Ordering::Relaxed);
            status.track_limit.store(c.full(), Ordering::Relaxed);
        }
        c.poll();
        status.frames.store(c.position(), Ordering::Relaxed);
        status.dropped_frames.store(c.dropped_frames(), Ordering::Relaxed);
        if status.state() == Some(State::Stopped) {
            return;
        }
        if stop.load(Ordering::Acquire) {
            // Until the mixer has let go of the taps it may still store blocks.
            if let Some((applied, generation)) = after.pop() {
                let mut waits = 0;
                while applied.load(Ordering::Acquire) < generation && waits < STOP_WAITS {
                    if status.state() == Some(State::Stopped) {
                        return;
                    }
                    c.poll();
                    std::thread::park_timeout(IDLE);
                    waits += 1;
                }
            }
            // The blocks the mixer stored before the stop, then the end.
            c.poll();
            status.frames.store(c.position(), Ordering::Relaxed);
            while !c.finish() {
                if status.state() == Some(State::Stopped) {
                    return;
                }
                std::thread::park_timeout(IDLE);
            }
            status.dropped_frames.store(c.dropped_frames(), Ordering::Relaxed);
            return;
        }
        std::thread::park_timeout(IDLE);
    }
}

fn disk_loop(cfg: DiskConfig, queue: Arc<ArrayQueue<collect::ToDisk>>, free: Arc<ArrayQueue<Vec<u8>>>, status: &Status, stop: &AtomicBool) {
    let mut w = match disk::Writer::create(cfg, free) {
        Ok(w) => w,
        Err(f) => {
            log::warn!("audio tracks: {}", f.error);
            status.stop(f.reason);
            return;
        }
    };
    let (mut header_at, mut sync_at) = (Instant::now(), Instant::now());
    loop {
        let mut ended = false;
        let mut result = Ok(());
        while let Some(msg) = queue.pop() {
            match w.handle(msg) {
                Ok(true) => {
                    ended = true;
                    break;
                }
                Ok(false) => {}
                Err(f) => {
                    result = Err(f);
                    break;
                }
            }
        }
        if result.is_ok() && !ended {
            if sync_at.elapsed() >= SYNC_EVERY {
                result = w.sync();
                sync_at = Instant::now();
                header_at = sync_at;
            } else if header_at.elapsed() >= HEADER_EVERY {
                result = w.refresh_headers();
                header_at = Instant::now();
            }
        }
        let p = w.progress();
        status.bytes.store(p.bytes, Ordering::Relaxed);
        status.free_bytes.store(p.free_bytes, Ordering::Relaxed);
        status.low_space.store(p.low_space, Ordering::Relaxed);
        if let Err(f) = result {
            log::warn!("audio tracks stopped: {}: {}", f.reason.text(), f.error);
            status.stop(f.reason);
            w.abandon(f.reason);
            return;
        }
        if ended {
            status.state.store(State::Done as u8, Ordering::Release);
            return;
        }
        // The collect thread ended early (the take stopped) without a last message.
        if stop.load(Ordering::Acquire) && status.state() == Some(State::Stopped) {
            return;
        }
        std::thread::park_timeout(IDLE);
    }
}
