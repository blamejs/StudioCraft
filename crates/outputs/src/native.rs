//! A recording through the built-in encoders, written to a Matroska file by this process.
//!
//! Three threads run for each recording:
//!
//! - The video thread places each NV12 frame on the same timeline as the ffmpeg writer
//!   ([`crate::writer`]): a late frame is dropped, and a gap is filled by repeating the previous
//!   frame. It encodes the frames with the built-in video encoder's session.
//! - The audio thread places the mixer's blocks the same way, fills gaps with silence, and
//!   encodes AAC.
//! - The file thread writes the packets of both streams to the file in time order. A packet of
//!   one stream waits until the other stream has a packet at the same time or later, or has
//!   ended. When one encoder falls more than [`MAX_LAG_NS`] nanoseconds behind the other, the
//!   recording fails.
//!
//! The encoders hand packets to the file thread through a queue. The packets in the queue and
//! those the file thread holds back come to at most [`QUEUE_BYTES`] bytes (a single packet larger
//! than that is let through alone), and the queue spans at most [`QUEUE_SPAN_NS`] nanoseconds of
//! media. An encoder that finds no room waits for up to [`DISK_WAIT`], and then the recording
//! fails.
//!
//! The file thread writes each cluster of the file whole. The first cluster starts with the
//! encoder's delay of the sound, just before the first keyframe; every later cluster starts at a
//! video keyframe, so a cluster spans at most the keyframe interval (at most
//! [`RECORDING_KEYFRAME_S`] seconds). The size of the segment is written last. A recording that
//! is not finished (the app was ended, or finishing took longer than [`FINISH_TIMEOUT`]) still
//! plays up to its last whole cluster.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use craft_media::matroska::{Color, MkvWriter, MuxOptions, TrackKind, TrackSpec};
use crossbeam_queue::ArrayQueue;
use studiocraft_encode::AacSession;
use studiocraft_encode::h264::RECORDING_KEYFRAME_S;
use studiocraft_media::Tick;
use studiocraft_media::audio::{BLOCK_FRAMES, CHANNELS, PooledBlock, SAMPLE_RATE};
use studiocraft_media::queue::Fifo;
use studiocraft_media::stats::EncoderStats;
use studiocraft_media::video::{PixelFormat, PooledFrame};
use studiocraft_plugin::{OutputJob, OutputStatus, Packet, PluginError, RateControl, VideoParams, VideoSession};

use crate::writer::{FINISH_TIMEOUT, IDLE, Timeline, audio_step, seconds_since, video_step};

/// Most bytes of packets waiting for the file thread.
pub const QUEUE_BYTES: usize = 64 << 20;
/// Most media time waiting for the file thread, in nanoseconds.
pub const QUEUE_SPAN_NS: i64 = 3_000_000_000;
/// How long an encoder waits for room in a full packet queue.
pub const DISK_WAIT: Duration = Duration::from_secs(2);
/// How far one stream may run ahead in the file thread while it waits for the other, in
/// nanoseconds. Past that the recording fails, because the other stream's encoder cannot keep up.
pub const MAX_LAG_NS: i64 = 10_000_000_000;
const NS: i128 = 1_000_000_000;

/// The file's tracks, in the order the file lists them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Track {
    Video = 0,
    Audio = 1,
}

/// One packet on its way to the file.
struct Item {
    track: Track,
    /// When the packet is written relative to the other track: its decoding time, or for a
    /// video keyframe its presentation time.
    order_ns: i64,
    pts_ns: i64,
    keyframe: bool,
    data: Vec<u8>,
}

/// Packets from the two encoder threads to the file thread, bounded by bytes and media time.
struct PacketQueue {
    q: ArrayQueue<Item>,
    /// Bytes of the packets in the queue and of those the file thread has taken and not yet
    /// written ([`PacketQueue::written`]).
    bytes: AtomicUsize,
    /// Order time of the newest packet taken, or of the first one queued before any was taken.
    taken_ns: AtomicI64,
    max_bytes: usize,
    max_span_ns: i64,
}

impl PacketQueue {
    fn new(max_bytes: usize, max_span_ns: i64) -> Self {
        PacketQueue { q: ArrayQueue::new(4096), bytes: AtomicUsize::new(0), taken_ns: AtomicI64::new(i64::MIN), max_bytes, max_span_ns }
    }

    /// Queue `item` if there is room; it comes back when there is not. When no packet is waiting
    /// to be written, any packet is taken, so one packet larger than the limit still gets through.
    fn try_push(&self, item: Item) -> Result<(), Item> {
        let _ = self.taken_ns.compare_exchange(i64::MIN, item.order_ns, Ordering::Relaxed, Ordering::Relaxed);
        let len = item.data.len();
        let span = item.order_ns.saturating_sub(self.taken_ns.load(Ordering::Relaxed));
        // The limit is checked and the bytes are counted in one step, so the two encoder threads
        // cannot both pass the check and go past the limit together. The bytes are counted before
        // the packet can be taken, so `written` never subtracts bytes that were not added yet.
        let room = |bytes: usize| (bytes == 0 || (bytes.saturating_add(len) <= self.max_bytes && span <= self.max_span_ns)).then_some(bytes + len);
        if self.bytes.try_update(Ordering::SeqCst, Ordering::SeqCst, room).is_err() {
            return Err(item);
        }
        self.q.push(item).inspect_err(|_| {
            self.bytes.fetch_sub(len, Ordering::SeqCst);
        })
    }

    /// Take the oldest packet. Its bytes count against the limit until [`PacketQueue::written`].
    fn pop(&self) -> Option<Item> {
        let item = self.q.pop()?;
        // The two streams arrive interleaved, so a packet taken can be older than one taken
        // before it; the span counts from the newest.
        self.taken_ns.fetch_max(item.order_ns, Ordering::Relaxed);
        Some(item)
    }

    /// A taken packet of `len` bytes has been written, or dropped.
    fn written(&self, len: usize) {
        self.bytes.fetch_sub(len, Ordering::SeqCst);
    }

    fn is_empty(&self) -> bool {
        self.q.is_empty()
    }
}

struct Shared {
    /// The inputs have ended: the encoders finish what they hold.
    stop: AtomicBool,
    /// Leave the file as it is: the encoders stop and the file thread does not finish the file.
    abandon: AtomicBool,
    /// The file has its first audio.
    started: AtomicBool,
    /// Late frames dropped by the video thread (queue overflows are counted by the queue).
    late: AtomicU64,
    /// Whether the video and the audio encoder thread are still running, by [`Track`].
    producing: [AtomicBool; 2],
    /// A failure is recorded in [`Shared::failure`]; the encoder threads test this for each
    /// frame and block instead of taking the lock.
    failed: AtomicBool,
    failure: Mutex<Option<String>>,
    /// Where the two encoder threads are; see [`crate::writer`].
    timeline: Mutex<Timeline>,
    packets: PacketQueue,
    disk_wait: Duration,
}

impl Shared {
    fn new(packets: PacketQueue, disk_wait: Duration) -> Self {
        Shared {
            stop: AtomicBool::new(false),
            abandon: AtomicBool::new(false),
            started: AtomicBool::new(false),
            late: AtomicU64::new(0),
            producing: [AtomicBool::new(true), AtomicBool::new(true)],
            failed: AtomicBool::new(false),
            failure: Mutex::new(None),
            timeline: Mutex::new(Timeline::default()),
            packets,
            disk_wait,
        }
    }

    /// Record `message` unless an earlier failure is recorded.
    fn fail(&self, message: String) {
        let mut f = self.failure.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if f.is_none() {
            *f = Some(message);
        }
        self.failed.store(true, Ordering::SeqCst);
    }

    /// Whether the encoders stop: the recording is abandoned or has failed.
    fn halted(&self) -> bool {
        self.abandon.load(Ordering::SeqCst) || self.failed.load(Ordering::SeqCst)
    }

    fn failure(&self) -> Option<String> {
        self.failure.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }

    /// Hand `item` to the file thread, waiting for room for up to `disk_wait`. `false` when the
    /// recording is abandoned or the queue stayed full, which fails the recording.
    fn send(&self, mut item: Item) -> bool {
        let mut deadline = None;
        loop {
            match self.packets.try_push(item) {
                Ok(()) => return true,
                Err(back) => item = back,
            }
            if self.abandon.load(Ordering::SeqCst) {
                return false;
            }
            let now = Instant::now();
            if now >= *deadline.get_or_insert(now + self.disk_wait) {
                self.fail("the disk or an encoder could not keep up with the recording".into());
                return false;
            }
            std::thread::sleep(IDLE);
        }
    }

    /// Whether an encoder whose input queue is empty ends: the inputs have ended, or the
    /// recording is abandoned.
    fn ending(&self) -> bool {
        self.abandon.load(Ordering::SeqCst) || self.stop.load(Ordering::SeqCst)
    }
}

/// Marks the encoder thread of a track as ended when it ends, also when it ends by a panic.
struct Producing<'a>(&'a Shared, Track);

impl Drop for Producing<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.fail("an encoder stopped unexpectedly".into());
        }
        self.0.producing[self.1 as usize].store(false, Ordering::SeqCst);
    }
}

/// Fails the recording when the file thread ends by a panic.
struct Writing<'a>(&'a Shared);

impl Drop for Writing<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.fail("the recording could not be written".into());
            self.0.abandon.store(true, Ordering::SeqCst);
        }
    }
}

/// A file writer that counts the bytes in the file into [`EncoderStats::bytes`].
struct Counted<W> {
    inner: W,
    pos: u64,
    stats: Arc<EncoderStats>,
}

impl<W: Write> Write for Counted<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.pos += n as u64;
        self.stats.bytes.fetch_max(self.pos, Ordering::Relaxed);
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl<W: Seek> Seek for Counted<W> {
    fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
        self.pos = self.inner.seek(to)?;
        Ok(self.pos)
    }
}

type Mux = MkvWriter<Counted<BufWriter<File>>>;

/// Nanoseconds of `n` frames at `fps_num / fps_den` frames per second.
fn frames_ns(n: i64, fps_num: u32, fps_den: u32) -> i64 {
    (i128::from(n) * NS * i128::from(fps_den) / i128::from(fps_num.max(1))) as i64
}

/// Nanoseconds of `n` samples at 48 kHz.
fn samples_ns(n: i64) -> i64 {
    (i128::from(n) * NS / i128::from(SAMPLE_RATE)) as i64
}

/// The file's tracks: H.264 video (`avc` is its `avcC` record) and AAC audio.
fn tracks(job: &OutputJob, avc: Vec<u8>, aac: &AacSession) -> Vec<TrackSpec> {
    let v = job.video;
    let mut video = TrackSpec::new(TrackKind::Video, "V_MPEG4/ISO/AVC");
    video.codec_private = avc;
    video.default_duration_ns = u64::try_from(frames_ns(1, v.fps_num, v.fps_den)).ok();
    video.video_size = Some((v.width, v.height));
    // BT.709 at limited range, as the NV12 conversion writes it (H.273 code points; Range 1 is limited).
    video.color =
        Some(Color { matrix_coefficients: Some(1), range: Some(1), transfer_characteristics: Some(1), primaries: Some(1), ..Color::default() });
    let mut audio = TrackSpec::new(TrackKind::Audio, "A_AAC");
    audio.codec_private = aac.audio_specific_config();
    audio.audio = Some((f64::from(SAMPLE_RATE), CHANNELS as u64, None));
    audio.codec_delay_ns = u64::try_from(samples_ns(i64::from(aac.priming_samples()))).unwrap_or(0);
    vec![video, audio]
}

/// A recording through the built-in encoders.
pub struct Native {
    video: Arc<Fifo<Arc<PooledFrame>>>,
    audio: Arc<Fifo<PooledBlock>>,
    shared: Arc<Shared>,
    /// The video, audio and file threads.
    threads: Vec<JoinHandle<()>>,
    stats: Arc<EncoderStats>,
    finished: Option<OutputStatus>,
}

impl Native {
    /// Open the encoders for `job` and start writing a Matroska file at `path`.
    pub fn start(job: &OutputJob, path: &Path) -> Result<Native, PluginError> {
        Self::start_with(job, path, PacketQueue::new(QUEUE_BYTES, QUEUE_SPAN_NS), DISK_WAIT)
    }

    fn start_with(job: &OutputJob, path: &Path, packets: PacketQueue, disk_wait: Duration) -> Result<Native, PluginError> {
        let v = job.video;
        if job.sample_rate != SAMPLE_RATE {
            return Err(PluginError::Failed(format!("the built-in encoders take audio at {SAMPLE_RATE} Hz, not {} Hz", job.sample_rate)));
        }
        if v.pixel != PixelFormat::Nv12 {
            return Err(PluginError::Failed("the built-in encoders take NV12 frames".into()));
        }
        // A cluster ends at a keyframe, so the keyframe interval bounds what a file cut short loses
        // and what the writer holds in memory.
        if let RateControl::Bitrate { keyframe_s, .. } = job.rate
            && keyframe_s > RECORDING_KEYFRAME_S
        {
            return Err(PluginError::Failed(format!(
                "an MKV recording through the built-in encoders needs a keyframe at least every {RECORDING_KEYFRAME_S} seconds, not every {keyframe_s}"
            )));
        }
        let params = VideoParams { width: v.width, height: v.height, fps_num: v.fps_num, fps_den: v.fps_den, rate: job.rate };
        let session = job.video_encoder.open(&job.video_encoder_settings, &params)?;
        let aac = AacSession::new(job.audio_bitrate_kbps)?;
        let file = File::create(path).map_err(|e| PluginError::Failed(format!("{} could not be written: {e}", path.display())))?;
        let out = Counted { inner: BufWriter::with_capacity(1 << 20, file), pos: 0, stats: Arc::clone(&job.stats) };
        let opts = MuxOptions { writing_app: format!("StudioCraft {}", env!("CARGO_PKG_VERSION")), ..MuxOptions::default() };
        let mux = MkvWriter::new(out, tracks(job, session.codec_private(), &aac), opts)
            .map_err(|e| PluginError::Failed(format!("{} could not be written: {e}", path.display())))?;

        let fps = f64::from(v.fps_num) / f64::from(v.fps_den.max(1));
        let video = Arc::new(Fifo::new((fps.ceil() as usize).clamp(8, 240)));
        let audio = Arc::new(Fifo::new(256));
        let shared = Arc::new(Shared::new(packets, disk_wait));
        let frame_bytes = PixelFormat::Nv12.frame_bytes(v.width, v.height).unwrap_or(0);
        let spawn = |name: &str, f: Box<dyn FnOnce() + Send>| std::thread::Builder::new().name(name.into()).spawn(f).map_err(PluginError::failed);
        let mut threads = Vec::new();
        let (q, sh, st, start, rate) = (Arc::clone(&video), Arc::clone(&shared), Arc::clone(&job.stats), job.start, (v.fps_num, v.fps_den));
        let started = spawn("encoder video", Box::new(move || encode_video(session, &q, &sh, &st, start, rate, frame_bytes)));
        let started = started.and_then(|t| {
            threads.push(t);
            let (q, sh) = (Arc::clone(&audio), Arc::clone(&shared));
            spawn("encoder audio", Box::new(move || encode_audio(aac, &q, &sh, start)))
        });
        let started = started.and_then(|t| {
            threads.push(t);
            let sh = Arc::clone(&shared);
            spawn("encoder file", Box::new(move || write_file(mux, &sh)))
        });
        match started {
            Ok(t) => threads.push(t),
            Err(e) => {
                shared.abandon.store(true, Ordering::SeqCst);
                shared.stop.store(true, Ordering::SeqCst);
                return Err(e);
            }
        }
        Ok(Native { video, audio, shared, threads, stats: Arc::clone(&job.stats), finished: None })
    }

    pub fn video(&self) -> Arc<Fifo<Arc<PooledFrame>>> {
        Arc::clone(&self.video)
    }

    pub fn audio(&self) -> Arc<Fifo<PooledBlock>> {
        Arc::clone(&self.audio)
    }

    pub fn status(&self) -> OutputStatus {
        if let Some(s) = &self.finished {
            return s.clone();
        }
        self.stats.queued.store(self.video.len() as u64, Ordering::Relaxed);
        self.stats.dropped.store(self.video.dropped() + self.shared.late.load(Ordering::Relaxed), Ordering::Relaxed);
        if let Some(f) = self.shared.failure() {
            return OutputStatus::Failed(f);
        }
        if self.shared.started.load(Ordering::Relaxed) { OutputStatus::Running } else { OutputStatus::Starting }
    }

    /// End the inputs and wait for the file to be finished.
    pub fn stop(&mut self) -> Result<(), PluginError> {
        self.end(FINISH_TIMEOUT)
    }

    /// End the inputs and wait up to `timeout` for the file to be finished. A file that is not
    /// finished by then is left as it is.
    fn end(&mut self, timeout: Duration) -> Result<(), PluginError> {
        if let Some(s) = &self.finished {
            return match s {
                OutputStatus::Failed(e) => Err(PluginError::Failed(e.clone())),
                _ => Ok(()),
            };
        }
        self.shared.stop.store(true, Ordering::SeqCst);
        let running = |threads: &[JoinHandle<()>]| threads.iter().any(|t| !t.is_finished());
        let deadline = Instant::now() + timeout;
        while running(&self.threads) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let in_time = !running(&self.threads);
        if !in_time {
            // The file thread leaves the file as it is once it sees this; an encoder busy with a
            // frame ends when the frame is done.
            self.shared.abandon.store(true, Ordering::SeqCst);
            let grace = Instant::now() + Duration::from_secs(2);
            while self.threads.last().is_some_and(|t| !t.is_finished()) && Instant::now() < grace {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        for t in std::mem::take(&mut self.threads) {
            // A thread still running is left to end on its own.
            if t.is_finished() {
                let _ = t.join();
            }
        }
        let result = match self.shared.failure() {
            Some(f) => Err(PluginError::Failed(f)),
            None if !in_time => {
                Err(PluginError::Failed("the recording was not finished in time; the file plays up to the last part written before the stop".into()))
            }
            None => Ok(()),
        };
        self.finished = Some(match &result {
            Ok(()) => OutputStatus::Stopped,
            Err(e) => OutputStatus::Failed(e.to_string()),
        });
        result
    }
}

impl Drop for Native {
    fn drop(&mut self) {
        if self.finished.is_none() {
            let _ = self.stop();
        }
    }
}

/// When the video packet `p` is written relative to the sound, in frames. `last` is the order of
/// the packet before it and is moved on.
fn video_order(p: &Packet, last: &mut i64) -> i64 {
    // A keyframe starts a cluster at its presentation time, so the sound before that time goes
    // into the cluster before it. The frames after a keyframe in decoding order are shown after
    // it; one whose decoding time comes before the keyframe's presentation time is written with
    // the keyframe, so the order never goes back.
    let order = if p.keyframe { p.pts.max(p.dts) } else { p.dts };
    *last = order.max(*last);
    *last
}

/// Hand the video packets in `out` to the file thread; `false` when the recording cannot go on.
/// `last` is the order of the last packet handed over ([`video_order`]).
fn send_video(out: &mut Vec<Packet>, shared: &Shared, rate: (u32, u32), last: &mut i64) -> bool {
    for p in out.drain(..) {
        let order = video_order(&p, last);
        let item = Item {
            track: Track::Video,
            order_ns: frames_ns(order, rate.0, rate.1),
            pts_ns: frames_ns(p.pts, rate.0, rate.1),
            keyframe: p.keyframe,
            data: p.data,
        };
        if !shared.send(item) {
            return false;
        }
    }
    true
}

fn encode_video(
    mut session: Box<dyn VideoSession>,
    q: &Fifo<Arc<PooledFrame>>,
    shared: &Shared,
    stats: &EncoderStats,
    start: Tick,
    rate: (u32, u32),
    frame_bytes: usize,
) {
    let _producing = Producing(shared, Track::Video);
    let fps = f64::from(rate.0) / f64::from(rate.1.max(1));
    let (mut next, mut coded, mut order) = (0u64, 0i64, i64::MIN);
    let mut last: Option<Arc<PooledFrame>> = None;
    let mut out = Vec::new();
    'frames: loop {
        if shared.halted() {
            return;
        }
        let Some(frame) = q.pop() else {
            if shared.ending() {
                break;
            }
            std::thread::sleep(IDLE);
            continue;
        };
        if frame.data.len() != frame_bytes {
            shared.fail(format!("a video frame of {} bytes does not have the output's size ({frame_bytes} bytes)", frame.data.len()));
            break;
        }
        let Some(secs) = seconds_since(start, frame.pts) else { continue };
        let Some((n, after)) = video_step(secs, next, fps, &shared.timeline) else {
            shared.late.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        let fill = last.clone().unwrap_or_else(|| Arc::clone(&frame));
        for f in std::iter::repeat_n(&fill, n as usize).chain(std::iter::once(&frame)) {
            if let Err(e) = session.encode(&f.data, coded, &mut out) {
                shared.fail(e.to_string());
                break 'frames;
            }
            coded += 1;
            stats.frames.fetch_add(1, Ordering::Relaxed);
            if !send_video(&mut out, shared, rate, &mut order) {
                return;
            }
        }
        next = after;
        last = Some(frame);
    }
    session.flush(&mut out);
    send_video(&mut out, shared, rate, &mut order);
}

fn encode_audio(mut session: AacSession, q: &Fifo<PooledBlock>, shared: &Shared, start: Tick) {
    let _producing = Producing(shared, Track::Audio);
    let rate = f64::from(SAMPLE_RATE);
    let delay_ns = samples_ns(i64::from(session.priming_samples()));
    let silence = vec![0f32; BLOCK_FRAMES * CHANNELS];
    let (mut written, mut units) = (0i64, 0i64);
    let mut out: Vec<Vec<u8>> = Vec::new();
    // Every access unit holds 1024 samples; the first ones are the encoder's delay.
    let mut send = |out: &mut Vec<Vec<u8>>| {
        for data in out.drain(..) {
            let at = samples_ns(units * 1024) - delay_ns;
            units += 1;
            if !shared.send(Item { track: Track::Audio, order_ns: at, pts_ns: at, keyframe: true, data }) {
                return false;
            }
        }
        true
    };
    loop {
        if shared.halted() {
            return;
        }
        let Some(block) = q.pop() else {
            if shared.ending() {
                break;
            }
            std::thread::sleep(IDLE);
            continue;
        };
        let Some(secs) = seconds_since(start, block.pts) else { continue };
        let frames = (block.samples.len() / CHANNELS) as i64;
        let Some(mut gap) = audio_step(secs, written, frames, rate, &shared.timeline) else { continue };
        while gap > 0 {
            let n = gap.min(BLOCK_FRAMES as i64);
            session.encode(&silence[..n as usize * CHANNELS], &mut out);
            written += n;
            gap -= n;
        }
        session.encode(&block.samples, &mut out);
        written += frames;
        if !send(&mut out) {
            return;
        }
    }
    session.flush(&mut out);
    send(&mut out);
}

/// The next packet to write from `pending` (video, then audio): the one with the earlier
/// decoding time. When only one stream has packets waiting, they wait for the other stream
/// unless its encoder has ended (`ended`, by [`Track`]). `Err` names the stream whose encoder fell
/// more than [`MAX_LAG_NS`] behind.
fn next_item(pending: &mut [VecDeque<Item>; 2], ended: [bool; 2]) -> Result<Option<Item>, Track> {
    let pick = match (pending[0].front(), pending[1].front()) {
        (Some(v), Some(a)) => usize::from(a.order_ns < v.order_ns),
        (None, None) => return Ok(None),
        (v, _) => {
            let (t, other) = if v.is_some() { (0, Track::Audio) } else { (1, Track::Video) };
            if !ended[other as usize] {
                let span = match (pending[t].front(), pending[t].back()) {
                    (Some(first), Some(last)) => last.order_ns.saturating_sub(first.order_ns),
                    _ => 0,
                };
                return if span > MAX_LAG_NS { Err(other) } else { Ok(None) };
            }
            t
        }
    };
    Ok(pending[pick].pop_front())
}

fn write_file(mut mux: Mux, shared: &Shared) {
    let _writing = Writing(shared);
    let mut pending: [VecDeque<Item>; 2] = [VecDeque::new(), VecDeque::new()];
    let write = |mux: &mut Mux, item: Item| -> Result<(), String> {
        mux.write_frame(item.track as usize, item.pts_ns, item.keyframe, &item.data, None)
            .map_err(|e| format!("the recording could not be written: {e}"))?;
        if item.track == Track::Audio {
            shared.started.store(true, Ordering::Relaxed);
        }
        Ok(())
    };
    loop {
        if shared.abandon.load(Ordering::SeqCst) {
            // Dropping the writer writes out what it has buffered, without finishing the file.
            return;
        }
        // Read the flags before the queue: a thread that has ended queued its last packet first.
        let ended = [0, 1].map(|t| !shared.producing[t].load(Ordering::SeqCst));
        let mut idle = true;
        while let Some(item) = shared.packets.pop() {
            pending[item.track as usize].push_back(item);
            idle = false;
        }
        let done = ended == [true, true] && shared.packets.is_empty();
        loop {
            let item = match next_item(&mut pending, ended) {
                Ok(Some(item)) => item,
                Ok(None) => break,
                Err(behind) => {
                    // The encoders end at the failure, and then the packets held here are written
                    // and the file is finished.
                    let name = if behind == Track::Video { "video" } else { "audio" };
                    shared.fail(format!(
                        "the {name} encoder fell more than {} seconds behind; choose a faster preset or a smaller output size in Settings",
                        MAX_LAG_NS / 1_000_000_000
                    ));
                    break;
                }
            };
            let len = item.data.len();
            let wrote = write(&mut mux, item);
            shared.packets.written(len);
            if let Err(e) = wrote {
                shared.fail(e);
                shared.abandon.store(true, Ordering::SeqCst);
                return;
            }
            idle = false;
        }
        if done && pending.iter().all(VecDeque::is_empty) {
            break;
        }
        if idle {
            std::thread::sleep(IDLE);
        }
    }
    let finished = mux.finish().map_err(|e| e.to_string()).and_then(|mut w| w.flush().map_err(|e| e.to_string()));
    if let Err(e) = finished {
        shared.fail(format!("the recording could not be finished: {e}"));
    }
}

#[cfg(test)]
mod tests;
