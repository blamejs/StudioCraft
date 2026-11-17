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

/// Audio format of the mix.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioSettings {
    pub sample_rate: u32,
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self { sample_rate: 48_000 }
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

/// A named set of video, audio, output and stream settings.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    pub name: String,
    pub video: VideoSettings,
    pub audio: AudioSettings,
    pub output: OutputSettings,
    pub stream: StreamSettings,
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
