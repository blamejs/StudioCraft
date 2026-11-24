use std::path::PathBuf;
use std::sync::mpsc;

use craft_media::matroska::Demuxer;
use studiocraft_media::audio::{BLOCK_SAMPLES, block_pool};
use studiocraft_media::clock::tick_of;
use studiocraft_media::pool::Pool;
use studiocraft_media::video::VideoFrame;
use studiocraft_model::{Settings, SettingsSchema};
use studiocraft_plugin::{Codec, Encoder, Output, Plugin, RateControl, VideoFormat};

use super::*;
use crate::Recording;

pub(crate) fn job(rate: RateControl) -> OutputJob {
    OutputJob {
        video: VideoFormat { width: 64, height: 48, fps_num: 10, fps_den: 1, pixel: PixelFormat::Nv12 },
        sample_rate: SAMPLE_RATE,
        video_encoder: Arc::new(studiocraft_encode::H264),
        video_encoder_settings: Settings::new(),
        rate,
        audio_bitrate_kbps: 128,
        ffmpeg: PathBuf::new(),
        stats: Arc::new(EncoderStats::default()),
        start: Tick(0),
        captions: None,
    }
}

pub(crate) fn temp_file(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("studiocraft-native-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("take.mkv")
}

/// Feed `seconds` of frames at 10 per second, whose luma steps with the frame number, and a
/// tone, stamped as the pipeline would. Each frame waits for room in the video queue, so none is
/// dropped.
fn feed(video: &Fifo<Arc<PooledFrame>>, audio: &Fifo<PooledBlock>, seconds: f64) -> u64 {
    Feeder::default().feed(video, audio, seconds)
}

/// [`feed`] in parts: each call goes on where the last one ended.
#[derive(Default)]
pub(crate) struct Feeder {
    blocks: u64,
    frames: u64,
}

impl Feeder {
    /// Feed `seconds` more; the number of frames sent by every call so far.
    pub(crate) fn feed(&mut self, video: &Fifo<Arc<PooledFrame>>, audio: &Fifo<PooledBlock>, seconds: f64) -> u64 {
        let total_blocks = self.blocks + (seconds * f64::from(SAMPLE_RATE) / BLOCK_FRAMES as f64) as u64;
        self.frames = feed_blocks(video, audio, self.blocks..total_blocks, self.frames);
        self.blocks = total_blocks;
        self.frames
    }
}

fn feed_blocks(video: &Fifo<Arc<PooledFrame>>, audio: &Fifo<PooledBlock>, range: std::ops::Range<u64>, mut sent: u64) -> u64 {
    let frames = Pool::new(16, VideoFrame::default);
    let blocks = block_pool();
    for b in range {
        let t = b as f64 * BLOCK_FRAMES as f64 / f64::from(SAMPLE_RATE);
        let mut block = blocks.take();
        block.samples.clear();
        block.samples.extend((0..BLOCK_SAMPLES).map(|i| ((i as f32) * 0.05).sin() * 0.2));
        block.pts = tick_of(Duration::from_secs_f64(t));
        while audio.len() >= 64 {
            std::thread::yield_now();
        }
        audio.push(block);
        while (sent as f64) / 10.0 <= t {
            while video.len() >= 4 {
                std::thread::yield_now();
            }
            let mut f = frames.take();
            assert!(f.reshape(64, 48, PixelFormat::Nv12));
            f.data.fill(128);
            f.data[..64 * 48].fill(16 + (sent % 200) as u8);
            f.pts = tick_of(Duration::from_secs_f64(sent as f64 / 10.0));
            video.push(Arc::new(f));
            sent += 1;
        }
    }
    sent
}

/// The packets of `file`, by track, as (presentation time, keyframe, data).
pub(crate) fn packets(file: &[u8]) -> (Vec<craft_media::matroska::Track>, [Vec<(i64, bool, Vec<u8>)>; 2]) {
    let mut d = Demuxer::from_slice(file).unwrap();
    let tracks = d.tracks().to_vec();
    let mut by_track = [Vec::new(), Vec::new()];
    while let Ok(Some(p)) = d.next_packet() {
        by_track[p.track].push((p.pts_ns, p.keyframe, p.data));
    }
    (tracks, by_track)
}

/// Decode `video` (in file order) with the `avcC` record `avc`; the luma of each picture's first
/// sample, in display order.
pub(crate) fn decode(avc: &[u8], video: &[(i64, bool, Vec<u8>)]) -> Vec<u8> {
    let mut dec = craft_video::h264::decode::Decoder::from_avcc(avc).unwrap();
    let mut pictures = Vec::new();
    for (pts, _, data) in video {
        pictures.extend(dec.decode(data, *pts).unwrap());
    }
    pictures.extend(dec.flush());
    pictures.sort_by_key(|p| p.pts);
    pictures.iter().map(|p| p.y[0]).collect()
}

#[test]
fn a_recording_through_the_built_in_encoders_has_every_frame_and_the_sound() {
    let path = temp_file("whole");
    let j = job(RateControl::Quality(20));
    let stats = Arc::clone(&j.stats);
    let mut out = Recording.start(&Settings::new().with("path", path.to_string_lossy().as_ref()), j).unwrap();
    assert!(matches!(out.status(), OutputStatus::Starting | OutputStatus::Running));
    let sent = feed(&out.video(), &out.audio(), 1.5);
    out.stop().unwrap();
    assert_eq!(out.status(), OutputStatus::Stopped);
    let file = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
    assert_eq!(stats.bytes.load(Ordering::Relaxed), file.len() as u64);
    assert_eq!(stats.frames.load(Ordering::Relaxed), sent);

    let (tracks, [video, audio]) = packets(&file);
    assert_eq!(tracks[0].codec_id, "V_MPEG4/ISO/AVC");
    assert_eq!(tracks[1].codec_id, "A_AAC");
    assert_eq!(tracks[1].codec_delay_ns, 21_333_333, "1024 samples at 48 kHz");
    assert_eq!(video.len() as u64, sent);
    assert!(video[0].1, "the file starts with a keyframe");
    let mut shown: Vec<i64> = video.iter().map(|p| p.0).collect();
    shown.sort_unstable();
    let want: Vec<i64> = (0..sent as i64).map(|n| n * 100_000_000).collect();
    assert_eq!(shown, want, "one frame every 100 ms from 0");
    let lumas = decode(&tracks[0].codec_private, &video);
    let want: Vec<u8> = (0..sent).map(|n| 16 + n as u8).collect();
    assert!(lumas.iter().zip(&want).all(|(a, b)| a.abs_diff(*b) <= 2), "{lumas:?}");
    // 70 blocks of 1024 samples, plus the encoder's delay.
    assert!(audio.len() >= 70, "{} audio frames", audio.len());
    let mut dec = craft_audio::aac::Decoder::new(&tracks[1].codec_private).unwrap();
    assert_eq!(dec.decode(&audio[0].2).unwrap().len(), 2);
    assert!(audio.windows(2).all(|w| w[1].0 > w[0].0), "the sound is in time order");
}

#[test]
fn a_file_cut_short_plays_up_to_its_last_whole_part() {
    let path = temp_file("cut");
    let j = job(RateControl::Quality(20));
    let mut n = Native::start(&j, &path).unwrap();
    let sent = feed(&n.video(), &n.audio(), 5.0);
    n.stop().unwrap();
    let file = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
    let (tracks, [whole, _]) = packets(&file);
    assert_eq!(whole.len() as u64, sent);
    let keyframes: Vec<i64> = whole.iter().filter(|p| p.1).map(|p| p.0).collect();
    assert_eq!(keyframes, [0, 2_000_000_000, 4_000_000_000], "a keyframe every 2 s");
    // A file cut anywhere, as by a power cut, still opens and decodes from its start.
    for cut in [file.len() * 3 / 10, file.len() / 2, file.len() * 9 / 10, file.len() - 1] {
        let (_, [video, audio]) = packets(&file[..cut]);
        assert!(video.len() >= 10 && !audio.is_empty(), "cut at {cut}: {} frames", video.len());
        assert!(video[0].1);
        let lumas = decode(&tracks[0].codec_private, &video);
        assert_eq!(lumas.len(), video.len(), "cut at {cut}");
        assert!(lumas.iter().enumerate().all(|(i, l)| l.abs_diff(16 + i as u8) <= 2), "cut at {cut}: {lumas:?}");
    }
}

#[test]
fn a_bitrate_recording_has_a_keyframe_every_two_seconds_and_plays_when_cut() {
    let path = temp_file("cut-bitrate");
    let Err(e) = Native::start(&job(RateControl::Bitrate { kbps: 800, keyframe_s: 3 }), &path) else {
        panic!("a recording with a keyframe every 3 s started")
    };
    assert!(e.to_string().contains("at least every 2 seconds"), "{e}");
    let j = job(RateControl::Bitrate { kbps: 800, keyframe_s: 2 });
    let mut n = Native::start(&j, &path).unwrap();
    let sent = feed(&n.video(), &n.audio(), 5.0);
    n.stop().unwrap();
    let file = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
    let (tracks, [whole, _]) = packets(&file);
    assert_eq!(whole.len() as u64, sent);
    let keyframes: Vec<i64> = whole.iter().filter(|p| p.1).map(|p| p.0).collect();
    assert_eq!(keyframes, [0, 2_000_000_000, 4_000_000_000], "a cluster, and so what a cut loses, spans at most 2 s");
    for cut in [file.len() / 2, file.len() * 9 / 10, file.len() - 1] {
        let (_, [video, audio]) = packets(&file[..cut]);
        assert!(video.len() >= 10 && !audio.is_empty() && video[0].1, "cut at {cut}: {} frames", video.len());
        let lumas = decode(&tracks[0].codec_private, &video);
        assert_eq!(lumas.len(), video.len(), "cut at {cut}");
    }
}

/// A video encoder whose sessions wait in `encode` until the test lets them go.
struct Stuck(Mutex<Option<mpsc::Receiver<()>>>);

impl Plugin for Stuck {
    fn id(&self) -> &'static str {
        "stuck"
    }
    fn name(&self) -> &'static str {
        "Stuck"
    }
    fn schema(&self) -> SettingsSchema {
        SettingsSchema::default()
    }
}

struct StuckSession(mpsc::Receiver<()>);

impl VideoSession for StuckSession {
    fn codec_private(&self) -> Vec<u8> {
        Vec::new()
    }
    fn encode(&mut self, _: &[u8], _: i64, _: &mut Vec<Packet>) -> Result<(), PluginError> {
        let _ = self.0.recv();
        Ok(())
    }
    fn flush(&mut self, _: &mut Vec<Packet>) -> Result<(), PluginError> {
        Ok(())
    }
}

impl Encoder for Stuck {
    fn codec(&self) -> Codec {
        Codec::H264
    }
    fn open(&self, _: &Settings, _: &VideoParams) -> Result<Box<dyn VideoSession>, PluginError> {
        let rx = self.0.lock().unwrap().take().unwrap();
        Ok(Box::new(StuckSession(rx)))
    }
}

#[test]
fn a_recording_not_finished_in_time_fails_and_leaves_the_file() {
    let path = temp_file("stuck");
    let (tx, rx) = mpsc::channel();
    let mut j = job(RateControl::Quality(20));
    j.video_encoder = Arc::new(Stuck(Mutex::new(Some(rx))));
    let mut n = Native::start(&j, &path).unwrap();
    feed(&n.video(), &n.audio(), 0.2);
    let Err(e) = n.end(Duration::from_millis(50)) else { panic!("a stuck recording finished") };
    assert!(e.to_string().contains("not finished in time"), "{e}");
    assert!(matches!(n.status(), OutputStatus::Failed(_)));
    assert!(n.stop().is_err(), "stopping again reports the same failure");
    drop(tx);
    let file = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
    assert!(Demuxer::from_slice(&file).is_ok(), "the header was written");
}

/// A video encoder that loses its last frames when it is flushed.
struct LosesLastFrames;

impl Plugin for LosesLastFrames {
    fn id(&self) -> &'static str {
        "loses_last_frames"
    }
    fn name(&self) -> &'static str {
        "Loses Last Frames"
    }
    fn schema(&self) -> SettingsSchema {
        SettingsSchema::default()
    }
}

impl VideoSession for LosesLastFrames {
    fn codec_private(&self) -> Vec<u8> {
        Vec::new()
    }
    fn encode(&mut self, _: &[u8], _: i64, _: &mut Vec<Packet>) -> Result<(), PluginError> {
        Ok(())
    }
    fn flush(&mut self, _: &mut Vec<Packet>) -> Result<(), PluginError> {
        Err(PluginError::Failed("the encoder lost its last frames".into()))
    }
}

impl Encoder for LosesLastFrames {
    fn codec(&self) -> Codec {
        Codec::H264
    }
    fn open(&self, _: &Settings, _: &VideoParams) -> Result<Box<dyn VideoSession>, PluginError> {
        Ok(Box::new(LosesLastFrames))
    }
}

#[test]
fn a_flush_that_fails_fails_the_recording() {
    let path = temp_file("flush");
    let mut j = job(RateControl::Quality(20));
    j.video_encoder = Arc::new(LosesLastFrames);
    let mut n = Native::start(&j, &path).unwrap();
    feed(&n.video(), &n.audio(), 0.2);
    let result = n.end(Duration::from_secs(30));
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
    let Err(e) = result else { panic!("a recording that lost its last frames finished") };
    assert!(e.to_string().contains("lost its last frames"), "{e}");
    assert!(matches!(n.status(), OutputStatus::Failed(_)));
}

fn item(track: Track, order_ns: i64, bytes: usize) -> Item {
    Item { track, order_ns, pts_ns: order_ns, dts_ns: order_ns, keyframe: true, data: vec![0; bytes] }
}

#[test]
fn the_packet_queue_is_bounded_by_bytes_and_by_media_time() {
    let q = PacketQueue::new(1000, 3_000_000_000);
    assert!(q.try_push(item(Track::Video, 0, 5000)).is_ok(), "an empty queue takes any packet");
    assert!(q.try_push(item(Track::Video, 1, 1)).is_err(), "past the byte limit");
    assert!(q.pop().is_some() && q.is_empty());
    assert!(q.try_push(item(Track::Video, 1, 1)).is_err(), "a packet taken and not yet written still counts");
    q.written(5000);
    assert!(q.try_push(item(Track::Audio, 10, 400)).is_ok());
    assert!(q.try_push(item(Track::Audio, 20, 600)).is_ok());
    assert!(q.try_push(item(Track::Audio, 30, 1)).is_err());
    let q = PacketQueue::new(1 << 20, 3_000_000_000);
    for t in [0, 1_000_000_000, 3_000_000_000] {
        assert!(q.try_push(item(Track::Audio, t, 1)).is_ok());
    }
    assert!(q.try_push(item(Track::Audio, 3_000_000_001, 1)).is_err(), "past 3 s of media");
    assert!(q.pop().is_some());
    assert!(q.try_push(item(Track::Audio, 3_000_000_001, 1)).is_err(), "the span counts from the last packet taken");
    assert!(q.pop().is_some());
    assert!(q.try_push(item(Track::Audio, 3_000_000_001, 1)).is_ok(), "the span counts from the last packet taken");
    // Sound ahead of the pictures: a late video packet taken after newer sound does not move
    // the span's start back.
    let q = PacketQueue::new(1 << 20, 3_000_000_000);
    assert!(q.try_push(item(Track::Audio, 5_000_000_000, 1)).is_ok());
    assert!(q.try_push(item(Track::Video, 2_000_000_000, 1)).is_ok());
    assert!(q.pop().is_some() && q.pop().is_some());
    assert!(q.try_push(item(Track::Audio, 5_500_000_000, 1)).is_ok(), "the span counts from the newest packet taken");
}

#[test]
fn a_full_queue_that_stays_full_fails_the_recording() {
    let shared = Shared::new(PacketQueue::new(10, QUEUE_SPAN_NS), Duration::ZERO);
    assert!(shared.send(item(Track::Video, 0, 10)));
    assert!(!shared.send(item(Track::Video, 1, 10)));
    assert_eq!(shared.failure().as_deref(), Some("the disk or an encoder could not keep up with the recording"));
    assert!(shared.halted(), "the encoders see the failure");
    let shared = Shared::new(PacketQueue::new(10, QUEUE_SPAN_NS), Duration::from_secs(60));
    assert!(shared.send(item(Track::Video, 0, 10)));
    shared.abandon.store(true, Ordering::SeqCst);
    assert!(!shared.send(item(Track::Video, 1, 10)), "an abandoned recording does not wait");
    assert_eq!(shared.failure(), None);
    assert!(shared.halted());
}

#[test]
fn the_tracks_are_written_in_time_order() {
    let mut pending: [VecDeque<Item>; 2] = [VecDeque::new(), VecDeque::new()];
    pending[0].push_back(item(Track::Video, 2_000, 1));
    pending[1].push_back(item(Track::Audio, 1_000, 1));
    pending[1].push_back(item(Track::Audio, 3_000, 1));
    let running = [false, false];
    let order: Vec<i64> = std::iter::from_fn(|| next_item(&mut pending, running).unwrap()).map(|i| i.order_ns).collect();
    assert_eq!(order, [1_000, 2_000], "the last audio waits for video");
    assert!(next_item(&mut pending, [false, true]).unwrap().is_none(), "the audio waits while only the audio encoder has ended");
    assert!(next_item(&mut pending, [true, false]).unwrap().is_some(), "once the video encoder has ended nothing waits for it");
    // Sound well ahead of the pictures still waits for them, so the file stays in time order.
    pending[1].push_back(item(Track::Audio, 0, 1));
    pending[1].push_back(item(Track::Audio, 5_000_000_000, 1));
    assert!(next_item(&mut pending, running).unwrap().is_none());
    pending[0].push_back(item(Track::Video, 100, 1));
    let order: Vec<i64> = std::iter::from_fn(|| next_item(&mut pending, running).unwrap()).map(|i| i.order_ns).collect();
    assert_eq!(order, [0, 100], "the late video goes before the later sound");
    pending[1].push_back(item(Track::Audio, 5_000_000_000 + MAX_LAG_NS + 1, 1));
    assert_eq!(next_item(&mut pending, running).err(), Some(Track::Video), "a video encoder that falls too far behind is named");
}

#[test]
fn only_mkv_files_and_48_khz_are_taken() {
    let path = temp_file("formats");
    let mp4 = path.with_extension("mp4");
    let Err(e) = Recording.start(&Settings::new().with("path", mp4.to_string_lossy().as_ref()), job(RateControl::Quality(20))) else {
        panic!("an MP4 recording started")
    };
    assert!(e.to_string().contains("records MKV files only"), "{e}");
    let mut j = job(RateControl::Quality(20));
    j.sample_rate = 44_100;
    assert!(Native::start(&j, &path).is_err());
    let Err(e) = Native::start(&job(RateControl::Lossless), &path) else { panic!("a lossless recording started") };
    assert!(e.to_string().contains("losslessly"), "{e}");
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn the_video_track_names_the_encoder_codec() {
    let aac = AacSession::new(128).unwrap();
    let h264 = tracks(&job(RateControl::Quality(20)), vec![1], &aac);
    assert_eq!((h264[0].codec_id.as_str(), h264[0].codec_private.as_slice()), ("V_MPEG4/ISO/AVC", &[1u8][..]));
    let mut j = job(RateControl::Quality(20));
    j.video_encoder = Arc::new(studiocraft_encode::Nvenc::HEVC);
    let hevc = tracks(&j, vec![2], &aac);
    assert_eq!((hevc[0].codec_id.as_str(), hevc[0].codec_private.as_slice()), ("V_MPEGH/ISO/HEVC", &[2u8][..]));
    for av1 in [Arc::new(studiocraft_encode::Nvenc::AV1) as Arc<dyn studiocraft_plugin::Encoder>, Arc::new(studiocraft_encode::MediaFoundation::AV1)]
    {
        j.video_encoder = av1;
        let t = tracks(&j, vec![0x81, 3], &aac);
        assert_eq!((t[0].codec_id.as_str(), t[0].codec_private.as_slice()), ("V_AV1", &[0x81u8, 3][..]));
    }
}

#[test]
fn video_order_never_goes_back_after_a_keyframe_with_b_frames() {
    // Decoding order around a keyframe with two B-frames: (pts, dts, keyframe).
    let coded = [(57, 55, false), (58, 56, false), (60, 57, true), (63, 58, false), (61, 59, false), (62, 60, false), (66, 61, false)];
    let mut last = i64::MIN;
    let order: Vec<i64> =
        coded.iter().map(|&(pts, dts, keyframe)| video_order(&Packet { data: Vec::new(), pts, dts, keyframe }, &mut last)).collect();
    assert_eq!(order, [55, 56, 60, 60, 60, 60, 61], "the keyframe at its presentation time, the frames after it no earlier");
}

#[test]
fn time_conversions_are_exact() {
    assert_eq!(frames_ns(3, 30_000, 1001), 100_100_000);
    assert_eq!(frames_ns(i64::from(u32::MAX), 60, 1), 71_582_788_250_000_000);
    assert_eq!(samples_ns(48_000), 1_000_000_000);
    assert_eq!(samples_ns(-1024), -21_333_333);
}
