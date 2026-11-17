//! The ffmpeg process behind a recording or a stream.
//!
//! ffmpeg reads NV12 frames on its standard input and 48 kHz stereo `f32` samples from a pipe
//! given to it as its standard output handle (`pipe:1`). Only the ffmpeg process holds the read
//! end of either pipe, so no other process on the computer can receive the pictures or the
//! sound. Both inputs are timed by count (frames at
//! the output rate, samples at 48 kHz), so the writer threads keep the counts in step with the
//! master clock: a missing frame is filled by repeating the previous one, a late frame is
//! dropped, and a gap in the audio is filled with silence.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use portable_atomic::AtomicU128;
use studiocraft_media::Tick;
use studiocraft_media::audio::{BLOCK_FRAMES, CHANNELS, PooledBlock, SAMPLE_RATE};
use studiocraft_media::clock::duration_of;
use studiocraft_media::queue::Fifo;
use studiocraft_media::stats::EncoderStats;
use studiocraft_media::video::{PixelFormat, PooledFrame};
use studiocraft_platform::ffmpeg;
use studiocraft_plugin::{OutputJob, OutputStatus, PluginError};

/// How far, in seconds, a gap in one stream is filled past what the other stream has written,
/// by repeating frames or inserting silence. The rest of a longer gap in both streams (the
/// computer was suspended) is skipped in both.
const MAX_FILL_S: f64 = 5.0;
/// How long ffmpeg, or the built-in encoders, may take to finish a file or stream after the
/// inputs end.
pub(crate) const FINISH_TIMEOUT: Duration = Duration::from_secs(20);
/// Idle wait of the writer threads when their queue is empty.
pub(crate) const IDLE: Duration = Duration::from_millis(2);

#[derive(Default)]
struct Shared {
    stop: AtomicBool,
    /// ffmpeg has taken the first audio.
    started: AtomicBool,
    /// Late frames dropped by the writer (queue overflows are counted by the queue).
    late: AtomicU64,
    failure: Mutex<Option<String>>,
    /// The last line ffmpeg reported that was not progress.
    last_message: Mutex<String>,
    /// The first [`COMPONENT_LINES`] such lines from ffmpeg's components (`[h264_nvenc @ …] …`),
    /// one of which names the encoder when the encoder failed; a general line often follows them.
    component_message: Mutex<Vec<String>>,
    /// Text never logged or shown: the stream key.
    secret: String,
    /// Where the two writers are.
    timeline: Timeline,
}

/// Where the two writers are, as one word ([`Place`]) that a writer reads and changes in one step.
/// Both writers place what they write [`Place::skipped`] seconds earlier, so a gap that one of
/// them skips is skipped in both and they stay in step.
///
/// A step reads the word, places its frame or block, and commits the new word with a
/// compare-and-swap. When the other writer has committed in the meantime, the commit fails and
/// the step places its frame or block again, so a gap that both writers see is skipped once.
/// On aarch64, and on x86_64 processors with `cmpxchg16b`, the 128-bit compare-and-swap is
/// lock-free, so neither writer waits for the other.
#[derive(Default)]
pub(crate) struct Timeline(AtomicU128);

/// The nanoseconds of long gaps skipped so far (the high 64 bits of a [`Timeline`]) and the
/// nanoseconds of output the audio writer has written past the video writer (the low 64 bits,
/// negative when the video writer is ahead). Each writer knows its own position and finds the
/// other's from the difference.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Place {
    skipped_ns: u64,
    audio_ahead_ns: i64,
}

impl Place {
    fn unpack(word: u128) -> Place {
        Place { skipped_ns: (word >> 64) as u64, audio_ahead_ns: word as u64 as i64 }
    }

    fn pack(self) -> u128 {
        (u128::from(self.skipped_ns) << 64) | u128::from(self.audio_ahead_ns as u64)
    }

    fn skipped(self) -> f64 {
        self.skipped_ns as f64 / 1e9
    }

    /// `self` after `skip` more seconds are skipped and the audio writer moves `audio_ns`
    /// nanoseconds further ahead of the video writer (negative when the video writer moves).
    fn moved(self, skip: f64, audio_ns: i64) -> Place {
        let skip_ns = u64::try_from(ns(skip)).unwrap_or(0);
        Place { skipped_ns: self.skipped_ns.saturating_add(skip_ns), audio_ahead_ns: self.audio_ahead_ns.saturating_add(audio_ns) }
    }
}

/// `secs` in whole nanoseconds.
fn ns(secs: f64) -> i64 {
    (secs * 1e9).round() as i64
}

/// The whole frames or samples in `count`, which comes from a position rounded to the
/// nanosecond and can fall short of a whole count by up to half a nanosecond's worth (0.000024 of
/// a sample at 48 kHz).
fn whole(count: f64) -> f64 {
    (count + 1e-3).floor()
}

impl Timeline {
    /// The word, to pass to [`Timeline::commit`].
    fn read(&self) -> u128 {
        self.0.load(Ordering::Acquire)
    }

    /// Stores `next` if the word still holds `seen`. `false` when the other writer has committed
    /// since `seen` was read; nothing is changed then.
    fn commit(&self, seen: u128, next: Place) -> bool {
        self.0.compare_exchange(seen, next.pack(), Ordering::AcqRel, Ordering::Acquire).is_ok()
    }

    /// Seconds skipped so far.
    #[cfg(test)]
    fn skipped(&self) -> f64 {
        Place::unpack(self.read()).skipped()
    }
}

/// The distinct component lines of ffmpeg's errors a writer keeps. ffmpeg runs with
/// `-loglevel error`, so it prints only errors, and a repeated error is kept once.
const COMPONENT_LINES: usize = 16;

/// Why ffmpeg ended with exit status `code`: the components' error lines when ffmpeg ended with
/// another line after them, then the last line.
fn ended_text(shared: &Shared, code: impl std::fmt::Display) -> String {
    let last = shared.last_message.lock().map(|m| m.clone()).unwrap_or_default();
    let component = shared.component_message.lock().map(|m| m.join("; ")).unwrap_or_default();
    match (component.is_empty() || component == last, last.is_empty()) {
        (_, true) => format!("ffmpeg stopped unexpectedly ({code})"),
        (true, false) => format!("ffmpeg stopped: {last}"),
        (false, false) => format!("ffmpeg stopped: {component}; {last}"),
    }
}

/// Whether the log thread, the last of a writer's `threads`, has read all of ffmpeg's output.
fn log_read(threads: &[JoinHandle<()>]) -> bool {
    threads.last().is_none_or(JoinHandle::is_finished)
}

/// A writer's `failure` (ffmpeg stopped reading its input) with the components' error lines and
/// ffmpeg's last line, which say why when an encoder or its device failed.
fn failure_text(shared: &Shared, failure: String) -> String {
    let mut lines = shared.component_message.lock().map(|m| m.clone()).unwrap_or_default();
    let last = shared.last_message.lock().map(|m| m.clone()).unwrap_or_default();
    if !last.is_empty() && !lines.contains(&last) {
        lines.push(last);
    }
    if lines.is_empty() { failure } else { format!("{failure}: {}", lines.join("; ")) }
}

/// `line` with every occurrence of `secret` replaced by `***`.
fn redact(line: &str, secret: &str) -> String {
    if secret.is_empty() { line.to_string() } else { line.replace(secret, "***") }
}

impl Shared {
    fn fail(&self, message: String) {
        if let Ok(mut f) = self.failure.lock()
            && f.is_none()
        {
            *f = Some(message);
        }
    }

    /// Seconds skipped so far.
    #[cfg(test)]
    fn skipped(&self) -> f64 {
        self.timeline.skipped()
    }
}

/// A running ffmpeg with its writer threads.
pub struct Writer {
    video: Arc<Fifo<Arc<PooledFrame>>>,
    audio: Arc<Fifo<PooledBlock>>,
    shared: Arc<Shared>,
    child: Mutex<Child>,
    threads: Vec<JoinHandle<()>>,
    stats: Arc<EncoderStats>,
    finished: Option<OutputStatus>,
}

/// The arguments before the codec options: both inputs and the stream mapping. Video comes on
/// ffmpeg's standard input and audio on the pipe given to it as its standard output.
pub fn input_args(job: &OutputJob) -> Vec<String> {
    let v = job.video;
    let mut a: Vec<String> =
        ["-hide_banner", "-nostdin", "-y", "-loglevel", "error", "-progress", "pipe:2", "-stats_period", "0.5"].map(String::from).to_vec();
    a.extend(["-f", "rawvideo", "-pix_fmt", PixelFormat::Nv12.ffmpeg_name()].map(String::from));
    a.extend(["-video_size".into(), format!("{}x{}", v.width, v.height), "-framerate".into(), format!("{}/{}", v.fps_num, v.fps_den.max(1))]);
    a.extend(["-thread_queue_size", "64", "-i", "pipe:0"].map(String::from));
    a.extend(["-f".into(), "f32le".into(), "-ar".into(), job.sample_rate.to_string(), "-ac".into(), CHANNELS.to_string()]);
    a.extend(["-thread_queue_size", "64", "-i", "pipe:1"].map(String::from));
    a.extend(["-map", "0:v:0", "-map", "1:a:0"].map(String::from));
    a
}

/// AAC audio at `kbps`.
pub fn audio_args(kbps: u32) -> Vec<String> {
    vec!["-c:a".into(), "aac".into(), "-b:a".into(), format!("{}k", kbps.clamp(32, 512))]
}

impl Writer {
    /// Start ffmpeg with `codec_args` (video then audio) and `output_args` (container and
    /// destination). `secret` (a stream key) is replaced by `***` in every line ffmpeg reports.
    pub fn start(job: &OutputJob, codec_args: &[String], output_args: &[String], secret: &str) -> Result<Writer, PluginError> {
        if !Path::new(&job.ffmpeg).is_file() {
            return Err(PluginError::Unavailable(format!("ffmpeg was not found at {}", job.ffmpeg.display())));
        }
        let (audio_reader, audio_pipe) = std::io::pipe().map_err(|e| PluginError::Failed(format!("no pipe for audio: {e}")))?;
        let mut cmd = ffmpeg::command(&job.ffmpeg);
        cmd.args(input_args(job)).args(codec_args).args(output_args).stdin(Stdio::piped()).stdout(Stdio::from(audio_reader)).stderr(Stdio::piped());
        let spawned = cmd.spawn();
        // The command holds this process's copy of the audio pipe's read end. Dropping it leaves
        // ffmpeg the only reader, so a write fails once ffmpeg has exited.
        drop(cmd);
        let mut child = spawned.map_err(|e| PluginError::Failed(format!("ffmpeg could not be started: {e}")))?;
        let (stdin, stderr) = (child.stdin.take(), child.stderr.take());
        let (Some(stdin), Some(stderr)) = (stdin, stderr) else {
            let _ = child.kill();
            return Err(PluginError::Failed("ffmpeg's input could not be opened".into()));
        };

        let fps = f64::from(job.video.fps_num) / f64::from(job.video.fps_den.max(1));
        let video = Arc::new(Fifo::new((fps.ceil() as usize).clamp(8, 240)));
        let audio = Arc::new(Fifo::new(256));
        let shared = Arc::new(Shared { secret: secret.to_string(), ..Shared::default() });
        let mut threads = Vec::new();
        let spawn = |name: &str, f: Box<dyn FnOnce() + Send>| std::thread::Builder::new().name(name.into()).spawn(f).map_err(PluginError::failed);

        let (q, sh, st, start) = (Arc::clone(&video), Arc::clone(&shared), Arc::clone(&job.stats), job.start);
        let frame_bytes = PixelFormat::Nv12.frame_bytes(job.video.width, job.video.height).unwrap_or(0);
        let started = spawn("encoder video", Box::new(move || write_video(stdin, &q, &sh, &st, start, fps, frame_bytes)));
        let started = started.and_then(|t| {
            threads.push(t);
            let (q, sh) = (Arc::clone(&audio), Arc::clone(&shared));
            spawn("encoder audio", Box::new(move || write_audio(audio_pipe, &q, &sh, start)))
        });
        let started = started.and_then(|t| {
            threads.push(t);
            let (sh, st) = (Arc::clone(&shared), Arc::clone(&job.stats));
            spawn("encoder log", Box::new(move || read_log(stderr, &sh, &st)))
        });
        match started {
            Ok(t) => threads.push(t),
            Err(e) => {
                shared.stop.store(true, Ordering::Relaxed);
                let _ = child.kill();
                return Err(e);
            }
        }
        Ok(Writer { video, audio, shared, child: Mutex::new(child), threads, stats: Arc::clone(&job.stats), finished: None })
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
        // When ffmpeg has exited, its last lines say why; a failure is then reported once the log
        // thread (the last thread) has read them. A failure while ffmpeg runs is reported at once.
        let exited = self.child.try_lock().ok().and_then(|mut c| c.try_wait().ok().flatten());
        let log_read = exited.is_none() || log_read(&self.threads);
        if let Some(f) = self.shared.failure.lock().ok().and_then(|f| f.clone())
            && log_read
        {
            return OutputStatus::Failed(failure_text(&self.shared, f));
        }
        if let Some(code) = exited
            && log_read
        {
            return OutputStatus::Failed(self.ended_message(code));
        }
        if self.shared.started.load(Ordering::Relaxed) { OutputStatus::Running } else { OutputStatus::Starting }
    }

    fn ended_message(&self, code: std::process::ExitStatus) -> String {
        ended_text(&self.shared, code)
    }

    /// End both inputs and wait for ffmpeg to finish the file or stream.
    pub fn stop(&mut self) -> Result<(), PluginError> {
        if let Some(s) = &self.finished {
            return match s {
                OutputStatus::Failed(e) => Err(PluginError::Failed(e.clone())),
                _ => Ok(()),
            };
        }
        let failed_before = self.status();
        self.shared.stop.store(true, Ordering::Relaxed);
        // The video and audio writers (the first two threads) block while ffmpeg does not read
        // its input. When they have not ended by the deadline, ffmpeg is stopped, which closes
        // the pipes and ends them.
        let writers_running = |threads: &[JoinHandle<()>]| threads.iter().take(2).any(|t| !t.is_finished());
        let writers_deadline = Instant::now() + FINISH_TIMEOUT;
        while writers_running(&self.threads) && Instant::now() < writers_deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let stuck = writers_running(&self.threads);
        if stuck && let Ok(mut c) = self.child.lock() {
            let _ = c.kill();
            let _ = c.wait();
        }
        let mut writers = std::mem::take(&mut self.threads);
        // The log reader ends when ffmpeg exits, so it is joined after the wait below.
        let log_reader = writers.split_off(writers.len().min(2));
        for t in writers {
            let _ = t.join();
        }
        let deadline = Instant::now() + FINISH_TIMEOUT;
        let exited = loop {
            if stuck {
                break Err(PluginError::Failed("ffmpeg did not finish in time and was stopped".into()));
            }
            let waited = self.child.lock().map(|mut c| c.try_wait());
            match waited {
                Err(_) => break Err(PluginError::Failed("ffmpeg's handle was lost".into())),
                Ok(Ok(Some(code))) => break Ok(code),
                Ok(Ok(None)) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
                Ok(Ok(None)) => {
                    if let Ok(mut c) = self.child.lock() {
                        let _ = c.kill();
                        let _ = c.wait();
                    }
                    break Err(PluginError::Failed("ffmpeg did not finish in time and was stopped".into()));
                }
                Ok(Err(e)) => break Err(PluginError::failed(e)),
            }
        };
        // ffmpeg has exited or been stopped here, so the log reader reaches the end of its output.
        for t in log_reader {
            let _ = t.join();
        }
        let result = match exited {
            Ok(code) if code.success() => Ok(()),
            Ok(code) => Err(PluginError::Failed(self.ended_message(code))),
            Err(e) => Err(e),
        };
        // A writer thread can fail while it finishes its queue after the stop.
        let failed_writing = self.shared.failure.lock().ok().and_then(|f| f.clone());
        let result = match (failed_before, failed_writing, result) {
            (OutputStatus::Failed(e), _, _) | (_, Some(e), _) => Err(PluginError::Failed(e)),
            (_, None, r) => r,
        };
        self.finished = Some(match &result {
            Ok(()) => OutputStatus::Stopped,
            Err(e) => OutputStatus::Failed(e.to_string()),
        });
        result
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        if self.finished.is_none() {
            let _ = self.stop();
        }
    }
}

/// Seconds from `start` to `t`, or `None` before `start`.
pub(crate) fn seconds_since(start: Tick, t: Tick) -> Option<f64> {
    (t.0 >= start.0).then(|| duration_of(Tick(t.0 - start.0)).as_secs_f64())
}

/// What to do with a frame that belongs in slot `index` when `next` is the next slot to write.
#[derive(Debug, PartialEq, Eq)]
enum Slot {
    /// Too late: that slot is written.
    Drop,
    /// Write the previous frame `n` more times first.
    Fill(u64),
}

fn slot(index: u64, next: u64, max_fill: u64) -> Slot {
    if index < next { Slot::Drop } else { Slot::Fill((index - next).min(max_fill)) }
}

/// For an audio block that starts `ahead` sample frames after the last one written: the frames
/// of silence to write first, and the frames of the gap past `max_fill` that are skipped.
fn audio_gap(ahead: i64, max_fill: i64) -> (i64, i64) {
    if ahead <= (BLOCK_FRAMES / 2) as i64 {
        return (0, 0);
    }
    let fill = ahead.min(max_fill);
    (fill, ahead - fill)
}

/// For a frame `secs` after the start, when `next` is the next slot to write: `None` when its
/// slot is already written, otherwise the copies of the previous frame to write first and the
/// slot after this frame. A gap is filled up to [`MAX_FILL_S`] past what the audio writer has
/// written, so a gap in the pictures alone is filled whole; the rest is skipped in both writers.
pub(crate) fn video_step(secs: f64, next: u64, fps: f64, timeline: &Timeline) -> Option<(u64, u64)> {
    loop {
        let seen = timeline.read();
        let place = Place::unpack(seen);
        let index = ((secs - place.skipped()) * fps).round();
        if index < 0.0 {
            return None;
        }
        let index = index as u64;
        let own_ns = ns(next as f64 / fps);
        let audio_out = own_ns.saturating_add(place.audio_ahead_ns) as f64 / 1e9;
        let limit = whole((audio_out + MAX_FILL_S) * fps) as u64;
        match slot(index, next, limit.saturating_sub(next)) {
            Slot::Drop => return None,
            Slot::Fill(n) => {
                let excess = index - next - n;
                let after = index - excess + 1;
                let moved = ns(after as f64 / fps).saturating_sub(own_ns);
                if timeline.commit(seen, place.moved(excess as f64 / fps, -moved)) {
                    return Some((n, after));
                }
            }
        }
    }
}

/// For an audio block of `frames` sample frames `secs` after the start, when `written` sample
/// frames are written: `None` when it is too late, otherwise the frames of silence to write
/// first. A gap is filled up to [`MAX_FILL_S`] past what the video writer has written; the rest is
/// skipped in both writers.
pub(crate) fn audio_step(secs: f64, written: i64, frames: i64, rate: f64, timeline: &Timeline) -> Option<i64> {
    loop {
        let seen = timeline.read();
        let place = Place::unpack(seen);
        let pos = ((secs - place.skipped()) * rate).round() as i64;
        let ahead = pos - written;
        if ahead < -(BLOCK_FRAMES as i64) {
            return None;
        }
        let own_ns = ns(written as f64 / rate);
        let video_out = own_ns.saturating_sub(place.audio_ahead_ns) as f64 / 1e9;
        let limit = whole((video_out + MAX_FILL_S) * rate) as i64;
        let (gap, skipped) = audio_gap(ahead, (limit - written).max(0));
        let moved = ns((written + gap + frames) as f64 / rate).saturating_sub(own_ns);
        if timeline.commit(seen, place.moved(skipped as f64 / rate, moved)) {
            return Some(gap);
        }
    }
}

/// Writes the frames to `stdin`, ffmpeg's video input, until a stop.
fn write_video(mut stdin: impl Write, q: &Fifo<Arc<PooledFrame>>, shared: &Shared, stats: &EncoderStats, start: Tick, fps: f64, frame_bytes: usize) {
    let mut next = 0u64;
    let mut last: Option<Arc<PooledFrame>> = None;
    let write = |stdin: &mut dyn Write, f: &PooledFrame| -> bool {
        let ok = stdin.write_all(&f.data).is_ok();
        if ok {
            stats.frames.fetch_add(1, Ordering::Relaxed);
        }
        ok
    };
    loop {
        let Some(frame) = q.pop() else {
            if shared.stop.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(IDLE);
            continue;
        };
        if frame.data.len() != frame_bytes {
            shared.fail(format!("a video frame of {} bytes does not have the output's size ({frame_bytes} bytes)", frame.data.len()));
            return;
        }
        let Some(secs) = seconds_since(start, frame.pts) else { continue };
        let Some((n, after)) = video_step(secs, next, fps, &shared.timeline) else {
            shared.late.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        let fill = last.clone().unwrap_or_else(|| Arc::clone(&frame));
        for _ in 0..n {
            if !write(&mut stdin, &fill) {
                shared.fail("ffmpeg stopped reading video".into());
                return;
            }
        }
        if !write(&mut stdin, &frame) {
            shared.fail("ffmpeg stopped reading video".into());
            return;
        }
        next = after;
        last = Some(frame);
    }
    // Dropping stdin ends ffmpeg's video input.
}

/// Writes the audio blocks to `sink`, the write end of ffmpeg's audio pipe, until a stop.
fn write_audio(mut sink: impl Write, q: &Fifo<PooledBlock>, shared: &Shared, start: Tick) {
    let rate = f64::from(SAMPLE_RATE);
    let mut written: i64 = 0;
    let mut bytes: Vec<u8> = Vec::with_capacity(BLOCK_FRAMES * CHANNELS * 4);
    let silence = vec![0u8; BLOCK_FRAMES * CHANNELS * 4];
    loop {
        let Some(block) = q.pop() else {
            if shared.stop.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(IDLE);
            continue;
        };
        let Some(secs) = seconds_since(start, block.pts) else { continue };
        let frames = (block.samples.len() / CHANNELS) as i64;
        let Some(mut gap) = audio_step(secs, written, frames, rate, &shared.timeline) else { continue };
        while gap > 0 {
            let frames = gap.min(BLOCK_FRAMES as i64) as usize;
            if sink.write_all(&silence[..frames * CHANNELS * 4]).is_err() {
                shared.fail("ffmpeg stopped reading audio".into());
                return;
            }
            written += frames as i64;
            gap -= frames as i64;
        }
        bytes.clear();
        bytes.extend(block.samples.iter().flat_map(|s| s.to_le_bytes()));
        if sink.write_all(&bytes).is_err() {
            shared.fail("ffmpeg stopped reading audio".into());
            return;
        }
        shared.started.store(true, Ordering::Relaxed);
        written += (block.samples.len() / CHANNELS) as i64;
    }
    // Dropping the write end ends ffmpeg's audio input.
    let _ = sink.flush();
}

fn read_log(stderr: impl Read, shared: &Shared, stats: &EncoderStats) {
    for line in BufReader::new(stderr).lines() {
        let Ok(line) = line else { return };
        let line = line.trim();
        match line.split_once('=') {
            Some(("total_size", v)) => {
                if let Ok(n) = v.trim().parse::<u64>() {
                    stats.bytes.store(n, Ordering::Relaxed);
                }
            }
            Some((k, _)) if !k.contains(' ') => {}
            _ if !line.is_empty() => {
                let line = redact(line, &shared.secret);
                log::warn!("ffmpeg: {line}");
                if line.starts_with('[')
                    && let Ok(mut m) = shared.component_message.lock()
                    && m.len() < COMPONENT_LINES
                    && !m.contains(&line)
                {
                    m.push(line.clone());
                }
                if let Ok(mut m) = shared.last_message.lock() {
                    *m = line;
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn late_frames_are_dropped_and_gaps_filled() {
        assert_eq!(slot(5, 5, 100), Slot::Fill(0));
        assert_eq!(slot(4, 5, 100), Slot::Drop);
        assert_eq!(slot(8, 5, 100), Slot::Fill(3));
        assert_eq!(slot(1000, 5, 100), Slot::Fill(100));
    }

    #[test]
    fn a_long_audio_gap_is_filled_up_to_the_cap_and_the_rest_skipped() {
        let block = BLOCK_FRAMES as i64;
        assert_eq!(audio_gap(0, 1000), (0, 0));
        assert_eq!(audio_gap(block / 2, 1000), (0, 0), "a small offset is not a gap");
        assert_eq!(audio_gap(800, 1000), (800, 0));
        assert_eq!(audio_gap(5000, 1000), (1000, 4000));
        // With no video written, a 10 s gap is filled for 5 s and the rest skipped; the next
        // block then follows on, so nothing more is filled.
        let (shared, rate) = (Shared::default(), f64::from(SAMPLE_RATE));
        let limit = (MAX_FILL_S * rate) as i64;
        let mut written = 0i64;
        for (secs, fill) in [(10.0, limit), (10.0 + block as f64 / rate, 0)] {
            assert_eq!(audio_step(secs, written, block, rate, &shared.timeline), Some(fill));
            written += fill + block;
        }
        assert_eq!(written, limit + 2 * block);
        assert!((shared.skipped() - 5.0).abs() < 1e-3, "{}", shared.skipped());
    }

    /// Feeds frames at `frames` (seconds) and audio blocks up to each frame's time and `to`,
    /// leaving out audio between `quiet.0` and `quiet.1`; the seconds of video and audio written.
    fn play(frames: &[f64], quiet: (f64, f64), to: f64, shared: &Shared) -> (f64, f64) {
        let (fps, rate, block) = (30.0, f64::from(SAMPLE_RATE), BLOCK_FRAMES as i64);
        let (mut next, mut written, mut audio_at) = (0u64, 0i64, 0.0);
        let mut audio_until = |t: f64, written: &mut i64| {
            while audio_at <= t {
                if (audio_at < quiet.0 || audio_at >= quiet.1)
                    && let Some(gap) = audio_step(audio_at, *written, block, rate, &shared.timeline)
                {
                    *written += gap + block;
                }
                audio_at += block as f64 / rate;
            }
        };
        for &secs in frames {
            audio_until(secs, &mut written);
            if let Some((_, after)) = video_step(secs, next, fps, &shared.timeline) {
                next = after;
            }
        }
        audio_until(to, &mut written);
        (next as f64 / fps, written as f64 / rate)
    }

    #[test]
    fn a_gap_in_the_pictures_alone_is_filled_whole() {
        let shared = Shared::default();
        // Pictures stop for 12 seconds after the first second while the sound goes on.
        let frames: Vec<f64> = (0..30).map(|i| f64::from(i) / 30.0).chain([13.0]).collect();
        let (video, audio) = play(&frames, (0.0, 0.0), 13.5, &shared);
        // The sound goes on for half a second after the last picture.
        assert!((audio - 0.5 - video).abs() < 0.1, "video {video} s, audio {audio} s");
        assert!(video > 12.9, "{video} s of video");
        assert_eq!(shared.skipped(), 0.0);
    }

    #[test]
    fn writers_that_resume_together_skip_a_gap_once() {
        let (fps, rate, block) = (30.0, f64::from(SAMPLE_RATE), BLOCK_FRAMES as i64);
        for _ in 0..200 {
            let shared = Arc::new(Shared::default());
            // One second of both, then both resume at the same moment after a 12 s gap.
            let (mut next, mut written) = (0u64, 0i64);
            for i in 0..30 {
                next = video_step(f64::from(i) / fps, next, fps, &shared.timeline).map_or(next, |(_, a)| a);
            }
            while (written as f64 / rate) < 1.0 {
                written += audio_step(written as f64 / rate, written, block, rate, &shared.timeline).unwrap_or(0) + block;
            }
            let start = Arc::new(std::sync::Barrier::new(2));
            let (s, b) = (Arc::clone(&shared), Arc::clone(&start));
            let video = std::thread::spawn(move || {
                b.wait();
                video_step(13.0, next, fps, &s.timeline)
            });
            start.wait();
            let _ = audio_step(13.0, written, block, rate, &shared.timeline);
            let _ = video.join().unwrap();
            assert!((shared.skipped() - 7.0).abs() < 0.1, "skipped {} s of a 12 s gap", shared.skipped());
        }
    }

    #[test]
    fn a_step_placed_before_the_other_writer_skipped_is_placed_again() {
        let (fps, rate, block) = (30.0, f64::from(SAMPLE_RATE), BLOCK_FRAMES as i64);
        let timeline = Timeline::default();
        // The sound has 4 seconds written and no picture has come yet.
        let mut written = 0i64;
        while written < 4 * i64::from(SAMPLE_RATE) {
            written += audio_step(written as f64 / rate, written, block, rate, &timeline).unwrap() + block;
        }
        // Both resume at 13 s: the audio writer reads the timeline, and then the video writer
        // fills 5 seconds past the sound and skips the other 4 before the audio writer commits.
        let seen = timeline.read();
        let fill = ((4.0 + MAX_FILL_S) * fps) as u64;
        assert_eq!(video_step(13.0, 0, fps, &timeline), Some((fill, fill + 1)));
        assert!((timeline.skipped() - 4.0).abs() < 1e-3, "{}", timeline.skipped());
        let late = Place::unpack(seen).moved(4.0, 1_000_000_000);
        assert!(!timeline.commit(seen, late), "a commit from the earlier read is refused");
        assert!((timeline.skipped() - 4.0).abs() < 1e-3, "a refused commit changes nothing: {}", timeline.skipped());
        // Placed again, the block sees the skip and where the pictures are now, so the gap up to
        // the block (about 5 seconds) is filled and nothing more is skipped.
        assert_eq!(audio_step(13.0, written, block, rate, &timeline), Some(9 * i64::from(SAMPLE_RATE) - written));
        assert!((timeline.skipped() - 4.0).abs() < 1e-3, "the gap is skipped once: {}", timeline.skipped());
    }

    #[test]
    fn a_gap_that_ends_on_the_fill_limit_is_filled_whole() {
        let (fps, rate) = (30.0, f64::from(SAMPLE_RATE));
        let timeline = Timeline::default();
        // The sound has one frame's time written (1,600 samples), so pictures are filled up to
        // 5 seconds past it: 151 frames.
        assert_eq!(audio_step(0.0, 0, 1600, rate, &timeline), Some(0));
        assert_eq!(video_step(5.0 + 1.0 / fps, 0, fps, &timeline), Some((151, 152)));
        assert_eq!(timeline.skipped(), 0.0);
        // The same at the sound's side: 31 frames of pictures, so sound is filled up to 5 seconds
        // past them: 289,600 samples.
        let timeline = Timeline::default();
        assert_eq!(video_step(30.0 / fps, 0, fps, &timeline), Some((30, 31)));
        assert_eq!(audio_step(6.0 + 1.0 / fps, 0, 1024, rate, &timeline), Some(289_600));
        assert_eq!(timeline.skipped(), 0.0);
    }

    #[test]
    fn skipped_time_keeps_its_fractions_and_range() {
        let fps = 30.0;
        let mut place = Place::unpack(0);
        for _ in 0..1000 {
            place = place.moved(1.0 / fps, 0);
        }
        assert!((place.skipped() - 1000.0 / fps).abs() < 1e-6, "1000 skips of one frame: {} s", place.skipped());
        let days = 60.0 * 86_400.0;
        assert!((place.moved(days, 0).skipped() - place.skipped() - days).abs() < 1e-3, "a skip of 60 days is counted whole");
        let far = Place { skipped_ns: u64::MAX - 1, audio_ahead_ns: -5 };
        assert_eq!(Place::unpack(far.pack()), far);
    }

    #[test]
    fn a_gap_in_both_is_skipped_in_both() {
        let shared = Shared::default();
        // Pictures and sound both stop for 12 seconds after the first second.
        let frames: Vec<f64> = (0..30).map(|i| f64::from(i) / 30.0).chain([13.0]).collect();
        let (video, audio) = play(&frames, (1.0, 13.0), 13.5, &shared);
        assert!((audio - 0.5 - video).abs() < 0.1, "video {video} s, audio {audio} s");
        assert!(video < 7.0, "{video} s of video");
        assert!((shared.skipped() - 7.0).abs() < 0.1, "{}", shared.skipped());
    }

    #[test]
    fn the_stream_key_is_hidden_in_what_ffmpeg_reports() {
        let shared = Shared { secret: "live_abc123".into(), ..Shared::default() };
        let stats = EncoderStats::default();
        read_log(&b"rtmp://x/app/live_abc123: Connection refused\n"[..], &shared, &stats);
        assert_eq!(shared.last_message.lock().unwrap().as_str(), "rtmp://x/app/***: Connection refused");
        assert_eq!(redact("no key here", ""), "no key here");
    }

    #[test]
    fn audio_reaches_ffmpeg_on_a_pipe_and_no_port_is_opened() {
        let x264 = crate::encoders::all().into_iter().next().unwrap();
        let job = OutputJob {
            video: studiocraft_plugin::VideoFormat { width: 64, height: 48, fps_num: 10, fps_den: 1, pixel: PixelFormat::Nv12 },
            sample_rate: SAMPLE_RATE,
            video_encoder: Arc::new(x264),
            video_encoder_settings: studiocraft_plugin::Settings::new(),
            rate: studiocraft_plugin::RateControl::Bitrate { kbps: 500, keyframe_s: 2 },
            audio_bitrate_kbps: 128,
            ffmpeg: "ffmpeg".into(),
            stats: Arc::new(EncoderStats::default()),
            start: Tick(0),
        };
        let args = input_args(&job);
        let inputs: Vec<&str> = args.windows(2).filter(|w| w[0] == "-i").map(|w| w[1].as_str()).collect();
        assert_eq!(inputs, ["pipe:0", "pipe:1"]);
        assert!(!args.iter().any(|a| a.contains("tcp:") || a.contains("udp:") || a.contains("127.0.0.1")), "{args:?}");
    }

    #[test]
    fn audio_is_written_to_the_pipe_it_is_given() {
        let shared = Shared::default();
        let q = Fifo::new(4);
        let blocks = studiocraft_media::audio::block_pool();
        let mut block = blocks.take();
        block.samples.clear();
        block.samples.extend((0..BLOCK_FRAMES * CHANNELS).map(|i| i as f32));
        block.pts = Tick(0);
        q.push(block);
        shared.stop.store(true, Ordering::Relaxed);
        let mut sink = Vec::new();
        write_audio(&mut sink, &q, &shared, Tick(0));
        assert_eq!(sink.len(), BLOCK_FRAMES * CHANNELS * 4);
        assert_eq!(sink[4..8], 1.0f32.to_le_bytes());
        assert!(shared.started.load(Ordering::Relaxed));
    }

    #[test]
    fn a_frame_of_the_wrong_size_fails_the_recording_and_is_not_written() {
        let shared = Shared::default();
        let stats = EncoderStats::default();
        let q = Fifo::new(4);
        let frames = studiocraft_media::pool::Pool::new(4, studiocraft_media::video::VideoFrame::default);
        for (n, (w, h)) in [(64u32, 48u32), (32, 24)].into_iter().enumerate() {
            let mut f = frames.take();
            assert!(f.reshape(w, h, PixelFormat::Nv12));
            f.pts = studiocraft_media::clock::tick_of(Duration::from_millis(100 * n as u64));
            q.push(Arc::new(f));
        }
        shared.stop.store(true, Ordering::Relaxed);
        let frame_bytes = PixelFormat::Nv12.frame_bytes(64, 48).unwrap();
        let mut sink = Vec::new();
        write_video(&mut sink, &q, &shared, &stats, Tick(0), 10.0, frame_bytes);
        assert_eq!(sink.len(), frame_bytes, "only the frame of the output's size is written");
        let failure = shared.failure.lock().unwrap().clone().unwrap_or_default();
        assert!(failure.contains("does not have the output's size"), "{failure:?}");
    }

    #[test]
    fn times_before_the_start_are_skipped() {
        assert_eq!(seconds_since(Tick(100), Tick(50)), None);
        assert_eq!(seconds_since(Tick(100), Tick(100)), Some(0.0));
    }

    #[test]
    fn progress_lines_update_the_byte_count_and_messages_are_kept() {
        let shared = Shared::default();
        let stats = EncoderStats::default();
        let text = "frame=10\ntotal_size=4096\nprogress=continue\nrtmp://x: Connection refused\n";
        read_log(text.as_bytes(), &shared, &stats);
        assert_eq!(stats.bytes.load(Ordering::Relaxed), 4096);
        assert_eq!(*shared.last_message.lock().unwrap(), "rtmp://x: Connection refused");
        assert_eq!(ended_text(&shared, 1), "ffmpeg stopped: rtmp://x: Connection refused");
    }

    #[test]
    fn the_encoders_line_is_kept_when_ffmpeg_ends_with_a_general_one() {
        let shared = Shared::default();
        let text = "[in#0 @ 0x3] Error during demuxing\n[h264_nvenc @ 0x1] Failed locking bitstream buffer: out of memory\n[out#0/mp4 @ 0x2] Error writing trailer\nConversion failed!\n";
        read_log(text.as_bytes(), &shared, &EncoderStats::default());
        let components = "[in#0 @ 0x3] Error during demuxing; [h264_nvenc @ 0x1] Failed locking bitstream buffer: out of memory; [out#0/mp4 @ 0x2] Error writing trailer";
        assert_eq!(ended_text(&shared, 1), format!("ffmpeg stopped: {components}; Conversion failed!"));
        assert_eq!(ended_text(&Shared::default(), 1), "ffmpeg stopped unexpectedly (1)");
        assert_eq!(
            failure_text(&shared, "ffmpeg stopped reading video".into()),
            format!("ffmpeg stopped reading video: {components}; Conversion failed!")
        );
        let plain = Shared::default();
        read_log(b"Cannot load nvcuda.dll\n".as_slice(), &plain, &EncoderStats::default());
        assert_eq!(failure_text(&plain, "ffmpeg stopped reading video".into()), "ffmpeg stopped reading video: Cannot load nvcuda.dll");
        let many = Shared::default();
        let log: String = (0..40).map(|n| format!("[c{n} @ 1] e\n[c0 @ 1] e\n")).collect();
        read_log(log.as_bytes(), &many, &EncoderStats::default());
        let kept = many.component_message.lock().unwrap().clone();
        assert_eq!(kept.len(), COMPONENT_LINES, "a repeated line is kept once");
        assert_eq!(kept[1], "[c1 @ 1] e");
        assert_eq!(failure_text(&Shared::default(), "ffmpeg stopped reading audio".into()), "ffmpeg stopped reading audio");
    }

    #[test]
    fn a_failure_waits_for_the_log_thread_to_read_ffmpegs_last_lines() {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let writer = std::thread::spawn(|| {});
        let log = std::thread::spawn(move || {
            let _ = rx.recv();
        });
        while !writer.is_finished() {
            std::thread::yield_now();
        }
        let threads = vec![writer, log];
        assert!(!log_read(&threads), "the log thread still reads");
        let _ = tx.send(());
        while !threads[1].is_finished() {
            std::thread::yield_now();
        }
        assert!(log_read(&threads));
        assert!(log_read(&[]));
    }
}
