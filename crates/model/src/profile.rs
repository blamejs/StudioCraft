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
}

/// Audio format of the mix, and the device monitored sources play on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioSettings {
    pub sample_rate: u32,
    /// The output device monitored sources play on, by its name; [`DEFAULT_AUDIO_DEVICE`] (or
    /// empty) for the system's default output. A name the system does not list is reported
    /// when monitoring starts.
    pub monitoring_device: String,
}

/// The device name that stands for the system's default device.
pub const DEFAULT_AUDIO_DEVICE: &str = "default";

impl Default for AudioSettings {
    fn default() -> Self {
        Self { sample_rate: 48_000, monitoring_device: DEFAULT_AUDIO_DEVICE.into() }
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
}

impl Default for Destination {
    fn default() -> Self {
        Self { id: String::new(), name: String::new(), enabled: true, server: String::new(), key: KeyRef::None }
    }
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
        }
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

    /// The first problem with the destinations, as an error message: too many, an empty or long
    /// id, or an id used twice.
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

/// Audio tracks: each source set to record its own track, and the program mix, written to WAV
/// files beside the recording.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
}

impl AudioTracksSettings {
    /// The write buffers the audio tracks take, in MiB.
    pub const BUFFER_MIB: std::ops::RangeInclusive<u32> = 16..=1024;
    /// The file sizes the audio tracks take, in GiB.
    pub const SEGMENT_GIB: std::ops::RangeInclusive<u32> = 1..=3;

    /// The first value out of its range, as an error message.
    pub fn check(&self) -> Result<(), String> {
        let range = |name: &str, v: u32, r: std::ops::RangeInclusive<u32>| {
            if r.contains(&v) { Ok(()) } else { Err(format!("audio_tracks.{name} is {v}; it must be from {} to {}", r.start(), r.end())) }
        };
        range("buffer_mib", self.buffer_mib, Self::BUFFER_MIB)?;
        range("segment_gib", self.segment_gib, Self::SEGMENT_GIB)
    }

    /// These settings with each size brought into its range.
    pub fn sanitized(self) -> Self {
        let clamp = |v: u32, r: std::ops::RangeInclusive<u32>| v.clamp(*r.start(), *r.end());
        Self { buffer_mib: clamp(self.buffer_mib, Self::BUFFER_MIB), segment_gib: clamp(self.segment_gib, Self::SEGMENT_GIB), ..self }
    }
}

impl Default for AudioTracksSettings {
    fn default() -> Self {
        Self { enabled: false, when: AudioTracksWhen::Recording, include_program: true, buffer_mib: 96, segment_gib: 1 }
    }
}

/// A named set of video, audio, output, stream, replay buffer and audio tracks settings.
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
}
