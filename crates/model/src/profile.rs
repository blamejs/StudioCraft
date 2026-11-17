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

/// Where and how to stream.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StreamSettings {
    /// The RTMP server, `rtmp://host/app` or `rtmps://host/app`.
    pub server: String,
    /// The stream key the service gave the user.
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
            server: String::new(),
            key: String::new(),
            video_bitrate_kbps: 6000,
            audio_bitrate_kbps: 160,
            video_encoder: "x264".into(),
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

/// A named set of video, audio, output, stream and replay buffer settings.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    pub name: String,
    pub video: VideoSettings,
    pub audio: AudioSettings,
    pub output: OutputSettings,
    pub stream: StreamSettings,
    pub replay: ReplaySettings,
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
