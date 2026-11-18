//! A video or audio file, played in real time.
//!
//! An Ogg file that holds one Vorbis stream and nothing else plays in this process: a player thread
//! reads it packet by packet with `craft-audio`'s Vorbis decoder and publishes the samples at the file's own speed.
//! Every other file plays through ffmpeg. Two ffmpeg processes read the file at its own speed
//! (`-re`): one writes RGBA pictures (PAM images, which carry their size) and one writes 48 kHz
//! stereo samples. A reader thread for each publishes into the source's outputs.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use craft_audio::vorbis::decoder::Decoder;
use craft_media::FileReader;
use craft_media::ogg::{self, Codec, OggFile};
use studiocraft_media::audio::SAMPLE_RATE;
use studiocraft_media::clock::MasterClock;
use studiocraft_media::video::PixelFormat;
use studiocraft_model::{Field, FieldKind, PathMode, Settings, SettingsSchema};
use studiocraft_platform::ffmpeg;
use studiocraft_plugin::{AudioOut, Pace, Plugin, PluginError, Source, SourceCaps, SourceInstance, SourceIo, VideoContent, VideoOut};

use crate::{AudioFeed, FfmpegSetting};

pub struct MediaSource {
    ffmpeg: FfmpegSetting,
}

impl MediaSource {
    pub fn new(ffmpeg: FfmpegSetting) -> Self {
        Self { ffmpeg }
    }
}

impl Plugin for MediaSource {
    fn id(&self) -> &'static str {
        "media"
    }
    fn name(&self) -> &'static str {
        "Media file"
    }
    fn schema(&self) -> SettingsSchema {
        let extensions =
            ["mp4", "mkv", "mov", "webm", "avi", "flv", "ts", "mp3", "wav", "flac", "ogg", "m4a", "aac", "gif"].map(String::from).to_vec();
        SettingsSchema::new(vec![
            Field::new("file", "Media file", FieldKind::Path { mode: PathMode::OpenFile, filter_name: "Media files".into(), extensions }, ""),
            Field::new("loop", "Loop", FieldKind::Bool, false),
        ])
    }
}

impl Source for MediaSource {
    fn caps(&self) -> SourceCaps {
        SourceCaps { video: true, audio: true }
    }
    fn create(&self, settings: &Settings, io: &SourceIo) -> Result<Box<dyn SourceInstance>, PluginError> {
        let configured = self.ffmpeg.read().map(|s| s.clone()).unwrap_or_default();
        // An Ogg Vorbis file plays without ffmpeg, so a missing ffmpeg is reported only for the
        // files that need it.
        let program = ffmpeg::locate(&configured).map_err(|e| e.to_string());
        let mut i = Instance::new(program);
        i.restart(settings, io)?;
        Ok(Box::new(i))
    }
}

struct Instance {
    /// The ffmpeg program, or why it was not found.
    program: Result<PathBuf, String>,
    file: String,
    looping: bool,
    children: Vec<Child>,
    /// The threads reading the children's output. They are joined when the children stop, so
    /// nothing read from an earlier file is published after the next file starts.
    readers: Vec<JoinHandle<()>>,
    /// The Ogg Vorbis player of the file playing now, and what it shares with this instance.
    player_thread: Option<JoinHandle<()>>,
    player: Arc<PlayerState>,
    /// The settings of a restart that failed, tried again on the next produce.
    retry: Option<Settings>,
    /// Why ffmpeg, or the Ogg Vorbis player, ended before the file did.
    failure: Option<String>,
    /// The ffmpeg processes started for the file, and how many of them ended with an error. A
    /// file without video, or without sound, ends one of them at once.
    started: usize,
    ended_with_error: usize,
}

impl Drop for Instance {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Instance {
    fn new(program: Result<PathBuf, String>) -> Self {
        Instance {
            program,
            file: String::new(),
            looping: false,
            children: Vec::new(),
            readers: Vec::new(),
            player_thread: None,
            player: Arc::default(),
            retry: None,
            failure: None,
            started: 0,
            ended_with_error: 0,
        }
    }

    fn stop(&mut self) {
        for mut c in self.children.drain(..) {
            let _ = c.kill();
            let _ = c.wait();
        }
        // An ended process closes its output, so each reader reaches the end and returns.
        for r in self.readers.drain(..) {
            let _ = r.join();
        }
        // The player sees the flag before its next packet, or wakes from its wait to see it. A
        // player still indexing its file is not waited for: it checks the flag once the index is
        // read and ends without publishing anything.
        self.player.stopping.store(true, Ordering::SeqCst);
        let indexing = self.player.indexing.load(Ordering::SeqCst);
        if let Some(p) = self.player_thread.take() {
            p.thread().unpark();
            if !indexing {
                let _ = p.join();
            }
        }
    }

    fn restart(&mut self, s: &Settings, io: &SourceIo) -> Result<(), PluginError> {
        self.failure = None;
        self.ended_with_error = 0;
        // The picture of the file played before is not shown while the next one starts, or
        // when the next one has no video.
        self.stop();
        io.video.publish(VideoContent::Empty);
        let r = self.start(s, io);
        self.started = self.children.len();
        self.retry = r.is_err().then(|| s.clone());
        r
    }

    fn start(&mut self, s: &Settings, io: &SourceIo) -> Result<(), PluginError> {
        self.stop();
        self.player = Arc::default();
        self.file = s.str("file").unwrap_or_default().to_string();
        self.looping = s.bool("loop").unwrap_or(false);
        if self.file.trim().is_empty() {
            io.video.publish(VideoContent::Empty);
            return Ok(());
        }
        let path = Path::new(&self.file);
        if !path.is_file() {
            return Err(PluginError::Failed(format!("{} was not found", path.display())));
        }
        let path = path.to_path_buf();
        if is_ogg_vorbis(&path) {
            return self.start_player(path, io);
        }
        self.start_ffmpeg(&path, io)
    }

    /// Play the file through two ffmpeg processes and their reader threads.
    fn start_ffmpeg(&mut self, path: &Path, io: &SourceIo) -> Result<(), PluginError> {
        let program = self.program.clone().map_err(PluginError::Unavailable)?;
        let video = self.spawn(&program, path, &["-map", "0:v:0?", "-an", "-f", "image2pipe", "-c:v", "pam", "-pix_fmt", "rgba", "pipe:1"])?;
        self.children.push(video);
        let rate = SAMPLE_RATE.to_string();
        match self.spawn(&program, path, &["-map", "0:a:0?", "-vn", "-f", "f32le", "-ac", "2", "-ar", &rate, "pipe:1"]) {
            Ok(audio) => self.children.push(audio),
            Err(e) => {
                // Stop the video process that already started.
                self.stop();
                return Err(e);
            }
        }
        for (i, is_video) in [(0, true), (1, false)] {
            if let Some(out) = self.children.get_mut(i).and_then(|c| c.stdout.take()) {
                let (v, a, clock) = (Arc::clone(&io.video), Arc::clone(&io.audio), io.clock);
                let name = if is_video { "media video reader" } else { "media audio reader" };
                let started = std::thread::Builder::new().name(name.into()).spawn(move || {
                    if is_video {
                        read_pictures(out, &v, clock);
                    } else {
                        read_samples(out, &a, clock);
                    }
                });
                match started {
                    Ok(handle) => self.readers.push(handle),
                    Err(e) => {
                        self.stop();
                        return Err(PluginError::failed(e));
                    }
                }
            }
        }
        Ok(())
    }

    /// Play an Ogg Vorbis file on a thread of its own, which indexes it, decodes it packet by
    /// packet and publishes the samples at the file's own speed. A file that turns out to hold
    /// more than its one Vorbis stream (a chained file, or one with other streams later on) is
    /// left to ffmpeg on the next produce.
    fn start_player(&mut self, path: PathBuf, io: &SourceIo) -> Result<(), PluginError> {
        let state = Arc::clone(&self.player);
        let (looping, audio, clock) = (self.looping, Arc::clone(&io.audio), io.clock);
        state.indexing.store(true, Ordering::SeqCst);
        let started = std::thread::Builder::new().name("media vorbis player".into()).spawn(move || {
            let opened = VorbisFile::open(&path);
            state.indexing.store(false, Ordering::SeqCst);
            if state.stopping.load(Ordering::SeqCst) {
                return;
            }
            let played = match opened {
                Ok(Some(mut file)) => {
                    // Playing time starts once the file is indexed.
                    let begin = Instant::now();
                    let mut feed = AudioFeed::new(file.sample_rate(), 2, audio, clock);
                    file.play(looping, &mut |at| wait_until(&state.stopping, begin, at), &mut |s| feed.push(s))
                }
                Ok(None) => {
                    let _ = state.end.set(PlayerEnd::NeedsFfmpeg);
                    Ok(())
                }
                Err(e) => Err(e),
            };
            if let Err(e) = played {
                let _ = state.end.set(PlayerEnd::Failed(format!("{} could not be played: {e}", path.display())));
            }
        });
        match started {
            Ok(handle) => self.player_thread = Some(handle),
            Err(e) => {
                self.player.indexing.store(false, Ordering::SeqCst);
                return Err(PluginError::failed(e));
            }
        }
        Ok(())
    }

    fn spawn(&self, program: &Path, input: &Path, output: &[&str]) -> Result<Child, PluginError> {
        let mut cmd = ffmpeg::command(program);
        cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-re"]);
        if self.looping {
            cmd.args(["-stream_loop", "-1"]);
        }
        cmd.arg("-i").arg(input).args(output).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
        cmd.spawn().map_err(|e| PluginError::Failed(format!("ffmpeg could not be started: {e}")))
    }
}

impl SourceInstance for Instance {
    fn pace(&self) -> Pace {
        // The reader threads publish; the source thread checks ffmpeg once a second.
        Pace::Every(Duration::from_secs(1))
    }
    fn produce(&mut self, io: &SourceIo) -> Result<(), PluginError> {
        // A file that was missing, or an ffmpeg that did not start, is tried again.
        if let Some(s) = self.retry.take() {
            return self.restart(&s, io);
        }
        // When every ffmpeg ended with an error, none could read the file; the error stays until
        // the settings change. One alone ends with an error for a file without its stream.
        let (mut ended, mut errors) = (None, 0);
        self.children.retain_mut(|c| match c.try_wait() {
            Ok(Some(status)) if !status.success() => {
                ended = Some(status);
                errors += 1;
                false
            }
            _ => true,
        });
        if let Some(status) = ended {
            self.ended_with_error += errors;
            if self.ended_with_error >= self.started {
                self.failure = Some(format!("ffmpeg could not read {} ({status})", self.file));
            }
        }
        match self.player.end.get() {
            Some(PlayerEnd::Failed(e)) => self.failure = Some(e.clone()),
            Some(PlayerEnd::NeedsFfmpeg) => {
                // The player has ended; ffmpeg plays the file from here.
                if let Some(p) = self.player_thread.take() {
                    let _ = p.join();
                }
                self.player = Arc::default();
                let path = PathBuf::from(&self.file);
                let started = self.start_ffmpeg(&path, io);
                self.started = self.children.len();
                if let Err(e) = started {
                    self.failure = Some(e.to_string());
                    return Err(e);
                }
            }
            None => {}
        }
        self.failure.clone().map_or(Ok(()), |e| Err(PluginError::Failed(e)))
    }
    fn update(&mut self, s: &Settings, io: &SourceIo) -> Result<(), PluginError> {
        let file = s.str("file").unwrap_or_default();
        let looping = s.bool("loop").unwrap_or(false);
        if file != self.file || looping != self.looping || self.retry.is_some() {
            self.restart(s, io)?;
        }
        Ok(())
    }
}

/// Bytes read from the start of a file to find its first Ogg streams. The streams multiplexed at
/// the start of a file each begin with a page of their own before any other page (RFC 3533 §4), and
/// a Vorbis stream's first page holds only its 30-byte identification header.
const OGG_HEAD: u64 = 64 << 10;

/// Whether the file is an Ogg file whose first streams are all Vorbis, from the first page of each
/// stream at its start. A file with video, or any other stream there, plays through ffmpeg. The
/// player checks the whole file again once it has indexed it, for streams chained later on.
fn is_ogg_vorbis(path: &Path) -> bool {
    let mut head = Vec::new();
    let read = std::fs::File::open(path).and_then(|f| f.take(OGG_HEAD).read_to_end(&mut head));
    read.is_ok() && only_vorbis_streams(&head)
}

/// Whether `head` starts with the first pages of one or more Ogg streams, all of them Vorbis.
fn only_vorbis_streams(head: &[u8]) -> bool {
    let (mut at, mut streams) = (0usize, 0usize);
    // A page: "OggS", version 0, flags (2 marks a stream's first page), granule, serial,
    // sequence, checksum, then the segment count and table at 26 and 27 (RFC 3533 §6).
    while let Some(page) = head.get(at..).filter(|p| p.len() >= 27 && p.starts_with(b"OggS\0")) {
        let first_of_stream = page.get(5).is_some_and(|f| f & 2 != 0);
        if !first_of_stream {
            break;
        }
        let segments = page.get(26).map_or(0, |&n| usize::from(n));
        let Some(table) = page.get(27..27 + segments) else { return false };
        let body = 27 + segments;
        if !page.get(body..).is_some_and(|b| b.starts_with(b"\x01vorbis")) {
            return false;
        }
        streams += 1;
        at = at.saturating_add(body).saturating_add(table.iter().map(|&n| usize::from(n)).sum());
    }
    streams > 0
}

/// The largest Vorbis audio packet the player reads. A larger one is skipped as damaged, unread.
const MAX_PACKET_BYTES: u32 = 8 << 20;

/// What an Ogg Vorbis player shares with its source instance. Each file started gets a new one.
#[derive(Default)]
struct PlayerState {
    /// Set to stop the player.
    stopping: AtomicBool,
    /// Set while the player reads the file's index; it publishes nothing until it is done.
    indexing: AtomicBool,
    /// Why the player ended before the file did.
    end: OnceLock<PlayerEnd>,
}

enum PlayerEnd {
    /// The file could not be read or decoded.
    Failed(String),
    /// The file holds more than one stream, and plays through ffmpeg instead.
    NeedsFfmpeg,
}

/// Wait until `at` after `begin`. `false` when the player is to stop.
fn wait_until(stopping: &AtomicBool, begin: Instant, at: Duration) -> bool {
    loop {
        if stopping.load(Ordering::Acquire) {
            return false;
        }
        match at.checked_sub(begin.elapsed()) {
            Some(left) if !left.is_zero() => std::thread::park_timeout(left),
            _ => return true,
        }
    }
}

/// An indexed Ogg Vorbis file and the decoder of its first Vorbis stream.
struct VorbisFile {
    src: FileReader,
    ogg: OggFile,
    stream: usize,
    decoder: Decoder,
    /// The stream's length in samples per channel, from its last granule position.
    length: Option<u64>,
    downmix: Downmix,
}

impl VorbisFile {
    /// Index the file and read the three header packets. Only the index and one packet at a time
    /// are held in memory; `craft-media` bounds the index by the file's size. `None` when the file
    /// holds any stream besides one Vorbis stream.
    fn open(path: &Path) -> Result<Option<Self>, String> {
        let src = FileReader::open(path).map_err(|e| e.to_string())?;
        let ogg = ogg::open(&src).map_err(|e| e.to_string())?;
        let stream = 0;
        let Some(s) = ogg.streams.first().filter(|s| ogg.streams.len() == 1 && s.codec == Codec::Vorbis) else { return Ok(None) };
        let [id, comment, setup, ..] = s.headers.as_slice() else { return Err("the Vorbis stream ends before its setup header".into()) };
        let decoder = Decoder::new(id, comment, setup).map_err(|e| e.to_string())?;
        let length = s.last_granule.and_then(|g| u64::try_from(g).ok());
        let downmix = Downmix::new(usize::from(decoder.identification().channels));
        Ok(Some(VorbisFile { src, ogg, stream, decoder, length, downmix }))
    }

    fn sample_rate(&self) -> u32 {
        self.decoder.identification().sample_rate
    }

    /// Decode the file from the start, once or until stopped when `looping`, and hand `sink`
    /// interleaved stereo samples. `pace` gets the playing time of each packet's first sample,
    /// counted from the start of playback, before the packet is read; `false` stops. A packet that
    /// does not decode, or is larger than [`MAX_PACKET_BYTES`], is skipped as a damaged one; one that
    /// cannot be read ends playback.
    fn play(&mut self, looping: bool, pace: &mut dyn FnMut(Duration) -> bool, sink: &mut dyn FnMut(&[f32])) -> Result<(), String> {
        let sizes: Vec<u32> = self.ogg.streams.get(self.stream).map(|s| s.packets.iter().map(|p| p.size).collect()).unwrap_or_default();
        let rate = u64::from(self.sample_rate().max(1));
        let mut stereo = Vec::new();
        let mut played = 0u64;
        loop {
            self.decoder.reset();
            let mut this_pass = 0u64;
            for (i, &size) in sizes.iter().enumerate() {
                let at = Duration::from_secs(played / rate) + Duration::from_nanos(played % rate * 1_000_000_000 / rate);
                if !pace(at) {
                    return Ok(());
                }
                if size > MAX_PACKET_BYTES {
                    continue;
                }
                let packet = self.ogg.read_packet(&self.src, self.stream, i).map_err(|e| e.to_string())?;
                let Ok(pcm) = self.decoder.decode_packet(&packet) else { continue };
                let mut frames = pcm.first().map_or(0, Vec::len);
                // The last granule position ends the stream inside its last packet.
                if let Some(left) = self.length.map(|n| n.saturating_sub(this_pass)) {
                    frames = frames.min(usize::try_from(left).unwrap_or(usize::MAX));
                }
                if frames == 0 {
                    continue;
                }
                self.downmix.interleave(&pcm, frames, &mut stereo);
                sink(&stereo);
                let frames = u64::try_from(frames).unwrap_or(u64::MAX);
                this_pass = this_pass.saturating_add(frames);
                played = played.saturating_add(frames);
            }
            // A file with no samples to play is not looped, so the thread does not spin.
            if !looping || this_pass == 0 {
                return Ok(());
            }
        }
    }
}

/// Stereo from a Vorbis stream's channels, in the channel order of Vorbis I §4.3.9. The center
/// goes to both sides at -3 dB, side and rear channels to their own side at -3 dB, the rear
/// center to both sides at -6 dB, and the LFE channel is left out. The gains are then scaled down
/// so that the channels of one side, all at full scale, add up to full scale at most. Past eight
/// channels the order is the file's own, and the first two channels are the left and the right.
struct Downmix {
    /// The (left, right) gains of each channel.
    gains: Vec<[f32; 2]>,
}

impl Downmix {
    fn new(channels: usize) -> Self {
        const LEFT: [f32; 2] = [1.0, 0.0];
        const RIGHT: [f32; 2] = [0.0, 1.0];
        const CENTER: [f32; 2] = [std::f32::consts::FRAC_1_SQRT_2; 2];
        const LEFT_SIDE: [f32; 2] = [std::f32::consts::FRAC_1_SQRT_2, 0.0];
        const RIGHT_SIDE: [f32; 2] = [0.0, std::f32::consts::FRAC_1_SQRT_2];
        const REAR_CENTER: [f32; 2] = [0.5, 0.5];
        const NONE: [f32; 2] = [0.0, 0.0];
        let order: &[[f32; 2]] = match channels {
            1 => &[[1.0, 1.0]],
            2 => &[LEFT, RIGHT],
            3 => &[LEFT, CENTER, RIGHT],
            4 => &[LEFT, RIGHT, LEFT_SIDE, RIGHT_SIDE],
            5 => &[LEFT, CENTER, RIGHT, LEFT_SIDE, RIGHT_SIDE],
            6 => &[LEFT, CENTER, RIGHT, LEFT_SIDE, RIGHT_SIDE, NONE],
            7 => &[LEFT, CENTER, RIGHT, LEFT_SIDE, RIGHT_SIDE, REAR_CENTER, NONE],
            8 => &[LEFT, CENTER, RIGHT, LEFT_SIDE, RIGHT_SIDE, LEFT_SIDE, RIGHT_SIDE, NONE],
            _ => &[LEFT, RIGHT],
        };
        let mut gains = vec![NONE; channels.max(1)];
        for (g, o) in gains.iter_mut().zip(order) {
            *g = *o;
        }
        let loudest = gains.iter().fold([0.0f32; 2], |s, g| [s[0] + g[0], s[1] + g[1]]);
        let scale = loudest[0].max(loudest[1]).max(1.0);
        for g in &mut gains {
            *g = [g[0] / scale, g[1] / scale];
        }
        Downmix { gains }
    }

    /// The first `frames` samples of each channel of `pcm`, as interleaved stereo in `out`.
    fn interleave(&self, pcm: &[Vec<f32>], frames: usize, out: &mut Vec<f32>) {
        out.clear();
        for f in 0..frames {
            let (mut l, mut r) = (0.0, 0.0);
            for (channel, g) in pcm.iter().zip(&self.gains) {
                let s = channel.get(f).copied().unwrap_or(0.0);
                l += s * g[0];
                r += s * g[1];
            }
            out.extend_from_slice(&[l, r]);
        }
    }
}

/// The size and depth from a PAM header, or `None` at the end of the stream.
fn read_pam_header(r: &mut impl BufRead) -> Option<(u32, u32, u32)> {
    let (mut w, mut h, mut depth) = (0, 0, 0);
    let mut line = String::new();
    let mut lines = 0;
    loop {
        line.clear();
        if r.read_line(&mut line).ok()? == 0 || lines > 16 {
            return None;
        }
        lines += 1;
        let mut parts = line.split_whitespace();
        match (parts.next(), parts.next()) {
            (Some("ENDHDR"), _) => return (w > 0 && h > 0).then_some((w, h, depth)),
            (Some("WIDTH"), Some(v)) => w = v.parse().ok()?,
            (Some("HEIGHT"), Some(v)) => h = v.parse().ok()?,
            (Some("DEPTH"), Some(v)) => depth = v.parse().ok()?,
            _ => {}
        }
    }
}

fn read_pictures(out: impl Read, video: &VideoOut, clock: MasterClock) {
    let mut r = BufReader::with_capacity(1 << 20, out);
    while let Some((w, h, depth)) = read_pam_header(&mut r) {
        let mut frame = video.frame();
        if depth != 4 || !frame.reshape(w, h, PixelFormat::Rgba8) {
            return;
        }
        if r.read_exact(&mut frame.data).is_err() {
            return;
        }
        frame.pts = clock.now();
        video.publish(VideoContent::Frame(frame));
    }
}

fn read_samples(mut out: impl Read, audio: &Arc<AudioOut>, clock: MasterClock) {
    let mut feed = AudioFeed::new(SAMPLE_RATE, 2, Arc::clone(audio), clock);
    let mut bytes = vec![0u8; 8192];
    let mut samples = Vec::with_capacity(2048);
    let mut carry = 0;
    loop {
        let n = match out.read(&mut bytes[carry..]) {
            Ok(0) | Err(_) => return,
            Ok(n) => n + carry,
        };
        // Whole stereo frames only (two 4-byte samples); the rest waits for the next read.
        let whole = n / 8 * 8;
        samples.clear();
        samples.extend(bytes[..whole].as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)));
        feed.push(&samples);
        bytes.copy_within(whole..n, 0);
        carry = n - whole;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn pam_headers_are_parsed() {
        let data = b"P7\nWIDTH 3\nHEIGHT 2\nDEPTH 4\nMAXVAL 255\nTUPLTYPE RGB_ALPHA\nENDHDR\n";
        assert_eq!(read_pam_header(&mut &data[..]), Some((3, 2, 4)));
        assert_eq!(read_pam_header(&mut &b""[..]), None);
    }

    #[test]
    fn pictures_and_samples_are_published() {
        let mut pam = b"P7\nWIDTH 2\nHEIGHT 1\nDEPTH 4\nMAXVAL 255\nTUPLTYPE RGB_ALPHA\nENDHDR\n".to_vec();
        pam.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        let video = VideoOut::default();
        read_pictures(&pam[..], &video, MasterClock::new());
        let Some(VideoContent::Frame(f)) = video.take() else { panic!("no picture") };
        assert_eq!(f.data, [1, 2, 3, 4, 5, 6, 7, 8]);

        let audio = Arc::new(AudioOut::default());
        // Two blocks of stereo samples, plus the one frame the resampler holds back.
        let pcm: Vec<u8> = (0..4098).flat_map(|_| 0.5f32.to_le_bytes()).collect();
        read_samples(&pcm[..], &audio, MasterClock::new());
        assert_eq!(audio.queued(), 2);
        assert_eq!(audio.pop().unwrap().samples[0], 0.5);
    }

    /// Hands out at most 12 bytes per read: three samples, so a read ends inside a frame.
    struct ThreeSamplesAtATime<'a>(&'a [u8]);

    impl Read for ThreeSamplesAtATime<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = buf.len().min(12).min(self.0.len());
            buf[..n].copy_from_slice(&self.0[..n]);
            self.0 = &self.0[n..];
            Ok(n)
        }
    }

    #[test]
    fn a_read_that_ends_inside_a_frame_keeps_the_channels_in_order() {
        let pcm: Vec<u8> = (0..4098).flat_map(|i| if i % 2 == 0 { 0.25f32 } else { 0.75f32 }.to_le_bytes()).collect();
        let audio = Arc::new(AudioOut::default());
        read_samples(ThreeSamplesAtATime(&pcm), &audio, MasterClock::new());
        let block = audio.pop();
        assert!(block.is_some_and(|b| b.samples.as_chunks::<2>().0.iter().all(|f| *f == [0.25, 0.75])));
    }

    #[test]
    fn a_file_that_was_missing_is_tried_again() {
        let dir = std::env::temp_dir().join(format!("studiocraft-media-retry-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("later.mkv");
        let io = SourceIo::new(MasterClock::new());
        let settings = Settings::new().with("file", file.to_string_lossy().as_ref());
        // No program is started: the file check fails first, and then the program is missing.
        let mut i = Instance::new(Ok(dir.join("no-such-program")));
        let first = i.update(&settings, &io);
        assert!(first.as_ref().is_err_and(|e| e.to_string().contains("was not found")), "{first:?}");
        assert!(matches!(io.video.take(), Some(VideoContent::Empty)), "nothing from an earlier file is shown");
        std::fs::write(&file, b"").unwrap();
        // The same settings again, and the next produce, each start over now that the file exists.
        let again = i.update(&settings, &io);
        let produced = i.produce(&io);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(again.is_err_and(|e| e.to_string().contains("could not be started")));
        assert!(produced.is_err_and(|e| e.to_string().contains("could not be started")));
    }

    /// `false` stands in for an ffmpeg that cannot read the file: it ends with an error at once.
    #[cfg(unix)]
    #[test]
    fn an_ffmpeg_that_ends_with_an_error_is_reported_until_the_settings_change() {
        let Some(program) = ["/bin/false", "/usr/bin/false"].map(PathBuf::from).into_iter().find(|p| p.is_file()) else { return };
        let dir = std::env::temp_dir().join(format!("studiocraft-media-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("bad.mkv");
        std::fs::write(&file, b"not a video").unwrap();
        let io = SourceIo::new(MasterClock::new());
        let mut i = Instance::new(Ok(program));
        i.update(&Settings::new().with("file", file.to_string_lossy().as_ref()), &io).unwrap();
        // One process ending with an error is a file without that stream, and is not reported.
        // The other is held out of the check until then, since `false` ends at once.
        let other = i.children.pop();
        for c in &mut i.children {
            let _ = c.wait();
        }
        let one = i.produce(&io);
        i.children.extend(other);
        for c in &mut i.children {
            let _ = c.wait();
        }
        let first = i.produce(&io);
        let second = i.produce(&io);
        i.update(&Settings::new(), &io).unwrap();
        let cleared = i.produce(&io);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(one.is_ok(), "{one:?}");
        assert!(first.is_err_and(|e| e.to_string().contains("could not read")));
        assert!(second.is_err(), "the error stays");
        assert!(cleared.is_ok());
    }

    #[test]
    fn plays_a_generated_file_through_ffmpeg() {
        // Machines without ffmpeg pass without checking.
        let Ok(program) = ffmpeg::locate("") else { return };
        let dir = std::env::temp_dir().join(format!("studiocraft-media-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("clip.mkv");
        let made = ffmpeg::command(&program)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=64x48:rate=10:duration=1",
                "-f",
                "lavfi",
                "-i",
                "sine=duration=1",
            ])
            .args(["-c:v", "ffv1", "-c:a", "flac", "-shortest"])
            .arg(&file)
            .status()
            .unwrap();
        assert!(made.success());
        let io = SourceIo::new(MasterClock::new());
        let src = MediaSource::new(FfmpegSetting::default());
        let i = src.create(&Settings::new().with("file", file.to_string_lossy().as_ref()), &io).unwrap();
        let mut seen = (false, false);
        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(20));
            seen.0 |= io.video.size() == Some((64, 48));
            seen.1 |= io.audio.queued() > 0;
            if seen == (true, true) {
                break;
            }
        }
        drop(i);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(seen, (true, true));
    }

    /// `true` stands in for an ffmpeg that starts and writes no pictures, as for a file without
    /// video.
    #[cfg(unix)]
    #[test]
    fn a_file_without_video_clears_the_earlier_picture() {
        let Some(program) = ["/bin/true", "/usr/bin/true"].map(PathBuf::from).into_iter().find(|p| p.is_file()) else { return };
        let dir = std::env::temp_dir().join(format!("studiocraft-media-audio-only-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("tone.wav");
        std::fs::write(&file, b"not read").unwrap();
        let io = SourceIo::new(MasterClock::new());
        // A picture from the file played before.
        io.video.publish(VideoContent::Solid { rgba: [255; 4], width: 64, height: 48 });
        let mut i = Instance::new(Ok(program));
        let started = i.update(&Settings::new().with("file", file.to_string_lossy().as_ref()), &io);
        let shown = io.video.take();
        drop(i);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(started.is_ok(), "{started:?}");
        assert!(matches!(shown, Some(VideoContent::Empty)), "the earlier picture is not kept on screen");
        assert_eq!(io.video.size(), None);
    }

    #[test]
    fn stopping_waits_for_the_readers() {
        let io = SourceIo::new(MasterClock::new());
        let mut i = Instance::new(Ok(PathBuf::from("no-such-program")));
        let (go, wait) = std::sync::mpsc::channel::<()>();
        let video = Arc::clone(&io.video);
        // A reader with one last picture of the earlier file still to publish.
        i.readers.push(std::thread::spawn(move || {
            let _ = wait.recv();
            video.publish(VideoContent::Solid { rgba: [0; 4], width: 2, height: 2 });
        }));
        go.send(()).unwrap();
        i.stop();
        assert!(i.readers.is_empty());
        assert!(
            matches!(io.video.take(), Some(VideoContent::Solid { .. })),
            "the reader finished before stop returned, so the next file's content comes after it"
        );
    }

    /// An Ogg Vorbis file of `frames` frames at 48 kHz, each channel a sine at its own pitch.
    fn ogg_vorbis(channels: u16, frames: usize) -> Vec<u8> {
        let mut buf = craft_audio::AudioBuffer::new(48_000, channels, frames);
        for (c, ch) in buf.channels.iter_mut().enumerate() {
            for (i, s) in ch.iter_mut().enumerate() {
                *s = 0.5 * (i as f32 * 0.01 * (c as f32 + 1.0)).sin();
            }
        }
        craft_audio::encode_ogg_vorbis(&buf, &craft_audio::OggVorbisOptions::default()).unwrap()
    }

    /// The length of the first page of `ogg`.
    fn first_page(ogg: &[u8]) -> usize {
        let segments = usize::from(ogg[26]);
        27 + segments + ogg[27..27 + segments].iter().map(|&n| usize::from(n)).sum::<usize>()
    }

    /// `bytes` written to `name` in a new folder; remove the folder when done.
    fn temp_file(test: &str, name: &str, bytes: &[u8]) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("studiocraft-media-{test}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(name);
        std::fs::write(&file, bytes).unwrap();
        (dir, file)
    }

    #[test]
    fn ogg_files_whose_streams_are_all_vorbis_are_recognized() {
        let vorbis = ogg_vorbis(2, 4800);
        let first = first_page(&vorbis);
        assert!(only_vorbis_streams(&vorbis));
        // The same first page for a Theora video stream.
        let mut theora = vorbis[..first].to_vec();
        let body = 27 + usize::from(theora[26]);
        theora[body..body + 7].copy_from_slice(b"\x80theora");
        assert!(!only_vorbis_streams(&theora));
        // A stream of a codec this source does not know.
        let mut unknown = vorbis[..first].to_vec();
        unknown[body..body + 7].copy_from_slice(b"\x01vorbit");
        assert!(!only_vorbis_streams(&unknown));
        // A Vorbis stream and a Theora stream: the file has video.
        let mut both = vorbis[..first].to_vec();
        both.extend_from_slice(&theora);
        assert!(!only_vorbis_streams(&both));
        // Two Vorbis streams.
        let mut two = vorbis[..first].to_vec();
        two.extend_from_slice(&vorbis);
        assert!(only_vorbis_streams(&two));
        assert!(!only_vorbis_streams(b""));
        assert!(!only_vorbis_streams(b"not a video"));
        // A first page that ends before its packet does.
        assert!(!only_vorbis_streams(&vorbis[..28]));
    }

    #[test]
    fn an_ogg_vorbis_file_plays_every_sample_once_in_order() {
        let frames = 12_345;
        let bytes = ogg_vorbis(2, frames);
        let (dir, file) = temp_file("vorbis-once", "tone.ogg", &bytes);
        let mut f = VorbisFile::open(&file).unwrap().unwrap();
        let (mut times, mut out) = (Vec::new(), Vec::new());
        let played = f.play(
            false,
            &mut |at| {
                times.push(at);
                true
            },
            &mut |s| out.extend_from_slice(s),
        );
        let _ = std::fs::remove_dir_all(&dir);
        played.unwrap();
        let decoded = craft_audio::vorbis::decoder::decode_ogg(&bytes).unwrap();
        let expected: Vec<f32> = (0..frames).flat_map(|i| [decoded.channels[0][i], decoded.channels[1][i]]).collect();
        assert_eq!(out, expected, "the file's frames, trimmed to its last granule position");
        assert_eq!(times.first(), Some(&Duration::ZERO));
        assert!(times.windows(2).all(|w| w[0] <= w[1]));
        assert!(times.last().is_some_and(|t| t.as_secs_f64() < frames as f64 / 48_000.0));
    }

    #[test]
    fn a_looping_ogg_vorbis_file_starts_again_until_stopped() {
        let bytes = ogg_vorbis(1, 4800);
        let (dir, file) = temp_file("vorbis-loop", "tone.ogg", &bytes);
        let mut f = VorbisFile::open(&file).unwrap().unwrap();
        let mut out = Vec::new();
        let played = f.play(true, &mut |at| at < Duration::from_millis(250), &mut |s| out.extend_from_slice(s));
        let _ = std::fs::remove_dir_all(&dir);
        played.unwrap();
        // 0.1 s per pass, as stereo: past two passes when it stops at 0.25 s.
        let pass = 4800 * 2;
        assert!(out.len() > 2 * pass, "{}", out.len());
        assert_eq!(out[..pass], out[pass..2 * pass], "each pass plays the file from its start");
        assert!(out.as_chunks::<2>().0.iter().all(|[l, r]| l == r), "a mono file plays on both sides");
    }

    #[test]
    fn a_looping_ogg_vorbis_file_without_samples_ends() {
        let bytes = ogg_vorbis(2, 0);
        let (dir, file) = temp_file("vorbis-empty", "empty.ogg", &bytes);
        let opened = VorbisFile::open(&file);
        let _ = std::fs::remove_dir_all(&dir);
        let mut f = opened.unwrap().unwrap();
        let mut packets = 0;
        let played = f.play(
            true,
            &mut |_| {
                packets += 1;
                packets < 1000
            },
            &mut |_| {},
        );
        assert!(played.is_ok());
        assert!(packets < 10, "the player ends instead of starting the empty file again ({packets} packets)");
    }

    #[test]
    fn an_audio_packet_past_the_size_limit_is_skipped() {
        let frames = 9600;
        let base = ogg_vorbis(2, frames);
        let parsed = ogg::open(&base[..]).unwrap();
        let stream = &parsed.streams[0];
        // The same stream, with the fourth audio packet padded past the limit. Bits after a
        // packet's audio are not read, so the padded packet would still decode.
        let mut w = ogg::PageWriter::new(1);
        let mut bytes = Vec::new();
        for h in &stream.headers {
            bytes.extend(w.header(h));
        }
        for i in 0..stream.packets.len() {
            let mut p = parsed.read_packet(&base[..], 0, i).unwrap();
            if i == 3 {
                p.resize(MAX_PACKET_BYTES as usize + 1, 0);
            }
            bytes.extend(w.packet(&p, 0));
        }
        bytes.extend(w.finish(Some(frames as i64)));
        let (dir, file) = temp_file("vorbis-big-packet", "big.ogg", &bytes);
        let mut out = Vec::new();
        let played = VorbisFile::open(&file).map(|f| f.map(|mut f| f.play(false, &mut |_| true, &mut |s| out.extend_from_slice(s))));
        let _ = std::fs::remove_dir_all(&dir);
        played.unwrap().unwrap().unwrap();
        assert!(!out.is_empty() && out.len() < frames * 2, "the padded packet's samples are missing: {}", out.len());
    }

    #[test]
    fn surround_channels_are_mixed_to_stereo_without_clipping() {
        let mut out = Vec::new();
        // 5.1 in Vorbis order: left, center, right, left surround, right surround, LFE.
        let surround = Downmix::new(6);
        let only = |c: usize| (0..6).map(|i| vec![if i == c { 1.0 } else { 0.0 }]).collect::<Vec<_>>();
        surround.interleave(&only(1), 1, &mut out);
        assert!(out[0] > 0.0 && out[0] == out[1], "the center is on both sides: {out:?}");
        surround.interleave(&only(3), 1, &mut out);
        assert!(out[0] > 0.0 && out[1] == 0.0, "the left surround is on the left: {out:?}");
        surround.interleave(&only(5), 1, &mut out);
        assert_eq!(out, [0.0, 0.0], "the LFE channel is left out");
        surround.interleave(&vec![vec![1.0]; 6], 1, &mut out);
        assert!(out.iter().all(|s| (s - 1.0).abs() < 1e-6), "every channel at full scale stays at full scale: {out:?}");

        Downmix::new(1).interleave(&[vec![0.5, -0.25]], 2, &mut out);
        assert_eq!(out, [0.5, 0.5, -0.25, -0.25]);
        Downmix::new(2).interleave(&[vec![0.5], vec![-0.25]], 1, &mut out);
        assert_eq!(out, [0.5, -0.25]);
        let nine: Vec<Vec<f32>> = (0..9).map(|i| vec![i as f32 / 10.0]).collect();
        Downmix::new(9).interleave(&nine, 1, &mut out);
        assert_eq!(out, [0.0, 0.1], "past eight channels the first two are the left and the right");
    }

    #[test]
    fn an_ogg_vorbis_file_plays_without_ffmpeg() {
        let (dir, file) = temp_file("vorbis-no-ffmpeg", "tone.ogg", &ogg_vorbis(2, 48_000 * 3));
        let other = dir.join("clip.mkv");
        std::fs::write(&other, b"not a video").unwrap();
        let io = SourceIo::new(MasterClock::new());
        let mut i = Instance::new(Err("ffmpeg (set its location in Settings › Output) was not found".into()));
        let started = i.update(&Settings::new().with("file", file.to_string_lossy().as_ref()), &io);
        let player = (i.player_thread.is_some(), i.children.len());
        // The player sees the stop before the file ends.
        i.stop();
        let produced = i.produce(&io);
        let needs_ffmpeg = i.update(&Settings::new().with("file", other.to_string_lossy().as_ref()), &io);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(started.is_ok(), "{started:?}");
        assert_eq!(player, (true, 0), "a player thread and no ffmpeg");
        assert!(produced.is_ok(), "{produced:?}");
        assert!(matches!(&needs_ffmpeg, Err(PluginError::Unavailable(e)) if e.contains("was not found")), "{needs_ffmpeg:?}");
    }

    #[test]
    fn an_ogg_vorbis_file_that_cannot_be_decoded_is_reported() {
        // The identification header's page alone: the comment and setup headers are missing.
        let bytes = ogg_vorbis(2, 4800);
        let (dir, file) = temp_file("vorbis-cut", "cut.ogg", &bytes[..first_page(&bytes)]);
        let io = SourceIo::new(MasterClock::new());
        let mut i = Instance::new(Err("ffmpeg (set its location in Settings › Output) was not found".into()));
        let started = i.update(&Settings::new().with("file", file.to_string_lossy().as_ref()), &io);
        // Once the player has ended, its error is in place.
        let _ = i.player_thread.take().map(JoinHandle::join);
        let first = i.produce(&io);
        let second = i.produce(&io);
        i.update(&Settings::new(), &io).unwrap();
        let cleared = i.produce(&io);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(started.is_ok(), "{started:?}");
        assert!(first.is_err_and(|e| e.to_string().contains("could not be played")));
        assert!(second.is_err(), "the error stays");
        assert!(cleared.is_ok(), "{cleared:?}");
    }

    #[test]
    fn a_chained_ogg_vorbis_file_plays_through_ffmpeg() {
        // Two Vorbis streams one after the other, each with its own serial number.
        let first = ogg_vorbis(2, 4800);
        let parsed = ogg::open(&first[..]).unwrap();
        let mut w = ogg::PageWriter::new(2);
        let mut bytes = first.clone();
        for h in &parsed.streams[0].headers {
            bytes.extend(w.header(h));
        }
        for i in 0..parsed.streams[0].packets.len() {
            bytes.extend(w.packet(&parsed.read_packet(&first[..], 0, i).unwrap(), 0));
        }
        bytes.extend(w.finish(Some(4800)));
        let (dir, file) = temp_file("vorbis-chained", "chained.ogg", &bytes);
        let opened = VorbisFile::open(&file).map(|f| f.is_none());
        let io = SourceIo::new(MasterClock::new());
        let mut i = Instance::new(Err("ffmpeg (set its location in Settings › Output) was not found".into()));
        let started = i.update(&Settings::new().with("file", file.to_string_lossy().as_ref()), &io);
        let _ = i.player_thread.take().map(JoinHandle::join);
        let produced = i.produce(&io);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(opened, Ok(true), "the player leaves the file to ffmpeg");
        assert!(started.is_ok(), "the first stream at the start of the file is Vorbis: {started:?}");
        assert!(matches!(&produced, Err(PluginError::Unavailable(e)) if e.contains("was not found")), "{produced:?}");
    }

    #[test]
    fn stopping_does_not_wait_for_a_player_reading_its_index() {
        let mut i = Instance::new(Ok(PathBuf::from("no-such-program")));
        let (go, wait) = std::sync::mpsc::channel::<()>();
        let state = Arc::clone(&i.player);
        state.indexing.store(true, Ordering::SeqCst);
        // A player still reading its index until the test lets it go.
        i.player_thread = Some(std::thread::spawn(move || {
            let _ = wait.recv();
            state.indexing.store(false, Ordering::SeqCst);
        }));
        i.stop();
        let stopped = i.player.stopping.load(Ordering::SeqCst);
        go.send(()).unwrap();
        assert!(stopped, "the player ends without publishing once its index is read");
        assert!(i.player_thread.is_none());
    }
}
