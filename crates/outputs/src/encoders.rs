//! Video encoders: the software x264 encoder and the hardware encoders ffmpeg offers, each an
//! [`Encoder`] whose [`FfmpegEncoder`] form gives the ffmpeg program its codec arguments, and the
//! [`Hardware`] choice, which the studio turns into one of them when an output starts.

use craft_hwenc::{Api, Candidate, Input};
use studiocraft_model::{ChoiceOption, Field, FieldKind, Settings, SettingsSchema};
use studiocraft_plugin::{Availability, Codec, Encoder, EncoderEnv, FfmpegEncoder, Plugin, PluginError, RateControl};

/// A video encoder that is one of ffmpeg's codecs.
pub struct VideoEncoder {
    id: &'static str,
    name: &'static str,
    /// What it writes.
    format: Codec,
    /// The ffmpeg codec.
    codec: &'static str,
    /// The hardware encoder it runs; `None` for x264.
    hardware: Option<&'static Candidate>,
}

impl VideoEncoder {
    /// The hardware encoder this is; `None` for the software encoder.
    pub fn hardware(&self) -> Option<&'static Candidate> {
        self.hardware
    }
}

fn codec_of(c: &Candidate) -> Codec {
    match c.codec {
        craft_hwenc::Codec::H264 => Codec::H264,
        craft_hwenc::Codec::Hevc => Codec::Hevc,
        craft_hwenc::Codec::Av1 => Codec::Av1,
    }
}

/// Every video encoder ffmpeg runs: x264, then each hardware encoder of [`craft_hwenc::CANDIDATES`].
pub fn all() -> Vec<VideoEncoder> {
    let x264 = VideoEncoder { id: "x264", name: "Software (x264)", format: Codec::H264, codec: "libx264", hardware: None };
    let hardware = craft_hwenc::CANDIDATES.iter().map(|c| VideoEncoder {
        id: c.id,
        name: c.name,
        format: codec_of(c),
        codec: c.ffmpeg_codec,
        hardware: Some(c),
    });
    std::iter::once(x264).chain(hardware).collect()
}

/// The id of the [`Hardware`] choice.
pub const HARDWARE: &str = "hardware";

/// The hardware encoder set in Settings › Video: the automatic choice or one named encoder. The
/// studio replaces it with the encoder it resolves to before the output starts, so it has no
/// ffmpeg form of its own.
pub struct Hardware;

impl Plugin for Hardware {
    fn id(&self) -> &'static str {
        HARDWARE
    }
    fn name(&self) -> &'static str {
        "Hardware (set in Settings › Video)"
    }
    fn schema(&self) -> SettingsSchema {
        SettingsSchema::default()
    }
}

impl Encoder for Hardware {
    fn codec(&self) -> Codec {
        Codec::H264
    }
    fn open(&self, _: &Settings, _: &studiocraft_plugin::VideoParams) -> Result<Box<dyn studiocraft_plugin::VideoSession>, PluginError> {
        Err(PluginError::Unavailable("the hardware choice is resolved to an encoder before an output starts".into()))
    }
    /// It runs wherever ffmpeg runs: without a working hardware encoder it becomes the software one.
    fn availability(&self, env: &EncoderEnv) -> Availability {
        if env.ffmpeg_encoders.is_some() { Availability::Ready } else { Availability::Unknown }
    }
}

const X264_PRESETS: [&str; 6] = ["ultrafast", "superfast", "veryfast", "faster", "fast", "medium"];
const NVENC_PRESETS: [&str; 7] = ["p1", "p2", "p3", "p4", "p5", "p6", "p7"];

impl Plugin for VideoEncoder {
    fn id(&self) -> &'static str {
        self.id
    }
    fn name(&self) -> &'static str {
        self.name
    }
    fn schema(&self) -> SettingsSchema {
        match self.hardware.map(|c| c.api) {
            None => {
                let options = X264_PRESETS.iter().map(|p| ChoiceOption::new(*p, *p)).collect();
                SettingsSchema::new(vec![Field::new("preset", "CPU usage preset (faster uses less CPU)", FieldKind::Choice { options }, "veryfast")])
            }
            Some(Api::Nvenc) => {
                let options = NVENC_PRESETS.iter().map(|p| ChoiceOption::new(*p, p.to_uppercase())).collect();
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
    fn codec(&self) -> Codec {
        self.format
    }
    fn ffmpeg(&self) -> Option<&dyn FfmpegEncoder> {
        Some(self)
    }
    /// A hardware encoder whose API this system lacks does not run here, whatever ffmpeg lists.
    fn availability(&self, env: &EncoderEnv) -> Availability {
        match (self.hardware, env.ffmpeg_encoders) {
            (Some(c), _) if !c.runs_on(craft_hwenc::Os::current()) => Availability::NotRunHere,
            (_, None) => Availability::Unknown,
            (_, Some(listed)) if listed.iter().any(|c| c == self.codec) => Availability::Ready,
            (_, Some(_)) => Availability::NotInFfmpeg,
        }
    }
}

impl FfmpegEncoder for VideoEncoder {
    fn ffmpeg_codec(&self) -> &'static str {
        self.codec
    }
    fn ffmpeg_args(&self, settings: &Settings, rate: RateControl, fps: f64) -> Result<Vec<String>, PluginError> {
        let fps = if fps.is_finite() && fps > 0.0 { fps } else { 30.0 };
        // The frames are NV12, as the compositor's GPU conversion writes them.
        let mut a = self.hardware.map(|c| c.prelude(Input::Nv12)).unwrap_or_default();
        a.extend([s("-c:v"), s(self.codec)]);
        let gop = |seconds: u32| s((fps * f64::from(seconds.clamp(1, 20))).round().max(1.0) as u64);
        let cbr = |a: &mut Vec<String>, kbps: u32| {
            a.extend([s("-b:v"), format!("{kbps}k"), s("-maxrate"), format!("{kbps}k"), s("-bufsize"), format!("{}k", kbps.saturating_mul(2))])
        };
        let no_lossless = || PluginError::Unavailable(format!("{} cannot record losslessly; choose the software encoder", self.name));
        let Some(hw) = self.hardware else {
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
            return Ok(a);
        };
        match hw.api {
            Api::Nvenc => {
                let preset = settings.str("preset").filter(|p| NVENC_PRESETS.contains(p)).unwrap_or("p5");
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
                    RateControl::Lossless if hw.codec == craft_hwenc::Codec::Av1 => return Err(no_lossless()),
                    RateControl::Lossless => a.extend([s("-tune"), s("lossless"), s("-g"), gop(5)]),
                }
            }
            Api::Amf => match rate {
                RateControl::Quality(q) => {
                    a.extend([s("-rc"), s("cqp"), s("-qp_i"), s(q), s("-qp_p"), s(q)]);
                    // Only the H.264 encoder writes B-frames with a quantizer of their own.
                    if hw.codec == craft_hwenc::Codec::H264 {
                        a.extend([s("-qp_b"), s(q)]);
                    }
                    a.extend([s("-g"), gop(5)]);
                }
                RateControl::Bitrate { kbps, keyframe_s } => {
                    a.extend([s("-rc"), s("cbr")]);
                    cbr(&mut a, kbps);
                    a.extend([s("-g"), gop(keyframe_s)]);
                }
                RateControl::Lossless => return Err(no_lossless()),
            },
            Api::Qsv => match rate {
                RateControl::Quality(q) => a.extend([s("-global_quality"), s(q), s("-g"), gop(5)]),
                RateControl::Bitrate { kbps, keyframe_s } => {
                    cbr(&mut a, kbps);
                    a.extend([s("-g"), gop(keyframe_s)]);
                }
                RateControl::Lossless => return Err(no_lossless()),
            },
            Api::MediaFoundation => match rate {
                RateControl::Quality(q) => {
                    a.extend([s("-rate_control"), s("quality"), s("-quality"), s(100u32.saturating_sub(q.saturating_mul(2))), s("-g"), gop(5)])
                }
                RateControl::Bitrate { kbps, keyframe_s } => {
                    a.extend([s("-rate_control"), s("cbr")]);
                    cbr(&mut a, kbps);
                    a.extend([s("-g"), gop(keyframe_s)]);
                }
                RateControl::Lossless => return Err(no_lossless()),
            },
            Api::VideoToolbox => match rate {
                // `-q:v` runs from 1 to 100, higher is better.
                RateControl::Quality(q) => a.extend([s("-q:v"), s(100u32.saturating_sub(q.saturating_mul(2)).max(1)), s("-g"), gop(5)]),
                RateControl::Bitrate { kbps, keyframe_s } => {
                    cbr(&mut a, kbps);
                    a.extend([s("-g"), gop(keyframe_s)]);
                }
                RateControl::Lossless => return Err(no_lossless()),
            },
            Api::Vaapi => match rate {
                RateControl::Quality(q) => a.extend([s("-rc_mode"), s("CQP"), s("-qp"), s(q.min(51)), s("-g"), gop(5)]),
                RateControl::Bitrate { kbps, keyframe_s } => {
                    a.extend([s("-rc_mode"), s("CBR")]);
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

    fn by_id(id: &str) -> VideoEncoder {
        all().into_iter().find(|e| e.id == id).unwrap()
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
    fn an_nvenc_preset_outside_its_choices_falls_back() {
        let nvenc = by_id("nvenc_h264");
        for bad in ["p0", "p8", "p9", "px", "P3"] {
            let a = nvenc.ffmpeg_args(&Settings::new().with("preset", bad), RateControl::Quality(23), 30.0).unwrap();
            assert_eq!(a[3], "p5", "{bad}");
        }
        let a = nvenc.ffmpeg_args(&Settings::new().with("preset", "p7"), RateControl::Quality(23), 30.0).unwrap();
        assert_eq!(a[3], "p7");
    }

    #[test]
    fn each_encoder_names_the_codec_its_ffmpeg_codec_writes() {
        for e in all() {
            let expected = match e.codec.split('_').next() {
                Some("libx264" | "h264") => Codec::H264,
                Some("hevc") => Codec::Hevc,
                Some("av1") => Codec::Av1,
                other => panic!("{other:?}"),
            };
            assert_eq!(e.codec(), expected, "{}", e.id);
            assert_eq!(e.ffmpeg().map(|f| f.ffmpeg_codec()), Some(e.codec), "{} runs in ffmpeg", e.id);
            assert_eq!(e.hardware().map(|c| c.ffmpeg_codec), (e.id != "x264").then_some(e.codec));
        }
    }

    #[test]
    fn hardware_encoders_without_lossless_say_so() {
        for id in ["amf_h264", "qsv_hevc", "mf_h264", "vt_h264", "vaapi_h264", "nvenc_av1"] {
            assert!(matches!(by_id(id).ffmpeg_args(&Settings::new(), RateControl::Lossless, 30.0), Err(PluginError::Unavailable(_))), "{id}");
        }
        assert!(by_id("nvenc_hevc").ffmpeg_args(&Settings::new(), RateControl::Lossless, 30.0).is_ok());
        let ids: Vec<_> = all().iter().map(|e| e.id).collect();
        assert_eq!(ids.len(), 1 + craft_hwenc::CANDIDATES.len());
        for old in ["x264", "nvenc_h264", "nvenc_hevc", "amf_h264", "qsv_h264", "mf_h264"] {
            assert!(ids.contains(&old), "saved profiles name {old}");
        }
    }

    #[test]
    fn vaapi_uploads_the_frames_and_videotoolbox_and_amf_take_their_own_quality_options() {
        let vaapi = by_id("vaapi_hevc").ffmpeg_args(&Settings::new(), RateControl::Quality(20), 30.0).unwrap().join(" ");
        let device = craft_hwenc::vaapi_device(by_id("vaapi_hevc").hardware().unwrap());
        assert!(vaapi.starts_with(&format!("-vaapi_device {device} -vf format=nv12,hwupload -c:v hevc_vaapi -rc_mode CQP -qp 20")), "{vaapi}");
        let vt = by_id("vt_h264").ffmpeg_args(&Settings::new(), RateControl::Quality(20), 30.0).unwrap().join(" ");
        assert_eq!(vt, "-c:v h264_videotoolbox -q:v 60 -g 150");
        let top = by_id("vt_h264").ffmpeg_args(&Settings::new(), RateControl::Quality(u32::MAX), 30.0).unwrap().join(" ");
        assert_eq!(top, "-c:v h264_videotoolbox -q:v 1 -g 150", "a value past the range does not overflow");
        let mf = by_id("mf_h264").ffmpeg_args(&Settings::new(), RateControl::Quality(u32::MAX), 30.0).unwrap().join(" ");
        assert!(mf.contains("-quality 0 "), "{mf}");
        let amf = by_id("amf_hevc").ffmpeg_args(&Settings::new(), RateControl::Quality(20), 30.0).unwrap().join(" ");
        assert_eq!(amf, "-c:v hevc_amf -rc cqp -qp_i 20 -qp_p 20 -g 150");
        let amf264 = by_id("amf_h264").ffmpeg_args(&Settings::new(), RateControl::Quality(20), 30.0).unwrap().join(" ");
        assert!(amf264.contains("-qp_b 20"), "{amf264}");
    }

    #[test]
    fn hardware_encoders_of_another_system_do_not_run_here_even_when_ffmpeg_lists_them() {
        let os = craft_hwenc::Os::current();
        let listed: Vec<String> = craft_hwenc::CANDIDATES.iter().map(|c| c.ffmpeg_codec.to_string()).collect();
        let env = EncoderEnv::ffmpeg(&listed);
        for c in &craft_hwenc::CANDIDATES {
            let expected = if c.runs_on(os) { Availability::Ready } else { Availability::NotRunHere };
            assert_eq!(by_id(c.id).availability(&env), expected, "{}", c.id);
        }
        assert_eq!(by_id("x264").availability(&EncoderEnv::ffmpeg(&["aac".to_string()])), Availability::NotInFfmpeg);
    }

    #[test]
    fn the_hardware_choice_runs_wherever_ffmpeg_runs_and_has_no_ffmpeg_form() {
        let listed = ["libx264".to_string()];
        assert_eq!(Hardware.availability(&EncoderEnv::ffmpeg(&listed)), Availability::Ready);
        assert_eq!(Hardware.availability(&EncoderEnv::ffmpeg(&[])), Availability::Unknown);
        assert!(Hardware.ffmpeg().is_none() && Hardware.streams());
        let params = studiocraft_plugin::VideoParams { width: 2, height: 2, fps_num: 30, fps_den: 1, rate: RateControl::Lossless };
        assert!(Hardware.open(&Settings::new(), &params).is_err());
    }
}
