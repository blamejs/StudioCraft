//! The ffmpeg process behind a recording or a stream.
//!
//! ffmpeg reads NV12 frames on its standard input and 48 kHz stereo `f32` samples over a
//! localhost TCP connection it opens to StudioCraft. Both inputs are timed by count (frames at
//! the output rate, samples at 48 kHz), so the writer threads keep the counts in step with the
//! master clock: a missing frame is filled by repeating the previous one, a late frame is
//! dropped, and a gap in the audio is filled with silence.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, ChildStdin, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

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
/// How long ffmpeg may take to connect for audio.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// How long ffmpeg may take to finish its file or stream after the inputs end.
const FINISH_TIMEOUT: Duration = Duration::from_secs(20);
/// Idle wait of the writer threads when their queue is empty.
const IDLE: Duration = Duration::from_millis(2);

#[derive(Default)]
struct Shared {
    stop: AtomicBool,
    /// ffmpeg has exited, so it will not connect for audio any more.
    exited: AtomicBool,
    connected: AtomicBool,
    /// Late frames dropped by the writer (queue overflows are counted by the queue).
    late: AtomicU64,
    failure: Mutex<Option<String>>,
    /// The last line ffmpeg reported that was not progress.
    last_message: Mutex<String>,
    /// Text never logged or shown: the stream key.
    secret: String,
    /// Where the two writers are. Each writer places and fills under this lock, so a gap they
    /// both see is skipped once.
    timeline: Mutex<Timeline>,
}

/// Seconds of output each writer has written, and seconds of long gaps skipped so far. Both
/// writers place what they write `skipped` seconds earlier, so a gap that one of them skips is
/// skipped in both and they stay in step.
#[derive(Default)]
struct Timeline {
    skipped: f64,
    video_out: f64,
    audio_out: f64,
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

    /// The writers' timeline, also after a writer thread panicked while holding it.
    fn timeline(&self) -> std::sync::MutexGuard<'_, Timeline> {
        self.timeline.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Seconds skipped so far.
    #[cfg(test)]
    fn skipped(&self) -> f64 {
        self.timeline().skipped
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

/// The arguments before the codec options: both inputs and the stream mapping.
pub fn input_args(job: &OutputJob, audio_port: u16) -> Vec<String> {
    let v = job.video;
    let mut a: Vec<String> =
        ["-hide_banner", "-nostdin", "-y", "-loglevel", "error", "-progress", "pipe:2", "-stats_period", "0.5"].map(String::from).to_vec();
    a.extend(["-f", "rawvideo", "-pix_fmt", PixelFormat::Nv12.ffmpeg_name()].map(String::from));
    a.extend(["-video_size".into(), format!("{}x{}", v.width, v.height), "-framerate".into(), format!("{}/{}", v.fps_num, v.fps_den.max(1))]);
    a.extend(["-thread_queue_size", "64", "-i", "pipe:0"].map(String::from));
    a.extend(["-f".into(), "f32le".into(), "-ar".into(), job.sample_rate.to_string(), "-ac".into(), CHANNELS.to_string()]);
    a.extend(["-thread_queue_size".into(), "64".into(), "-i".into(), format!("tcp://127.0.0.1:{audio_port}")]);
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
        let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(|e| PluginError::Failed(format!("no local port for audio: {e}")))?;
        let port = listener.local_addr().map_err(PluginError::failed)?.port();
        let mut cmd = ffmpeg::command(&job.ffmpeg);
        cmd.args(input_args(job, port)).args(codec_args).args(output_args).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| PluginError::Failed(format!("ffmpeg could not be started: {e}")))?;
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
            spawn("encoder audio", Box::new(move || write_audio(&listener, &q, &sh, start)))
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
                shared.exited.store(true, Ordering::Relaxed);
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
        if let Some(f) = self.shared.failure.lock().ok().and_then(|f| f.clone()) {
            return OutputStatus::Failed(f);
        }
        let exited = self.child.try_lock().ok().and_then(|mut c| c.try_wait().ok().flatten());
        if let Some(code) = exited {
            return OutputStatus::Failed(self.ended_message(code));
        }
        if self.shared.connected.load(Ordering::Relaxed) { OutputStatus::Running } else { OutputStatus::Starting }
    }

    fn ended_message(&self, code: std::process::ExitStatus) -> String {
        let last = self.shared.last_message.lock().map(|m| m.clone()).unwrap_or_default();
        if last.is_empty() { format!("ffmpeg stopped unexpectedly ({code})") } else { format!("ffmpeg stopped: {last}") }
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
            if self.child.try_lock().ok().and_then(|mut c| c.try_wait().ok().flatten()).is_some() {
                self.shared.exited.store(true, Ordering::Relaxed);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let stuck = writers_running(&self.threads);
        if stuck {
            self.shared.exited.store(true, Ordering::Relaxed);
        }
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
        let result = match (failed_before, result) {
            (OutputStatus::Failed(e), _) => Err(PluginError::Failed(e)),
            (_, r) => r,
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
fn seconds_since(start: Tick, t: Tick) -> Option<f64> {
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
fn video_step(secs: f64, next: u64, fps: f64, shared: &Shared) -> Option<(u64, u64)> {
    let mut tl = shared.timeline();
    let index = ((secs - tl.skipped) * fps).round();
    if index < 0.0 {
        return None;
    }
    let index = index as u64;
    let limit = ((tl.audio_out + MAX_FILL_S) * fps) as u64;
    match slot(index, next, limit.saturating_sub(next)) {
        Slot::Drop => None,
        Slot::Fill(n) => {
            let excess = index - next - n;
            tl.skipped += excess as f64 / fps;
            let after = index - excess + 1;
            tl.video_out = after as f64 / fps;
            Some((n, after))
        }
    }
}

/// For an audio block of `frames` sample frames `secs` after the start, when `written` sample
/// frames are written: `None` when it is too late, otherwise the frames of silence to write
/// first. A gap is filled up to [`MAX_FILL_S`] past what the video writer has written; the rest is
/// skipped in both writers.
fn audio_step(secs: f64, written: i64, frames: i64, rate: f64, shared: &Shared) -> Option<i64> {
    let mut tl = shared.timeline();
    let pos = ((secs - tl.skipped) * rate).round() as i64;
    let ahead = pos - written;
    if ahead < -(BLOCK_FRAMES as i64) {
        return None;
    }
    let limit = ((tl.video_out + MAX_FILL_S) * rate) as i64;
    let (gap, skipped) = audio_gap(ahead, (limit - written).max(0));
    tl.skipped += skipped as f64 / rate;
    tl.audio_out = (written + gap + frames) as f64 / rate;
    Some(gap)
}

fn write_video(mut stdin: ChildStdin, q: &Fifo<Arc<PooledFrame>>, shared: &Shared, stats: &EncoderStats, start: Tick, fps: f64, frame_bytes: usize) {
    let mut next = 0u64;
    let mut last: Option<Arc<PooledFrame>> = None;
    let write = |stdin: &mut ChildStdin, f: &PooledFrame| -> bool {
        if f.data.len() != frame_bytes {
            return true;
        }
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
        let Some(secs) = seconds_since(start, frame.pts) else { continue };
        let Some((n, after)) = video_step(secs, next, fps, shared) else {
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

fn accept(listener: &TcpListener, shared: &Shared) -> Option<TcpStream> {
    listener.set_nonblocking(true).ok()?;
    // A stop does not end the wait: ffmpeg needs the audio connection to finish a short file.
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    while Instant::now() < deadline && !shared.exited.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((s, _)) => {
                let _ = s.set_nonblocking(false);
                let _ = s.set_nodelay(true);
                return Some(s);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(5)),
            Err(_) => return None,
        }
    }
    None
}

fn write_audio(listener: &TcpListener, q: &Fifo<PooledBlock>, shared: &Shared, start: Tick) {
    let Some(mut sock) = accept(listener, shared) else {
        if !shared.stop.load(Ordering::Relaxed) {
            shared.fail("ffmpeg did not connect for audio".into());
        }
        return;
    };
    shared.connected.store(true, Ordering::Relaxed);
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
        let Some(mut gap) = audio_step(secs, written, frames, rate, shared) else { continue };
        while gap > 0 {
            let frames = gap.min(BLOCK_FRAMES as i64) as usize;
            if sock.write_all(&silence[..frames * CHANNELS * 4]).is_err() {
                shared.fail("ffmpeg stopped reading audio".into());
                return;
            }
            written += frames as i64;
            gap -= frames as i64;
        }
        bytes.clear();
        bytes.extend(block.samples.iter().flat_map(|s| s.to_le_bytes()));
        if sock.write_all(&bytes).is_err() {
            shared.fail("ffmpeg stopped reading audio".into());
            return;
        }
        written += (block.samples.len() / CHANNELS) as i64;
    }
    let _ = sock.flush();
    let _ = sock.shutdown(std::net::Shutdown::Write);
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
            assert_eq!(audio_step(secs, written, block, rate, &shared), Some(fill));
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
                    && let Some(gap) = audio_step(audio_at, *written, block, rate, shared)
                {
                    *written += gap + block;
                }
                audio_at += block as f64 / rate;
            }
        };
        for &secs in frames {
            audio_until(secs, &mut written);
            if let Some((_, after)) = video_step(secs, next, fps, shared) {
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
                next = video_step(f64::from(i) / fps, next, fps, &shared).map_or(next, |(_, a)| a);
            }
            while (written as f64 / rate) < 1.0 {
                written += audio_step(written as f64 / rate, written, block, rate, &shared).unwrap_or(0) + block;
            }
            let start = Arc::new(std::sync::Barrier::new(2));
            let (s, b) = (Arc::clone(&shared), Arc::clone(&start));
            let video = std::thread::spawn(move || {
                b.wait();
                video_step(13.0, next, fps, &s)
            });
            start.wait();
            let _ = audio_step(13.0, written, block, rate, &shared);
            let _ = video.join().unwrap();
            assert!((shared.skipped() - 7.0).abs() < 0.1, "skipped {} s of a 12 s gap", shared.skipped());
        }
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
    }
}
