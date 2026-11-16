//! A video or audio file, played in real time by ffmpeg.
//!
//! Two ffmpeg processes read the file at its own speed (`-re`): one writes RGBA pictures (PAM
//! images, which carry their size) and one writes 48 kHz stereo samples. A reader thread for
//! each publishes into the source's outputs.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

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
        let program = ffmpeg::locate(&configured).map_err(|e| PluginError::Unavailable(e.to_string()))?;
        let mut i = Instance::new(program);
        i.restart(settings, io)?;
        Ok(Box::new(i))
    }
}

struct Instance {
    program: PathBuf,
    file: String,
    looping: bool,
    children: Vec<Child>,
    /// The threads reading the children's output. They are joined when the children stop, so
    /// nothing read from an earlier file is published after the next file starts.
    readers: Vec<JoinHandle<()>>,
    /// The settings of a restart that failed, tried again on the next produce.
    retry: Option<Settings>,
    /// Why ffmpeg ended before the file did.
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
    fn new(program: PathBuf) -> Self {
        Instance {
            program,
            file: String::new(),
            looping: false,
            children: Vec::new(),
            readers: Vec::new(),
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
        let video = self.spawn(path, &["-map", "0:v:0?", "-an", "-f", "image2pipe", "-c:v", "pam", "-pix_fmt", "rgba", "pipe:1"])?;
        self.children.push(video);
        let rate = SAMPLE_RATE.to_string();
        match self.spawn(path, &["-map", "0:a:0?", "-vn", "-f", "f32le", "-ac", "2", "-ar", &rate, "pipe:1"]) {
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

    fn spawn(&self, input: &Path, output: &[&str]) -> Result<Child, PluginError> {
        let mut cmd = ffmpeg::command(&self.program);
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
        let mut i = Instance::new(dir.join("no-such-program"));
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
        let mut i = Instance::new(program);
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
        let mut i = Instance::new(program);
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
        let mut i = Instance::new(PathBuf::from("no-such-program"));
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
}
