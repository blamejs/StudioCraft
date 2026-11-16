//! Video encoders: the software x264 encoder and the hardware encoders ffmpeg offers.

use studiocraft_model::{ChoiceOption, Field, FieldKind, Settings, SettingsSchema};
use studiocraft_plugin::{Encoder, EncoderKind, Plugin, PluginError, RateControl};

/// The family of codec options an encoder takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Family {
    X264,
    Nvenc,
    Amf,
    Qsv,
    MediaFoundation,
}

/// A video encoder that is one of ffmpeg's codecs.
pub struct VideoEncoder {
    id: &'static str,
    name: &'static str,
    codec: &'static str,
    family: Family,
}

/// Every built-in video encoder, software first.
pub fn all() -> Vec<VideoEncoder> {
    vec![
        VideoEncoder { id: "x264", name: "Software (x264)", codec: "libx264", family: Family::X264 },
        VideoEncoder { id: "nvenc_h264", name: "NVIDIA NVENC H.264", codec: "h264_nvenc", family: Family::Nvenc },
        VideoEncoder { id: "nvenc_hevc", name: "NVIDIA NVENC HEVC", codec: "hevc_nvenc", family: Family::Nvenc },
        VideoEncoder { id: "amf_h264", name: "AMD AMF H.264", codec: "h264_amf", family: Family::Amf },
        VideoEncoder { id: "qsv_h264", name: "Intel Quick Sync H.264", codec: "h264_qsv", family: Family::Qsv },
        VideoEncoder { id: "mf_h264", name: "Windows Media Foundation H.264", codec: "h264_mf", family: Family::MediaFoundation },
    ]
}

const X264_PRESETS: [&str; 6] = ["ultrafast", "superfast", "veryfast", "faster", "fast", "medium"];

impl Plugin for VideoEncoder {
    fn id(&self) -> &'static str {
        self.id
    }
    fn name(&self) -> &'static str {
        self.name
    }
    fn schema(&self) -> SettingsSchema {
        match self.family {
            Family::X264 => {
                let options = X264_PRESETS.iter().map(|p| ChoiceOption::new(*p, *p)).collect();
                SettingsSchema::new(vec![Field::new("preset", "CPU usage preset (faster uses less CPU)", FieldKind::Choice { options }, "veryfast")])
            }
            Family::Nvenc => {
                let options = (1..=7).map(|i| ChoiceOption::new(format!("p{i}"), format!("P{i}"))).collect();
                SettingsSchema::new(vec![Field::new("preset", "Preset (higher is slower and better)", FieldKind::Choice { options }, "p5")])
            }
            _ => SettingsSchema::default(),
        }
    }
}

fn s(v: impl ToString) -> String {
    v.to_string()
}

impl Encoder for VideoEncoder {
    fn kind(&self) -> EncoderKind {
        EncoderKind::Video
    }
    fn ffmpeg_codec(&self) -> &'static str {
        self.codec
    }
    fn ffmpeg_args(&self, settings: &Settings, rate: RateControl, fps: f64) -> Result<Vec<String>, PluginError> {
        let fps = if fps.is_finite() && fps > 0.0 { fps } else { 30.0 };
        let mut a = vec![s("-c:v"), s(self.codec)];
        let gop = |seconds: u32| s((fps * f64::from(seconds.clamp(1, 20))).round().max(1.0) as u64);
        let cbr = |a: &mut Vec<String>, kbps: u32| {
            a.extend([s("-b:v"), format!("{kbps}k"), s("-maxrate"), format!("{kbps}k"), s("-bufsize"), format!("{}k", kbps.saturating_mul(2))])
        };
        let no_lossless = || PluginError::Unavailable(format!("{} cannot record losslessly; choose the software encoder", self.name));
        match self.family {
            Family::X264 => {
                let preset = settings.str("preset").filter(|p| X264_PRESETS.contains(p)).unwrap_or("veryfast");
                a.extend([s("-preset"), s(preset)]);
                match rate {
                    RateControl::Quality(q) => a.extend([s("-crf"), s(q.min(51)), s("-g"), gop(5)]),
                    RateControl::Bitrate { kbps, keyframe_s } => {
                        cbr(&mut a, kbps);
                        a.extend([s("-minrate"), format!("{kbps}k"), s("-x264-params"), s("nal-hrd=cbr"), s("-g"), gop(keyframe_s)]);
                    }
                    RateControl::Lossless => a.extend([s("-qp"), s(0), s("-g"), gop(5)]),
                }
                a.extend([s("-pix_fmt"), s("yuv420p")]);
            }
            Family::Nvenc => {
                let preset = settings.str("preset").filter(|p| p.len() == 2 && p.starts_with('p')).unwrap_or("p5");
                a.extend([s("-preset"), s(preset)]);
                match rate {
                    RateControl::Quality(q) => {
                        a.extend([s("-tune"), s("hq"), s("-rc"), s("vbr"), s("-cq"), s(q.min(51)), s("-b:v"), s(0), s("-g"), gop(5)])
                    }
                    RateControl::Bitrate { kbps, keyframe_s } => {
                        a.extend([s("-tune"), s("hq"), s("-rc"), s("cbr")]);
                        cbr(&mut a, kbps);
                        a.extend([s("-g"), gop(keyframe_s)]);
                    }
                    RateControl::Lossless => a.extend([s("-tune"), s("lossless"), s("-g"), gop(5)]),
                }
            }
            Family::Amf => match rate {
                RateControl::Quality(q) => a.extend([s("-rc"), s("cqp"), s("-qp_i"), s(q), s("-qp_p"), s(q), s("-qp_b"), s(q), s("-g"), gop(5)]),
                RateControl::Bitrate { kbps, keyframe_s } => {
                    a.extend([s("-rc"), s("cbr")]);
                    cbr(&mut a, kbps);
                    a.extend([s("-g"), gop(keyframe_s)]);
                }
                RateControl::Lossless => return Err(no_lossless()),
            },
            Family::Qsv => match rate {
                RateControl::Quality(q) => a.extend([s("-global_quality"), s(q), s("-g"), gop(5)]),
                RateControl::Bitrate { kbps, keyframe_s } => {
                    cbr(&mut a, kbps);
                    a.extend([s("-g"), gop(keyframe_s)]);
                }
                RateControl::Lossless => return Err(no_lossless()),
            },
            Family::MediaFoundation => match rate {
                RateControl::Quality(q) => {
                    a.extend([s("-rate_control"), s("quality"), s("-quality"), s(100u32.saturating_sub(q * 2)), s("-g"), gop(5)])
                }
                RateControl::Bitrate { kbps, keyframe_s } => {
                    a.extend([s("-rate_control"), s("cbr")]);
                    cbr(&mut a, kbps);
                    a.extend([s("-g"), gop(keyframe_s)]);
                }
                RateControl::Lossless => return Err(no_lossless()),
            },
        }
        Ok(a)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn x264() -> VideoEncoder {
        all().into_iter().next().unwrap()
    }

    #[test]
    fn x264_quality_and_bitrate_arguments() {
        let q = x264().ffmpeg_args(&Settings::new(), RateControl::Quality(23), 30.0).unwrap();
        assert_eq!(q[..6], ["-c:v", "libx264", "-preset", "veryfast", "-crf", "23"]);
        let b = x264().ffmpeg_args(&Settings::new().with("preset", "fast"), RateControl::Bitrate { kbps: 6000, keyframe_s: 2 }, 60.0).unwrap();
        let joined = b.join(" ");
        assert!(joined.contains("-preset fast"));
        assert!(joined.contains("-b:v 6000k -maxrate 6000k -bufsize 12000k"));
        assert!(joined.ends_with("-g 120 -pix_fmt yuv420p"), "{joined}");
    }

    #[test]
    fn an_unknown_preset_falls_back() {
        let a = x264().ffmpeg_args(&Settings::new().with("preset", "placebo; rm"), RateControl::Lossless, 30.0).unwrap();
        assert_eq!(a[3], "veryfast");
    }

    #[test]
    fn hardware_encoders_without_lossless_say_so() {
        let amf = all().into_iter().find(|e| e.id == "amf_h264").unwrap();
        assert!(matches!(amf.ffmpeg_args(&Settings::new(), RateControl::Lossless, 30.0), Err(PluginError::Unavailable(_))));
        let ids: Vec<_> = all().iter().map(|e| e.id).collect();
        assert_eq!(ids.len(), 6);
    }
}
