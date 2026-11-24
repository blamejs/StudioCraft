use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use studiocraft_media::Tick;
use studiocraft_media::audio::{BLOCK_FRAMES, BLOCK_SAMPLES, block_pool};
use studiocraft_media::clock::tick_of;
use studiocraft_media::pool::Pool;
use studiocraft_media::queue::Fifo;
use studiocraft_media::stats::EncoderStats;
use studiocraft_media::video::{PixelFormat, VideoFrame};
use studiocraft_platform::ffmpeg;
use studiocraft_plugin::VideoFormat;

use super::*;

#[test]
fn a_stream_reads_its_destinations_or_one_server() {
    let one = stream_targets(&Settings::new().with("server", "rtmp://x/app").with("key", "k")).unwrap();
    assert_eq!(one, vec![Target { id: "main".into(), server: "rtmp://x/app".into(), key: "k".into(), enabled: true }]);
    let list = serde_json::json!([
        { "id": "a", "server": "rtmp://a/live", "key": "1" },
        { "id": "b", "server": "rtmps://b/live", "key": null, "enabled": false },
    ]);
    let two = stream_targets(&Settings::new().with("destinations", list).with("server", "rtmp://ignored/live")).unwrap();
    assert_eq!(two.iter().map(|t| (t.id.as_str(), t.enabled)).collect::<Vec<_>>(), [("a", true), ("b", false)]);
    assert_eq!(two[1].key, "");
    for bad in [serde_json::json!({}), serde_json::json!([1]), serde_json::json!([{ "id": 2 }]), serde_json::json!([{ "id": "a", "enabled": "yes" }])]
    {
        assert!(stream_targets(&Settings::new().with("destinations", bad.clone())).is_err(), "{bad}");
    }
    // A list longer than a stream takes is refused before its entries are read.
    let most = craft_packet::MAX_SUBSCRIBERS;
    let long: Vec<serde_json::Value> = (0..=most).map(|_| serde_json::json!(1)).collect();
    let Err(e) = stream_targets(&Settings::new().with("destinations", long)) else { panic!("a list that is too long") };
    assert!(e.to_string().contains(&format!("at most {most} destinations")), "{e}");
}

#[test]
fn out_of_range_settings_are_refused() {
    assert_eq!(stream_reconnect(&Settings::new()), Ok(ReconnectSettings::default()), "no settings are the defaults");
    let some = Settings::new().with("reconnect", serde_json::json!({ "max_tries": 4 }));
    assert_eq!(stream_reconnect(&some), Ok(ReconnectSettings { max_tries: 4, ..ReconnectSettings::default() }));
    let all = serde_json::json!({ "enabled": false, "first_delay_s": 60, "max_delay_s": 600, "max_tries": 100 });
    let want = ReconnectSettings { enabled: false, first_delay_s: 60, max_delay_s: 600, max_tries: 100 };
    assert_eq!(stream_reconnect(&Settings::new().with("reconnect", all)), Ok(want));
    for (bad, says) in [
        (serde_json::json!([]), "not an object"),
        (serde_json::json!({ "enabled": "yes" }), "enabled"),
        (serde_json::json!({ "first_delay_s": 0 }), "first reconnect wait"),
        (serde_json::json!({ "first_delay_s": 61, "max_delay_s": 100 }), "first reconnect wait"),
        (serde_json::json!({ "first_delay_s": 1.5 }), "first_delay_s"),
        (serde_json::json!({ "max_delay_s": 1 }), "longest reconnect wait"),
        (serde_json::json!({ "max_delay_s": 601 }), "longest reconnect wait"),
        (serde_json::json!({ "max_tries": -1 }), "max_tries"),
        (serde_json::json!({ "max_tries": 4_294_967_296_u64 }), "max_tries"),
        (serde_json::json!({ "max_tries": 101 }), "reconnect tries"),
        (serde_json::json!({ "max_tries": null }), "max_tries"),
    ] {
        let Err(e) = stream_reconnect(&Settings::new().with("reconnect", bad.clone())) else { panic!("{bad} was taken") };
        assert!(e.to_string().contains(says), "{bad}: {e}");
    }
}

#[test]
fn a_stream_copy_is_an_mkv_file_or_none() {
    assert_eq!(stream_copy(&Settings::new()), Ok(None));
    assert_eq!(stream_copy(&Settings::new().with("copy", "")), Ok(None));
    assert_eq!(stream_copy(&Settings::new().with("copy", serde_json::Value::Null)), Ok(None));
    assert_eq!(stream_copy(&Settings::new().with("copy", "/v/Stream 1.MKV")), Ok(Some(PathBuf::from("/v/Stream 1.MKV"))));
    for (bad, says) in [
        (serde_json::json!("/v/Stream.mp4"), "is not one"),
        (serde_json::json!("/v/Stream"), "does not end in .mkv"),
        (serde_json::json!(7), "not a file name"),
        (serde_json::json!({"path": "/v/a.mkv"}), "not a file name"),
    ] {
        let Err(e) = stream_copy(&Settings::new().with("copy", bad.clone())) else { panic!("{bad} was taken") };
        assert!(e.to_string().contains(says), "{bad}: {e}");
    }
}

#[test]
fn a_stream_needs_an_h264_encoder() {
    use studiocraft_plugin::Encoder;
    let streamable: Vec<&str> = encoders::all().iter().filter(|e| e.streams()).map(|e| e.id()).collect();
    assert!(streamable.contains(&"x264") && !streamable.contains(&"nvenc_hevc"), "{streamable:?}");
    let Some(hevc) = encoders::all().into_iter().find(|e| e.id() == "nvenc_hevc") else { panic!("no HEVC encoder") };
    let mut j = job(PathBuf::from("no-such-ffmpeg"), RateControl::Bitrate { kbps: 1000, keyframe_s: 2 });
    j.video_encoder = Arc::new(hevc);
    let Err(e) = Stream.start(&Settings::new().with("server", "rtmp://127.0.0.1:9/live"), j) else { panic!("the stream started") };
    assert!(e.to_string().contains("H.264"), "{e}");
}

#[test]
fn an_audio_encoder_is_refused_as_the_video_encoder() {
    use studiocraft_plugin::{Codec, Encoder, FfmpegEncoder};
    struct Aac;
    impl Plugin for Aac {
        fn id(&self) -> &'static str {
            "aac"
        }
        fn name(&self) -> &'static str {
            "AAC"
        }
        fn schema(&self) -> SettingsSchema {
            SettingsSchema::default()
        }
    }
    impl FfmpegEncoder for Aac {
        fn ffmpeg_codec(&self) -> &'static str {
            "aac"
        }
        fn ffmpeg_args(&self, _: &Settings, _: RateControl, _: f64) -> Result<Vec<String>, PluginError> {
            Ok(vec!["-c:v".into(), "aac".into()])
        }
    }
    impl Encoder for Aac {
        fn codec(&self) -> Codec {
            Codec::Aac
        }
        fn ffmpeg(&self) -> Option<&dyn FfmpegEncoder> {
            Some(self)
        }
    }
    assert!(Aac.streams(), "FLV carries AAC audio");
    let mut j = job(PathBuf::from("no-such-ffmpeg"), RateControl::Bitrate { kbps: 1000, keyframe_s: 2 });
    j.video_encoder = Arc::new(Aac);
    let Err(e) = codec_args(&j, None) else { panic!("an audio encoder gave video arguments") };
    assert!(e.to_string().contains("AAC is not a video encoder"), "{e}");
    let Err(e) = Stream.start(&Settings::new().with("server", "rtmp://127.0.0.1:9/live"), j) else { panic!("the stream started") };
    assert!(e.to_string().contains("H.264"), "{e}");
}

#[test]
fn containers_follow_the_extension() {
    assert_eq!(container_for(Path::new("a/b.MKV")).unwrap(), "matroska");
    assert_eq!(container_for(Path::new("b.mp4")).unwrap(), "mp4");
    assert!(container_for(Path::new("b.avi")).is_err());
}

#[test]
fn lossless_recordings_keep_lossless_audio_in_every_container() {
    assert_eq!(lossless_audio_codec("matroska"), "flac");
    assert_eq!(lossless_audio_codec("mp4"), "alac");
    assert_eq!(lossless_audio_codec("mov"), "alac");
    let Ok(program) = ffmpeg::locate("") else { return };
    for ext in ["mkv", "mp4", "mov"] {
        let dir = temp_dir(&format!("lossless-{ext}"));
        let file = dir.join(format!("take.{ext}"));
        let mut out =
            Recording.start(&Settings::new().with("path", file.to_string_lossy().as_ref()), job(program.clone(), RateControl::Lossless)).unwrap();
        feed(out.as_ref(), 0.5);
        let r = out.stop();
        let (_, has_audio) = probe(&program, &file);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(r.is_ok(), "{ext}: {r:?}");
        assert!(has_audio, "{ext}");
    }
}

fn job(program: PathBuf, rate: RateControl) -> OutputJob {
    let x264 = encoders::all().into_iter().next().unwrap();
    OutputJob {
        video: VideoFormat { width: 64, height: 48, fps_num: 10, fps_den: 1, pixel: PixelFormat::Nv12 },
        sample_rate: 48_000,
        video_encoder: Arc::new(x264),
        video_encoder_settings: Settings::new().with("preset", "ultrafast"),
        rate,
        audio_bitrate_kbps: 128,
        ffmpeg: program,
        stats: Arc::new(EncoderStats::default()),
        start: Tick(0),
        captions: None,
    }
}

/// Feed `seconds` of gray frames at 10 per second and a tone, stamped as the pipeline would.
fn feed(out: &dyn OutputInstance, seconds: f64) {
    let frames = Pool::new(8, VideoFrame::default);
    let blocks = block_pool();
    let (video, audio) = (out.video(), out.audio());
    let total_blocks = (seconds * 48_000.0 / BLOCK_FRAMES as f64) as u64;
    let mut sent_frames = 0u64;
    for b in 0..total_blocks {
        let t = b as f64 * BLOCK_FRAMES as f64 / 48_000.0;
        let mut block = blocks.take();
        block.samples.clear();
        block.samples.extend((0..BLOCK_SAMPLES).map(|i| ((i as f32) * 0.05).sin() * 0.2));
        block.pts = tick_of(Duration::from_secs_f64(t));
        audio.push(block);
        while (sent_frames as f64) / 10.0 <= t {
            let mut f = frames.take();
            assert!(f.reshape(64, 48, PixelFormat::Nv12));
            f.data.fill(128);
            f.pts = tick_of(Duration::from_secs_f64(sent_frames as f64 / 10.0));
            video.push(Arc::new(f));
            sent_frames += 1;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Decoded video frames in `file`, and whether it has audio.
fn probe(program: &Path, file: &Path) -> (u64, bool) {
    let count = |map: &str| {
        let out = ffmpeg::command(program)
            .args(["-hide_banner", "-v", "error", "-i"])
            .arg(file)
            .args(["-map", map, "-f", "null", "-progress", "pipe:1", "-"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        (out.status.success(), text)
    };
    let (ok, text) = count("0:v:0");
    assert!(ok, "the video decodes");
    let frames = text.lines().rev().filter_map(|l| l.strip_prefix("frame=")).find_map(|v| v.trim().parse().ok()).unwrap_or(0);
    (frames, count("0:a:0").0)
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("studiocraft-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn records_a_playable_file_with_video_and_audio() {
    // Machines without ffmpeg pass without checking.
    let Ok(program) = ffmpeg::locate("") else { return };
    let dir = temp_dir("record");
    let file = dir.join("take.mkv");
    let j = job(program.clone(), RateControl::Quality(23));
    let stats = Arc::clone(&j.stats);
    let mut out = Recording.start(&Settings::new().with("path", file.to_string_lossy().as_ref()), j).unwrap();
    feed(out.as_ref(), 1.5);
    assert!(matches!(out.status(), OutputStatus::Running), "{:?}", out.status());
    out.stop().unwrap();
    assert_eq!(out.status(), OutputStatus::Stopped);
    let (frames, has_audio) = probe(&program, &file);
    let _ = std::fs::remove_dir_all(&dir);
    assert!((13..=17).contains(&frames), "{frames} frames for 1.5 s at 10 per second");
    assert!(has_audio);
    assert!(stats.frames.load(std::sync::atomic::Ordering::Relaxed) >= 13);
}

#[test]
fn a_built_in_recording_plays_in_ffmpeg() {
    // An independent player reads the file the built-in encoders write; machines without ffmpeg
    // pass without checking.
    let Ok(program) = ffmpeg::locate("") else { return };
    let dir = temp_dir("record-built-in");
    let file = dir.join("take.mkv");
    let mut j = job(PathBuf::new(), RateControl::Quality(23));
    j.video_encoder = Arc::new(studiocraft_encode::H264);
    j.video_encoder_settings = Settings::new();
    let mut out = Recording.start(&Settings::new().with("path", file.to_string_lossy().as_ref()), j).unwrap();
    feed(out.as_ref(), 1.5);
    out.stop().unwrap();
    let (frames, has_audio) = probe(&program, &file);
    let _ = std::fs::remove_dir_all(&dir);
    assert!((13..=17).contains(&frames), "{frames} frames for 1.5 s at 10 per second");
    assert!(has_audio);
}

#[test]
fn the_built_in_encoders_are_listed_after_the_ffmpeg_ones() {
    use studiocraft_plugin::Encoder;
    let mut reg = Registry::new();
    register_all(&mut reg).unwrap();
    let ids: Vec<&str> = reg.encoders().iter().map(|e| e.id()).collect();
    let built_in = [
        "h264",
        "nvenc_native_h264",
        "nvenc_native_hevc",
        "nvenc_native_av1",
        "mf_native_h264",
        "mf_native_hevc",
        "mf_native_av1",
        "vt_native_h264",
        "vt_native_hevc",
        "vaapi_native_h264",
        "vaapi_native_hevc",
        "vaapi_native_av1",
    ];
    assert_eq!(ids[ids.len().saturating_sub(13)..], [&["hardware"][..], &built_in[..]].concat(), "{ids:?}");
    for id in built_in {
        assert!(reg.encoder(id).unwrap().ffmpeg().is_none(), "{id}");
    }
    assert_eq!(reg.describe()["encoders"].as_array().and_then(|a| a.last()).map(|e| e["builtin"].clone()), Some(true.into()));
    let params = studiocraft_plugin::VideoParams { width: 64, height: 48, fps_num: 30, fps_den: 1, rate: RateControl::Quality(23) };
    for e in encoders::all() {
        let Err(err) = e.open(&Settings::new(), &params) else { panic!("{} opened a session", e.id()) };
        assert!(matches!(err, PluginError::Unavailable(_)) && err.to_string().contains("inside the ffmpeg program"), "{err}");
    }
}

#[test]
fn a_stream_refuses_an_ffmpeg_encoder() {
    let j = job(PathBuf::from("no-such-ffmpeg"), RateControl::Bitrate { kbps: 1000, keyframe_s: 2 });
    let Err(e) = Stream.start(&Settings::new().with("server", "rtmp://127.0.0.1:9/live").with("key", "k"), j) else { panic!("the stream started") };
    assert!(e.to_string().contains("runs inside ffmpeg"), "{e}");
}

#[test]
fn a_frame_of_another_size_fails_the_recording() {
    let mut jobs = Vec::new();
    let mut built_in = job(PathBuf::new(), RateControl::Quality(23));
    built_in.video_encoder = Arc::new(studiocraft_encode::H264);
    built_in.video_encoder_settings = Settings::new();
    jobs.push(("built-in", built_in));
    // The ffmpeg writer is checked where ffmpeg is installed.
    if let Ok(program) = ffmpeg::locate("") {
        jobs.push(("ffmpeg", job(program, RateControl::Quality(23))));
    }
    for (name, j) in jobs {
        let dir = temp_dir(&format!("record-wrong-size-{name}"));
        let file = dir.join("take.mkv");
        let mut out = Recording.start(&Settings::new().with("path", file.to_string_lossy().as_ref()), j).unwrap();
        let frames = Pool::new(1, VideoFrame::default);
        let mut f = frames.take();
        assert!(f.reshape(32, 24, PixelFormat::Nv12));
        out.video().push(Arc::new(f));
        let r = out.stop();
        let _ = std::fs::remove_dir_all(&dir);
        let Err(e) = r else { panic!("{name}: the recording finished") };
        assert!(e.to_string().contains("does not have the output's size"), "{name}: {e}");
    }
}

#[test]
fn a_recording_stopped_at_once_is_still_finished() {
    let Ok(program) = ffmpeg::locate("") else { return };
    let dir = temp_dir("record-at-once");
    let file = dir.join("take.mkv");
    let mut out = Recording.start(&Settings::new().with("path", file.to_string_lossy().as_ref()), job(program, RateControl::Quality(23))).unwrap();
    // A tenth of a second, stopped before ffmpeg has had time to connect for audio.
    feed(out.as_ref(), 0.1);
    let r = out.stop();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(r.is_ok(), "{r:?}");
}

/// [`job`] with the built-in H.264 encoder at a bitrate, as a stream runs.
fn stream_job(keyframe_s: u32) -> OutputJob {
    let mut j = job(PathBuf::new(), RateControl::Bitrate { kbps: 500, keyframe_s });
    j.video_encoder = Arc::new(studiocraft_encode::H264);
    j.video_encoder_settings = Settings::new();
    j
}

/// An independent receiver: the ffmpeg program reads the stream and writes it to an FLV file.
#[test]
fn streams_over_rtmp_to_a_local_server() {
    let Ok(program) = ffmpeg::locate("") else { return };
    let dir = temp_dir("stream");
    let received = dir.join("received.flv");
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let url = format!("rtmp://127.0.0.1:{port}/live");
    let mut server = ffmpeg::command(&program)
        .args(["-hide_banner", "-v", "error", "-y", "-listen", "1", "-i"])
        .arg(format!("{url}/secret"))
        .args(["-c", "copy", "-f", "flv"])
        .arg(&received)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(700));
    let mut job = stream_job(1);
    let phrases = Arc::new(Fifo::new(8));
    job.captions = Some(Arc::clone(&phrases));
    let mut out = Stream.start(&Settings::new().with("server", url.as_str()).with("key", "secret"), job).unwrap();
    assert_eq!(out.target(), url, "the key is not shown");
    // The second phrase wraps to a new line, so the first line is complete.
    phrases.push("checked by another decoder".to_string());
    phrases.push("second line".to_string());
    feed(out.as_ref(), 1.5);
    out.stop().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while server.try_wait().unwrap().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = server.kill();
    let (frames, has_audio) = probe(&program, &received);
    // ffmpeg reads the closed captions out of the H.264 stream it received.
    let srt = ffmpeg::command(&program)
        .current_dir(&dir)
        .args(["-hide_banner", "-v", "error", "-f", "lavfi", "-i", "movie=received.flv[out0+subcc]", "-map", "0:s", "-f", "srt", "-"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .unwrap();
    let srt = String::from_utf8_lossy(&srt.stdout).to_string();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(frames >= 10, "{frames}");
    assert!(has_audio);
    assert!(srt.contains("checked by another decoder"), "{srt}");
}
