//! A profile: the video, audio, recording and streaming settings, kept apart from the scenes so
//! one scene collection can be recorded or streamed with different setups.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::Settings;

/// Canvas and output frame size and rate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VideoSettings {
    /// The canvas scenes are laid out on.
    pub base_width: u32,
    pub base_height: u32,
    /// The size recordings and streams are scaled to.
    pub output_width: u32,
    pub output_height: u32,
    /// Frames per second as a fraction: 30/1, 60/1, 30000/1001.
    pub fps_num: u32,
    pub fps_den: u32,
}

impl Default for VideoSettings {
    fn default() -> Self {
        Self { base_width: 1920, base_height: 1080, output_width: 1920, output_height: 1080, fps_num: 30, fps_den: 1 }
    }
}

/// Smallest and largest canvas side accepted.
pub const MIN_SIDE: u32 = 16;
pub const MAX_SIDE: u32 = 8192;

impl VideoSettings {
    /// Values in range: sides between [`MIN_SIDE`] and [`MAX_SIDE`] and even (encoders need even
    /// sides for 4:2:0), 1 to 240 frames per second.
    pub fn sanitized(self) -> VideoSettings {
        let side = |v: u32| (v.clamp(MIN_SIDE, MAX_SIDE) / 2) * 2;
        let den = self.fps_den.clamp(1, 1001);
        let num = self.fps_num.clamp(den, den.saturating_mul(240));
        VideoSettings {
            base_width: side(self.base_width),
            base_height: side(self.base_height),
            output_width: side(self.output_width),
            output_height: side(self.output_height),
            fps_num: num,
            fps_den: den,
        }
    }

    pub fn fps(&self) -> f64 {
        f64::from(self.fps_num) / f64::from(self.fps_den.max(1))
    }

    /// The output size that is `scale` (a numerator and a denominator) of the canvas: each side
    /// rounded down to an even number, between [`MIN_SIDE`] and [`MAX_SIDE`].
    pub fn scaled_output(&self, scale: (u32, u32)) -> (u32, u32) {
        let v = self.sanitized();
        let side = |base: u32| {
            let s = u64::from(base) * u64::from(scale.0) / u64::from(scale.1.max(1));
            let s = u32::try_from(s).unwrap_or(MAX_SIDE).clamp(MIN_SIDE, MAX_SIDE);
            (s / 2) * 2
        };
        (side(v.base_width), side(v.base_height))
    }

    /// The entry of [`OUTPUT_SCALES`] that gives the current output size, if exactly one does.
    /// On a canvas so small that several entries give the same size, the output size counts as
    /// typed: no entry is shown as chosen, and a new canvas size leaves the output size as it is.
    pub fn output_scale(&self) -> Option<(u32, u32)> {
        let v = self.sanitized();
        let mut matching = OUTPUT_SCALES.into_iter().filter(|s| v.scaled_output(*s) == (v.output_width, v.output_height));
        match (matching.next(), matching.next()) {
            (Some(s), None) => Some(s),
            _ => None,
        }
    }

    /// These settings with a canvas of `width` x `height`. An output size that was one of
    /// [`OUTPUT_SCALES`] of the old canvas becomes the same share of the new one; any other output
    /// size stays.
    pub fn with_canvas(self, width: u32, height: u32) -> VideoSettings {
        let scale = self.output_scale();
        let mut v = VideoSettings { base_width: width, base_height: height, ..self.sanitized() }.sanitized();
        if let Some(s) = scale {
            (v.output_width, v.output_height) = v.scaled_output(s);
        }
        v
    }
}

/// The output sizes offered as a share of the canvas, as numerators and denominators: 100%, 75%,
/// two thirds and 50%.
pub const OUTPUT_SCALES: [(u32, u32); 4] = [(1, 1), (3, 4), (2, 3), (1, 2)];

/// Audio format of the mix, the device monitored sources play on, and the program loudness the
/// profile aims for.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioSettings {
    pub sample_rate: u32,
    /// The output device monitored sources play on, by its name; [`DEFAULT_AUDIO_DEVICE`] (or
    /// empty) for the system's default output. A name the system does not list is reported
    /// when monitoring starts.
    pub monitoring_device: String,
    /// The band the program's loudness should stay in.
    pub loudness_target: LoudnessTarget,
}

/// The device name that stands for the system's default device.
pub const DEFAULT_AUDIO_DEVICE: &str = "default";

impl Default for AudioSettings {
    fn default() -> Self {
        Self { sample_rate: 48_000, monitoring_device: DEFAULT_AUDIO_DEVICE.into(), loudness_target: LoudnessTarget::default() }
    }
}

/// The band of program loudness a profile aims for, from `low_lufs` to `high_lufs` in LUFS.
/// The default, −18 to −14 LUFS, and the allowed range, −60 to 0 LUFS, are StudioCraft's own
/// values.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LoudnessTarget {
    pub low_lufs: f64,
    pub high_lufs: f64,
}

impl LoudnessTarget {
    /// The lowest bound a band may have, in LUFS.
    pub const MIN_LUFS: f64 = -60.0;
    /// The highest bound a band may have, in LUFS.
    pub const MAX_LUFS: f64 = 0.0;

    /// An error when a bound is not a number in [`Self::MIN_LUFS`] to [`Self::MAX_LUFS`], or
    /// when `low_lufs` is not below `high_lufs`.
    pub fn check(&self) -> Result<(), String> {
        let in_range = |v: f64| v.is_finite() && (Self::MIN_LUFS..=Self::MAX_LUFS).contains(&v);
        if !in_range(self.low_lufs) || !in_range(self.high_lufs) {
            return Err(format!(
                "the loudness target must be between {} and {} LUFS (it is {} to {})",
                Self::MIN_LUFS,
                Self::MAX_LUFS,
                self.low_lufs,
                self.high_lufs
            ));
        }
        if self.low_lufs >= self.high_lufs {
            return Err(format!("the loudness target's low bound ({} LUFS) must be below its high bound ({} LUFS)", self.low_lufs, self.high_lufs));
        }
        Ok(())
    }
}

impl Default for LoudnessTarget {
    fn default() -> Self {
        Self { low_lufs: -18.0, high_lufs: -14.0 }
    }
}

/// Container of recordings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingFormat {
    /// Matroska: a recording cut short by a crash stays playable.
    #[default]
    Mkv,
    Mp4,
    Mov,
}

impl RecordingFormat {
    pub const ALL: [RecordingFormat; 3] = [RecordingFormat::Mkv, RecordingFormat::Mp4, RecordingFormat::Mov];

    pub fn extension(self) -> &'static str {
        match self {
            RecordingFormat::Mkv => "mkv",
            RecordingFormat::Mp4 => "mp4",
            RecordingFormat::Mov => "mov",
        }
    }
}

/// Quality of recordings, from smallest files to largest.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingQuality {
    /// About the size of a stream.
    Compact,
    #[default]
    High,
    /// Hard to tell from the source.
    Pristine,
    Lossless,
}

impl RecordingQuality {
    pub const ALL: [RecordingQuality; 4] =
        [RecordingQuality::Compact, RecordingQuality::High, RecordingQuality::Pristine, RecordingQuality::Lossless];

    /// The constant-quality value encoders aim for (lower is better); `None` for lossless.
    pub fn quality_level(self) -> Option<u32> {
        match self {
            RecordingQuality::Compact => Some(28),
            RecordingQuality::High => Some(23),
            RecordingQuality::Pristine => Some(16),
            RecordingQuality::Lossless => None,
        }
    }
}

/// Where and how recordings are written.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OutputSettings {
    /// Folder recordings are written to; empty means the user's Videos folder.
    pub folder: String,
    pub format: RecordingFormat,
    pub quality: RecordingQuality,
    /// Video encoder plug-in id.
    pub video_encoder: String,
    /// The settings of each video encoder used for recordings, by encoder plug-in id.
    pub encoder_settings: BTreeMap<String, Settings>,
    /// The ffmpeg program; empty means `ffmpeg` on the `PATH`.
    pub ffmpeg_path: String,
    /// What the `hardware` video encoder of recordings and streams runs: `auto` (the first
    /// hardware encoder that passes a one-frame test, else x264), `software`, or a hardware
    /// encoder's plug-in id.
    pub hardware_encoder: String,
}

impl Default for OutputSettings {
    fn default() -> Self {
        Self {
            folder: String::new(),
            format: RecordingFormat::default(),
            quality: RecordingQuality::default(),
            video_encoder: "x264".into(),
            encoder_settings: BTreeMap::new(),
            ffmpeg_path: String::new(),
            hardware_encoder: "auto".into(),
        }
    }
}

impl OutputSettings {
    /// The settings of the chosen video encoder; empty when none are saved, so the encoder's
    /// defaults apply.
    pub fn video_encoder_settings(&self) -> Settings {
        self.encoder_settings.get(&self.video_encoder).cloned().unwrap_or_default()
    }
}

/// Where a destination's stream key is kept.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyRef {
    /// No key: the server's address is all the service needs.
    #[default]
    None,
    /// The profile's own key, [`StreamSettings::key`], which is saved in the profile file as
    /// plain text, as profiles of version 6 and earlier saved it.
    Profile,
    /// A key the studio holds in memory under this name for the session. It is never written
    /// to the profile file.
    Held(String),
}

/// One place a stream is sent to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Destination {
    /// Names the destination in commands and status; unique within the profile.
    pub id: String,
    /// The name shown for it.
    pub name: String,
    /// Whether starting the stream sends to it.
    pub enabled: bool,
    /// The RTMP server, `rtmp://host/app` or `rtmps://host/app`.
    pub server: String,
    pub key: KeyRef,
    /// The id of the streaming service it was set up for ([`crate::services::SERVICES`]); empty
    /// for a server of the user's own choosing. A service id this version does not know is kept
    /// and shown as a custom server.
    pub service: String,
    /// The video encoder plug-in id it sends with; empty for the stream's encoder.
    pub video_encoder: String,
    /// The video bitrate it sends with, in kbps; 0 for the stream's bitrate.
    pub video_bitrate_kbps: u32,
}

impl Default for Destination {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            enabled: true,
            server: String::new(),
            key: KeyRef::None,
            service: String::new(),
            video_encoder: String::new(),
            video_bitrate_kbps: 0,
        }
    }
}

/// The video bitrates a stream and a destination may set, in kbps.
pub const VIDEO_BITRATE_KBPS: std::ops::RangeInclusive<u32> = 100..=200_000;
/// Longest video encoder id a destination may name, in bytes.
pub const MAX_ENCODER_ID: usize = 64;

/// One video encoder of a stream and the destinations that send what it makes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncoderGroup {
    /// The encoder plug-in id.
    pub video_encoder: String,
    pub video_bitrate_kbps: u32,
    /// Whether this is the stream's encoder, which runs for as long as the stream does.
    pub shared: bool,
    /// The ids of its destinations, in the order they are listed.
    pub destinations: Vec<String>,
}

/// Most destinations a profile holds.
pub const MAX_DESTINATIONS: usize = 16;
/// Longest destination id, in bytes.
pub const MAX_DESTINATION_ID: usize = 64;
/// The id of the destination a profile of version 6 or earlier streamed to.
pub const PRIMARY_DESTINATION: &str = "main";

/// Where and how to stream.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StreamSettings {
    /// The places the stream is sent to, in the order they are listed.
    pub destinations: Vec<Destination>,
    /// The stream key of each destination whose key is [`KeyRef::Profile`]. It is saved in the
    /// profile file as plain text, as earlier versions saved it.
    pub key: String,
    pub video_bitrate_kbps: u32,
    pub audio_bitrate_kbps: u32,
    pub video_encoder: String,
    /// The settings of each video encoder used for streams, by encoder plug-in id.
    pub encoder_settings: BTreeMap<String, Settings>,
    /// Seconds between keyframes.
    pub keyframe_interval_s: u32,
    /// How a destination tries again after its connection fails or is lost.
    pub reconnect: ReconnectSettings,
    /// Whether the connection check that runs by itself after a destination fails also contacts
    /// the service's other hosts: its list of ingest servers, two other ingest servers and its
    /// status summary. Off by default; Check Connection always contacts them.
    pub check_service: bool,
    /// Whether the stream also writes what it sends to an MKV file in the recording folder,
    /// `Stream <date and time>.mkv`. Off by default.
    pub save_copy: bool,
}

impl Default for StreamSettings {
    fn default() -> Self {
        Self {
            destinations: Vec::new(),
            key: String::new(),
            video_bitrate_kbps: 6000,
            audio_bitrate_kbps: 160,
            video_encoder: "h264".into(),
            encoder_settings: BTreeMap::new(),
            keyframe_interval_s: 2,
            reconnect: ReconnectSettings::default(),
            check_service: false,
            save_copy: false,
        }
    }
}

/// How a stream destination tries again after its connection fails or is lost. The first wait is
/// `first_delay_s`, each later wait doubles up to `max_delay_s`, and the destination gives up after
/// `max_tries` failed tries in a row. With `enabled` off it gives up at the first failure. The
/// defaults (on, 2 s, 30 s, 10 tries) and the ranges are StudioCraft's own values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReconnectSettings {
    pub enabled: bool,
    pub first_delay_s: u32,
    pub max_delay_s: u32,
    pub max_tries: u32,
}

impl ReconnectSettings {
    /// The allowed range of the first wait, in seconds.
    pub const FIRST_DELAY_S: std::ops::RangeInclusive<u32> = 1..=60;
    /// The longest wait allowed, in seconds; the longest wait is at least the first wait.
    pub const MAX_DELAY_S: u32 = 600;
    /// The allowed range of the number of tries.
    pub const MAX_TRIES: std::ops::RangeInclusive<u32> = 1..=100;

    /// An error naming the first value outside its range.
    pub fn check(&self) -> Result<(), String> {
        if !Self::FIRST_DELAY_S.contains(&self.first_delay_s) {
            return Err(format!(
                "the first reconnect wait must be {} to {} s (it is {} s)",
                Self::FIRST_DELAY_S.start(),
                Self::FIRST_DELAY_S.end(),
                self.first_delay_s
            ));
        }
        if self.max_delay_s < self.first_delay_s || self.max_delay_s > Self::MAX_DELAY_S {
            return Err(format!(
                "the longest reconnect wait must be {} to {} s (it is {} s)",
                self.first_delay_s,
                Self::MAX_DELAY_S,
                self.max_delay_s
            ));
        }
        if !Self::MAX_TRIES.contains(&self.max_tries) {
            return Err(format!(
                "the number of reconnect tries must be {} to {} (it is {})",
                Self::MAX_TRIES.start(),
                Self::MAX_TRIES.end(),
                self.max_tries
            ));
        }
        Ok(())
    }
}

impl Default for ReconnectSettings {
    fn default() -> Self {
        Self { enabled: true, first_delay_s: 2, max_delay_s: 30, max_tries: 10 }
    }
}

impl StreamSettings {
    /// The settings of the chosen video encoder; empty when none are saved, so the encoder's
    /// defaults apply.
    pub fn video_encoder_settings(&self) -> Settings {
        self.encoder_settings.get(&self.video_encoder).cloned().unwrap_or_default()
    }

    /// The destination with id `id`.
    pub fn destination(&self, id: &str) -> Option<&Destination> {
        self.destinations.iter().find(|d| d.id == id)
    }

    /// The first destination, added with the profile's key when the list is empty. Settings ›
    /// Stream edits this one.
    pub fn primary_mut(&mut self) -> &mut Destination {
        if self.destinations.is_empty() {
            self.destinations.push(Destination { id: PRIMARY_DESTINATION.into(), key: KeyRef::Profile, ..Destination::default() });
        }
        // Not empty: a destination was added above.
        &mut self.destinations[0]
    }

    /// The server of the first destination; empty when there is none.
    pub fn primary_server(&self) -> &str {
        self.destinations.first().map_or("", |d| d.server.as_str())
    }

    /// The stream's video bitrate as a stream starts with it, in [`VIDEO_BITRATE_KBPS`].
    pub fn video_bitrate(&self) -> u32 {
        self.video_bitrate_kbps.clamp(*VIDEO_BITRATE_KBPS.start(), *VIDEO_BITRATE_KBPS.end())
    }

    /// The video encoder plug-in id and bitrate `d` sends with: each its own when it sets one,
    /// else the stream's.
    pub fn encoder_of<'a>(&'a self, d: &'a Destination) -> (&'a str, u32) {
        let encoder = if d.video_encoder.is_empty() { self.video_encoder.as_str() } else { d.video_encoder.as_str() };
        let kbps = if d.video_bitrate_kbps == 0 { self.video_bitrate() } else { d.video_bitrate_kbps };
        (encoder, kbps)
    }

    /// Whether `d` sends what the stream's encoder makes: its encoder and bitrate are the stream's,
    /// whether it names them or not.
    pub fn shares_encoder(&self, d: &Destination) -> bool {
        self.encoder_of(d) == (self.video_encoder.as_str(), self.video_bitrate())
    }

    /// The video encoders the destinations send with: the stream's first, then one for each other
    /// encoder and bitrate in the order a destination first names it, each with its destinations.
    /// Destinations that name the same encoder and bitrate share one encoder.
    pub fn encoder_groups(&self) -> Vec<EncoderGroup> {
        let mut groups = vec![EncoderGroup {
            video_encoder: self.video_encoder.clone(),
            video_bitrate_kbps: self.video_bitrate(),
            shared: true,
            destinations: Vec::new(),
        }];
        for d in &self.destinations {
            let (encoder, kbps) = self.encoder_of(d);
            match groups.iter_mut().find(|g| g.video_encoder == encoder && g.video_bitrate_kbps == kbps) {
                Some(g) => g.destinations.push(d.id.clone()),
                None => groups.push(EncoderGroup {
                    video_encoder: encoder.to_string(),
                    video_bitrate_kbps: kbps,
                    shared: false,
                    destinations: vec![d.id.clone()],
                }),
            }
        }
        groups
    }

    /// The first problem with the destinations, as an error message: too many, an empty or long
    /// id, an id used twice, or an encoder id or bitrate of one that cannot be.
    pub fn check(&self) -> Result<(), String> {
        if self.destinations.len() > MAX_DESTINATIONS {
            return Err(format!("the stream has {} destinations; at most {MAX_DESTINATIONS} are allowed", self.destinations.len()));
        }
        for (i, d) in self.destinations.iter().enumerate() {
            if d.id.trim().is_empty() {
                return Err(format!("destination {} has no id", i + 1));
            }
            if d.id.len() > MAX_DESTINATION_ID {
                return Err(format!("destination {} has an id longer than {MAX_DESTINATION_ID} bytes", i + 1));
            }
            if self.destinations[..i].iter().any(|o| o.id == d.id) {
                return Err(format!("the destination id `{}` is used twice", d.id));
            }
            if d.video_encoder.len() > MAX_ENCODER_ID || d.video_encoder.chars().any(char::is_control) {
                return Err(format!("destination `{}` names a video encoder id that is not valid", d.id));
            }
            if d.video_bitrate_kbps != 0 && !VIDEO_BITRATE_KBPS.contains(&d.video_bitrate_kbps) {
                return Err(format!(
                    "destination `{}` has a video bitrate of {} kbps; it must be {} to {} kbps, or 0 for the stream's",
                    d.id,
                    d.video_bitrate_kbps,
                    VIDEO_BITRATE_KBPS.start(),
                    VIDEO_BITRATE_KBPS.end()
                ));
            }
        }
        Ok(())
    }
}

/// How much the replay buffer keeps in memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReplaySettings {
    /// Seconds of the newest video and sound a saved replay holds.
    pub seconds: u32,
    /// Most megabytes of encoded video and sound kept; past it the oldest go first.
    pub memory_mb: u32,
}

impl ReplaySettings {
    /// The lengths the replay buffer takes, in seconds.
    pub const SECONDS: std::ops::RangeInclusive<u32> = 5..=3600;
    /// The memory limits the replay buffer takes, in megabytes.
    pub const MEMORY_MB: std::ops::RangeInclusive<u32> = 32..=16_384;

    /// These settings with each value brought into its range.
    pub fn sanitized(self) -> Self {
        let clamp = |v: u32, r: std::ops::RangeInclusive<u32>| v.clamp(*r.start(), *r.end());
        Self { seconds: clamp(self.seconds, Self::SECONDS), memory_mb: clamp(self.memory_mb, Self::MEMORY_MB) }
    }
}

impl Default for ReplaySettings {
    fn default() -> Self {
        Self { seconds: 20, memory_mb: 512 }
    }
}

/// When audio tracks are recorded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioTracksWhen {
    /// While a recording runs.
    #[default]
    Recording,
    /// While the stream runs.
    Streaming,
    /// While a recording or the stream runs.
    Either,
}

impl AudioTracksWhen {
    pub const ALL: [AudioTracksWhen; 3] = [AudioTracksWhen::Recording, AudioTracksWhen::Streaming, AudioTracksWhen::Either];

    /// The name commands and files use.
    pub fn id(self) -> &'static str {
        match self {
            AudioTracksWhen::Recording => "recording",
            AudioTracksWhen::Streaming => "streaming",
            AudioTracksWhen::Either => "either",
        }
    }
}

/// What happens when a take of audio tracks ends and its session file is written.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AfterStop {
    /// Nothing more.
    #[default]
    Nothing,
    /// The take's folder opens in the file manager.
    ShowFolder,
    /// The take's session opens in the audio editor, or its folder in the file manager when the
    /// editor cannot be started.
    OpenEditor,
}

impl AfterStop {
    pub const ALL: [AfterStop; 3] = [AfterStop::Nothing, AfterStop::ShowFolder, AfterStop::OpenEditor];

    /// The name commands and files use.
    pub fn id(self) -> &'static str {
        match self {
            AfterStop::Nothing => "nothing",
            AfterStop::ShowFolder => "show_folder",
            AfterStop::OpenEditor => "open_editor",
        }
    }
}

/// Audio tracks: each source set to record its own track, and the program mix, written to WAV
/// files beside the recording.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioTracksSettings {
    pub enabled: bool,
    pub when: AudioTracksWhen,
    /// Whether the program mix gets a file too.
    pub include_program: bool,
    /// Memory that holds audio waiting for the disk, in MiB.
    pub buffer_mib: u32,
    /// Size at which a track's file is closed and the next part begins, in GiB.
    pub segment_gib: u32,
    /// What happens when a take ends.
    pub after_stop: AfterStop,
    /// The audio editor's program; empty to look it up on the `PATH`.
    pub editor_path: String,
}

impl AudioTracksSettings {
    /// The write buffers the audio tracks take, in MiB.
    pub const BUFFER_MIB: std::ops::RangeInclusive<u32> = 16..=1024;
    /// The file sizes the audio tracks take, in GiB.
    pub const SEGMENT_GIB: std::ops::RangeInclusive<u32> = 1..=3;
    /// Bytes an editor path holds at most.
    pub const MAX_EDITOR_PATH: usize = 4096;

    /// The first value out of its range, as an error message.
    pub fn check(&self) -> Result<(), String> {
        let range = |name: &str, v: u32, r: std::ops::RangeInclusive<u32>| {
            if r.contains(&v) { Ok(()) } else { Err(format!("audio_tracks.{name} is {v}; it must be from {} to {}", r.start(), r.end())) }
        };
        range("buffer_mib", self.buffer_mib, Self::BUFFER_MIB)?;
        range("segment_gib", self.segment_gib, Self::SEGMENT_GIB)?;
        if !Self::editor_path_ok(&self.editor_path) {
            return Err(format!("audio_tracks.editor_path must be at most {} bytes and hold no NUL character", Self::MAX_EDITOR_PATH));
        }
        Ok(())
    }

    fn editor_path_ok(path: &str) -> bool {
        path.len() <= Self::MAX_EDITOR_PATH && !path.contains('\0')
    }

    /// These settings with each size brought into its range, and an editor path that cannot be
    /// one cleared.
    pub fn sanitized(self) -> Self {
        let clamp = |v: u32, r: std::ops::RangeInclusive<u32>| v.clamp(*r.start(), *r.end());
        let editor_path = if Self::editor_path_ok(&self.editor_path) { self.editor_path } else { String::new() };
        Self { buffer_mib: clamp(self.buffer_mib, Self::BUFFER_MIB), segment_gib: clamp(self.segment_gib, Self::SEGMENT_GIB), editor_path, ..self }
    }
}

impl Default for AudioTracksSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            when: AudioTracksWhen::Recording,
            include_program: true,
            buffer_mib: 96,
            segment_gib: 1,
            after_stop: AfterStop::Nothing,
            editor_path: String::new(),
        }
    }
}

/// What the pause screen does when its countdown reaches zero.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PauseAtZero {
    /// The pause screen stays, and the text at zero replaces the countdown.
    #[default]
    Keep,
    /// The program resumes, as Resume does.
    Resume,
}

impl PauseAtZero {
    pub const ALL: [PauseAtZero; 2] = [PauseAtZero::Keep, PauseAtZero::Resume];

    /// The name commands and files use.
    pub fn id(self) -> &'static str {
        match self {
            PauseAtZero::Keep => "keep",
            PauseAtZero::Resume => "resume",
        }
    }

    /// The value named `id`.
    pub fn from_id(id: &str) -> Option<PauseAtZero> {
        Self::ALL.into_iter().find(|v| v.id() == id)
    }
}

/// Where the countdown text sits on the canvas.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PauseAnchor {
    TopLeft,
    TopCenter,
    TopRight,
    MiddleLeft,
    Center,
    MiddleRight,
    BottomLeft,
    #[default]
    BottomCenter,
    BottomRight,
}

impl PauseAnchor {
    pub const ALL: [PauseAnchor; 9] = [
        PauseAnchor::TopLeft,
        PauseAnchor::TopCenter,
        PauseAnchor::TopRight,
        PauseAnchor::MiddleLeft,
        PauseAnchor::Center,
        PauseAnchor::MiddleRight,
        PauseAnchor::BottomLeft,
        PauseAnchor::BottomCenter,
        PauseAnchor::BottomRight,
    ];

    /// Where the anchor is across and down the area inside the margins: 0 is the left or top edge,
    /// 1 the right or bottom edge.
    pub fn fractions(self) -> (f32, f32) {
        let i = Self::ALL.iter().position(|a| *a == self).unwrap_or(7);
        ((i % 3) as f32 * 0.5, (i / 3) as f32 * 0.5)
    }
}

/// The pause screen: what the program shows while it is paused, and its countdown.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PauseSettings {
    /// A picture or video file shown over the background; empty for none.
    pub media: String,
    /// How the picture or video is sized to the canvas.
    pub fit: crate::FitMode,
    /// The color under the picture or video, `#rrggbbaa`.
    pub background: String,
    /// The countdown's length in seconds; 0 for no countdown.
    pub countdown_s: u32,
    /// The countdown text, with one `{time}` where the remaining time goes.
    pub text: String,
    pub at_zero: PauseAtZero,
    /// The text shown in place of the countdown once it reaches zero, when the pause screen stays.
    pub zero_text: String,
    pub anchor: PauseAnchor,
    /// The text's height as a share of the canvas height, in percent.
    pub size_pct: u32,
    /// The font family; empty for the default.
    pub font: String,
    pub color: String,
    /// The outline's width in pixels.
    pub outline: u32,
    pub outline_color: String,
    /// Whether the program mix fades out while paused. Monitoring and each source's own audio
    /// track keep the source's audio.
    pub mute_scene: bool,
}

impl PauseSettings {
    /// The countdown lengths, in seconds (12 hours at most).
    pub const COUNTDOWN_S: std::ops::RangeInclusive<u32> = 0..=43_200;
    /// The text sizes, in percent of the canvas height.
    pub const SIZE_PCT: std::ops::RangeInclusive<u32> = 2..=15;
    /// The outline widths, in pixels.
    pub const OUTLINE: std::ops::RangeInclusive<u32> = 0..=64;
    /// Most characters in the countdown text and the text at zero.
    pub const MAX_TEXT: usize = 200;
    /// Longest media path, in bytes.
    pub const MAX_MEDIA_PATH: usize = 4096;
    /// Where the remaining time goes in the countdown text.
    pub const TIME: &'static str = "{time}";

    /// An error unless `text` is a countdown text: at most [`Self::MAX_TEXT`] characters with
    /// exactly one `{time}`.
    pub fn check_text(text: &str) -> Result<(), String> {
        if text.chars().count() > Self::MAX_TEXT {
            return Err(format!("the countdown text is longer than {} characters", Self::MAX_TEXT));
        }
        match text.matches(Self::TIME).count() {
            1 => Ok(()),
            0 => Err("the countdown text needs {time} where the remaining time goes".into()),
            _ => Err("the countdown text holds {time} more than once".into()),
        }
    }

    /// An error unless `path` can be a media path: at most [`Self::MAX_MEDIA_PATH`] bytes, with no
    /// NUL character.
    pub fn check_media(path: &str) -> Result<(), String> {
        if path.len() > Self::MAX_MEDIA_PATH || path.contains('\0') {
            return Err(format!("the pause media path must be at most {} bytes and hold no NUL character", Self::MAX_MEDIA_PATH));
        }
        Ok(())
    }

    /// The first value out of its range, as an error message.
    pub fn check(&self) -> Result<(), String> {
        let range = |name: &str, v: u32, r: std::ops::RangeInclusive<u32>| {
            if r.contains(&v) { Ok(()) } else { Err(format!("pause.{name} is {v}; it must be from {} to {}", r.start(), r.end())) }
        };
        range("countdown_s", self.countdown_s, Self::COUNTDOWN_S)?;
        range("size_pct", self.size_pct, Self::SIZE_PCT)?;
        range("outline", self.outline, Self::OUTLINE)?;
        Self::check_media(&self.media)?;
        Self::check_text(&self.text)?;
        if self.zero_text.chars().count() > Self::MAX_TEXT {
            return Err(format!("the text at zero is longer than {} characters", Self::MAX_TEXT));
        }
        for (name, c) in [("background", &self.background), ("color", &self.color), ("outline_color", &self.outline_color)] {
            if crate::settings::parse_color(c).is_none() {
                return Err(format!("pause.{name} is not a color like #rrggbbaa"));
            }
        }
        Ok(())
    }

    /// These settings with each number brought into its range, and each text, color or path that
    /// breaks a rule of [`PauseSettings::check`] replaced by its default.
    pub fn sanitized(self) -> Self {
        let d = Self::default();
        let clamp = |v: u32, r: std::ops::RangeInclusive<u32>| v.clamp(*r.start(), *r.end());
        let color = |c: String, default: String| if crate::settings::parse_color(&c).is_some() { c } else { default };
        Self {
            media: if Self::check_media(&self.media).is_ok() { self.media } else { d.media },
            countdown_s: clamp(self.countdown_s, Self::COUNTDOWN_S),
            text: if Self::check_text(&self.text).is_ok() { self.text } else { d.text },
            zero_text: if self.zero_text.chars().count() <= Self::MAX_TEXT { self.zero_text } else { d.zero_text },
            size_pct: clamp(self.size_pct, Self::SIZE_PCT),
            outline: clamp(self.outline, Self::OUTLINE),
            background: color(self.background, d.background),
            color: color(self.color, d.color),
            outline_color: color(self.outline_color, d.outline_color),
            ..self
        }
    }
}

impl Default for PauseSettings {
    fn default() -> Self {
        Self {
            media: String::new(),
            fit: crate::FitMode::Fit,
            background: "#101014ff".into(),
            countdown_s: 300,
            text: "Back in {time}".into(),
            at_zero: PauseAtZero::Keep,
            zero_text: "Back in a moment".into(),
            anchor: PauseAnchor::BottomCenter,
            size_pct: 6,
            font: String::new(),
            color: "#ffffffff".into(),
            outline: 4,
            outline_color: "#000000ff".into(),
            mute_scene: true,
        }
    }
}

/// A named set of video, audio, output, stream, replay buffer, audio tracks and pause screen
/// settings.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    pub name: String,
    pub video: VideoSettings,
    pub audio: AudioSettings,
    pub output: OutputSettings,
    pub stream: StreamSettings,
    pub replay: ReplaySettings,
    pub audio_tracks: AudioTracksSettings,
    pub pause: PauseSettings,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_settings_are_kept_in_range() {
        let v = VideoSettings { base_width: 3, base_height: 99_999, output_width: 1281, output_height: 720, fps_num: 0, fps_den: 0 }.sanitized();
        assert_eq!((v.base_width, v.base_height, v.output_width), (16, 8192, 1280));
        assert_eq!((v.fps_num, v.fps_den), (1, 1));
        let ntsc = VideoSettings { fps_num: 30_000, fps_den: 1001, ..VideoSettings::default() }.sanitized();
        assert!((ntsc.fps() - 29.97).abs() < 0.01);
    }

    #[test]
    fn output_scales_follow_the_canvas() {
        let v = VideoSettings { output_width: 1280, output_height: 720, ..VideoSettings::default() };
        assert_eq!(v.output_scale(), Some((2, 3)));
        assert_eq!(v.scaled_output((3, 4)), (1440, 810));
        // 1080 x 3 / 4 = 810 stays; 1366 / 2 = 683 rounds down to 682.
        let w = v.with_canvas(2560, 1440);
        assert_eq!((w.base_width, w.base_height, w.output_width, w.output_height), (2560, 1440, 1706, 960));
        let half = VideoSettings { base_width: 1366, base_height: 768, output_width: 682, output_height: 384, ..VideoSettings::default() };
        assert_eq!(half.output_scale(), Some((1, 2)));
        // A typed output size that is no share of the canvas stays as it is.
        let custom = VideoSettings { output_width: 1000, output_height: 500, ..VideoSettings::default() };
        assert_eq!(custom.output_scale(), None);
        let c = custom.with_canvas(1080, 1920);
        assert_eq!((c.base_width, c.base_height, c.output_width, c.output_height), (1080, 1920, 1000, 500));
        // Sides stay in range and even: a tiny canvas and a hostile denominator.
        let tiny = VideoSettings { base_width: 16, base_height: 16, output_width: 16, output_height: 16, ..VideoSettings::default() };
        assert_eq!(tiny.scaled_output((1, 2)), (16, 16));
        // Every share gives 16 x 16 here, so none is taken as chosen and the size stays.
        assert_eq!(tiny.output_scale(), None);
        let grown = tiny.with_canvas(1920, 1080);
        assert_eq!((grown.output_width, grown.output_height), (16, 16));
        assert_eq!(v.scaled_output((u32::MAX, 0)), (8192, 8192));
        let odd = v.with_canvas(1001, 99_999);
        assert_eq!((odd.base_width, odd.base_height, odd.output_width, odd.output_height), (1000, 8192, 666, 5460));
    }

    #[test]
    fn the_countdown_text_holds_time_exactly_once() {
        assert!(PauseSettings::check_text("Back in {time}").is_ok());
        assert!(PauseSettings::check_text("{time}").is_ok());
        assert!(PauseSettings::check_text("Back soon").unwrap_err().contains("{time}"));
        assert!(PauseSettings::check_text("{time} / {time}").unwrap_err().contains("more than once"));
        let at_limit = format!("{}{{time}}", "x".repeat(PauseSettings::MAX_TEXT - 6));
        assert!(PauseSettings::check_text(&at_limit).is_ok(), "200 characters");
        assert!(PauseSettings::check_text(&format!("x{at_limit}")).is_err(), "201 characters");
        // Characters are counted, not bytes.
        assert!(PauseSettings::check_text(&format!("{}{{time}}", "é".repeat(PauseSettings::MAX_TEXT - 6))).is_ok());
    }

    #[test]
    fn pause_settings_are_checked_and_brought_into_range() {
        let d = PauseSettings::default();
        assert!(d.check().is_ok());
        assert_eq!((d.countdown_s, d.at_zero, d.anchor, d.size_pct, d.mute_scene), (300, PauseAtZero::Keep, PauseAnchor::BottomCenter, 6, true));
        for bad in [
            PauseSettings { countdown_s: 43_201, ..d.clone() },
            PauseSettings { size_pct: 1, ..d.clone() },
            PauseSettings { size_pct: 16, ..d.clone() },
            PauseSettings { outline: 65, ..d.clone() },
            PauseSettings { text: "no placeholder".into(), ..d.clone() },
            PauseSettings { zero_text: "z".repeat(201), ..d.clone() },
            PauseSettings { background: "blue".into(), ..d.clone() },
            PauseSettings { media: "a\0b.png".into(), ..d.clone() },
        ] {
            assert!(bad.check().is_err(), "{bad:?}");
            assert_eq!(bad.clone().sanitized().check(), Ok(()), "{bad:?}");
        }
        let edges = PauseSettings { countdown_s: 43_200, size_pct: 15, outline: 0, ..d.clone() };
        assert_eq!(edges.check(), Ok(()));
        assert_eq!(edges.clone().sanitized(), edges);
        let s = PauseSettings { countdown_s: u32::MAX, size_pct: 0, text: "x".into(), color: "#zz".into(), ..d.clone() }.sanitized();
        assert_eq!((s.countdown_s, s.size_pct, s.text.as_str(), s.color.as_str()), (43_200, 2, "Back in {time}", "#ffffffff"));
    }

    #[test]
    fn pause_choices_have_stable_names_and_places() {
        assert_eq!(PauseAtZero::ALL.map(PauseAtZero::id), ["keep", "resume"]);
        assert_eq!(PauseAtZero::from_id("resume"), Some(PauseAtZero::Resume));
        assert_eq!(PauseAtZero::from_id("Resume"), None);
        assert_eq!(PauseAnchor::TopLeft.fractions(), (0.0, 0.0));
        assert_eq!(PauseAnchor::Center.fractions(), (0.5, 0.5));
        assert_eq!(PauseAnchor::BottomCenter.fractions(), (0.5, 1.0));
        assert_eq!(PauseAnchor::BottomRight.fractions(), (1.0, 1.0));
        let names: Vec<String> = PauseAnchor::ALL.iter().map(|a| serde_json::to_value(a).unwrap().as_str().unwrap_or("").to_string()).collect();
        assert_eq!(names[0], "top_left");
        assert_eq!(names[8], "bottom_right");
    }
}
