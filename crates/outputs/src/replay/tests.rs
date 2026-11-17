use std::io::Cursor;

use studiocraft_model::Settings;
use studiocraft_plugin::{Output, OutputStatus, RateControl};

use super::*;
use crate::native::tests::{Feeder, decode, job, packets, temp_file};

const S: i64 = 1_000_000_000;

/// A packet of `bytes` bytes at `ms` milliseconds.
fn held(track: Track, ms: i64, keyframe: bool, bytes: usize) -> Held {
    Held { track, order_ns: ms * 1_000_000, pts_ns: ms * 1_000_000, keyframe, data: Arc::new(vec![0; bytes]) }
}

/// `seconds` of a stream in file order: a picture every 100 ms with a keyframe every 2 s, and a
/// sound packet every 20 ms.
fn stream(seconds: i64, video_bytes: usize, audio_bytes: usize) -> Vec<Held> {
    let mut out = Vec::new();
    for ms in (0..seconds * 1000).step_by(20) {
        if ms % 100 == 0 {
            out.push(held(Track::Video, ms, ms % 2000 == 0, video_bytes));
        }
        out.push(held(Track::Audio, ms, true, audio_bytes));
    }
    out
}

#[test]
fn the_ring_keeps_the_newest_seconds_from_a_keyframe() {
    let mut ring = Ring::new(3 * S, usize::MAX);
    for h in stream(8, 1000, 100) {
        ring.push(h);
        if let Some(clip) = ring.snapshot() {
            let first = clip.items.iter().position(|h| h.track == Track::Video).unwrap();
            assert!(clip.items[first].keyframe, "a clip starts on a keyframe");
            assert!(first <= 1, "at most the one sound packet before it");
        }
    }
    let clip = ring.snapshot().unwrap();
    assert_eq!(clip.base_ns, 4 * S, "the groups from 4 s span 3 s or more, those from 6 s do not");
    assert_eq!((clip.items[0].track, clip.items[0].pts_ns), (Track::Audio, 3_980_000_000), "the sound packet before the keyframe");
    assert_eq!(clip.items.last().map(|h| h.order_ns), Some(7_980_000_000));
    assert_eq!(ring.held_ns(), Some(3_980_000_000));
    let keyframes = clip.items.iter().filter(|h| h.starts_group()).count();
    assert_eq!(keyframes, 2);
}

#[test]
fn the_ring_stays_within_its_memory_limit() {
    let mut ring = Ring::new(60 * S, 30_000);
    for h in stream(10, 1000, 50) {
        ring.push(h);
        assert!(ring.bytes() <= 30_000, "{} bytes held", ring.bytes());
    }
    let clip = ring.snapshot().unwrap();
    assert_eq!(clip.base_ns, 8 * S, "one group of 2 s (about 25 kB) fits, two do not");
    assert_eq!(ring.bytes(), clip.items.iter().map(|h| h.data.len()).sum::<usize>());

    // A group larger than the limit is not kept: the ring holds nothing until the next keyframe.
    let mut ring = Ring::new(60 * S, 5_000);
    let mut at_two = None;
    for h in stream(3, 1000, 100) {
        let (ms, keyframe) = (h.pts_ns / 1_000_000, h.starts_group());
        ring.push(h);
        assert!(ring.bytes() <= 5_000);
        if (300..2000).contains(&ms) {
            assert!(ring.snapshot().is_none(), "at {ms} ms the first group no longer fits");
        }
        if keyframe && ms == 2000 {
            at_two = ring.snapshot().map(|c| c.base_ns);
        }
    }
    assert_eq!(at_two, Some(2 * S), "the next keyframe starts a clip again");
}

#[test]
fn the_ring_holds_nothing_before_the_first_keyframe() {
    let mut ring = Ring::new(3 * S, usize::MAX);
    for ms in [0, 20, 40] {
        ring.push(held(Track::Audio, ms, true, 10));
        ring.push(held(Track::Video, ms, false, 10));
    }
    assert!(ring.snapshot().is_none());
    assert_eq!(ring.held_ns(), None);
    assert_eq!(ring.bytes(), 10, "only the newest sound packet is kept");
    ring.push(held(Track::Video, 60, true, 10));
    let clip = ring.snapshot().unwrap();
    assert_eq!(clip.items.iter().map(|h| h.pts_ns).collect::<Vec<_>>(), [40_000_000, 60_000_000]);
    assert_eq!(clip.base_ns, 60_000_000);
}

/// Wait until the ring thread of `r` holds `ns` nanoseconds from a keyframe.
fn wait_held(r: &ReplayBuffer, ns: i64) {
    while r.requests.held_ns.load(Ordering::Acquire) < ns {
        assert!(!matches!(r.status(), OutputStatus::Failed(_)), "{:?}", r.status());
        std::thread::yield_now();
    }
}

/// Save a replay of `r` to `path`; the file's video packets and the luma of each decoded picture.
fn save(r: &ReplayBuffer, path: &Path) -> (Vec<(i64, bool, Vec<u8>)>, Vec<u8>) {
    r.replay().unwrap().write(path).unwrap();
    let file = std::fs::read(path).unwrap();
    let (tracks, [video, audio]) = packets(&file);
    assert!(!audio.is_empty(), "the replay has its sound");
    assert!(audio.windows(2).all(|w| w[1].0 > w[0].0));
    let lumas = decode(&tracks[0].codec_private, &video);
    assert_eq!(lumas.len(), video.len(), "every picture decodes");
    (video, lumas)
}

/// The frame number of each luma from [`Feeder`], checking that they follow on one another.
fn frames(lumas: &[u8]) -> Vec<u64> {
    let n: Vec<u64> = lumas.iter().map(|l| u64::from(l.saturating_sub(16))).collect();
    assert!(n.windows(2).all(|w| w[1].abs_diff(w[0] + 1) <= 2), "{lumas:?}");
    n
}

#[test]
fn the_replay_buffer_saves_its_last_seconds_and_keeps_running() {
    let dir = temp_file("replay").parent().unwrap().to_path_buf();
    let j = job(RateControl::Quality(20));
    let stats = Arc::clone(&j.stats);
    let mut r = ReplayBuffer::start(&j, 3, 64 << 20).unwrap();
    let Err(e) = r.replay() else { panic!("an empty replay buffer saved") };
    assert_eq!(e.to_string(), "the replay buffer holds nothing to save yet");

    // While it is still filling, a save holds what there is, from the first frame.
    let mut feeder = Feeder::default();
    feeder.feed(&r.video(), &r.audio(), 1.0);
    wait_held(&r, S / 2);
    let (video, lumas) = save(&r, &dir.join("filling.mkv"));
    assert!(video[0].1 && video[0].0 == 0, "it starts on a keyframe at 0");
    assert!(frames(&lumas)[0] <= 2, "the first frame (lumas are coded to within 2): {lumas:?}");

    // Two saves in a row, each a file of its own that starts on a keyframe and spans at least 3 s.
    let sent = feeder.feed(&r.video(), &r.audio(), 7.0);
    wait_held(&r, 3 * S);
    for name in ["first.mkv", "second.mkv"] {
        let (video, lumas) = save(&r, &dir.join(name));
        assert!(video[0].1 && video[0].0 == 0, "{name}: times count from the first keyframe");
        let shown = frames(&lumas);
        assert!((shown[0] + 2) % 20 <= 4, "{name}: the first picture is a keyframe of the stream (one every 2 s): {lumas:?}");
        assert!(shown.len() >= 25, "{name}: {} pictures", shown.len());
        assert!(*shown.last().unwrap() < sent);
    }
    assert!(matches!(r.status(), OutputStatus::Running), "saving does not stop the buffer");
    assert!(stats.bytes.load(Ordering::Relaxed) > 0, "the bytes held are reported");

    // A file that cannot be written is an error, and the buffer keeps running.
    let Err(e) = r.replay().unwrap().write(&dir) else { panic!("a replay was written over a folder") };
    assert!(e.to_string().starts_with("the replay could not be written to"), "{e}");
    assert!(matches!(r.status(), OutputStatus::Running));
    save(&r, &dir.join("after-error.mkv"));

    // A save asked for before the buffer stops is still written.
    let pending = r.replay().unwrap();
    r.stop().unwrap();
    pending.write(&dir.join("stopped.mkv")).unwrap();
    assert!(!std::fs::read(dir.join("stopped.mkv")).unwrap().is_empty());
    let Err(e) = r.replay() else { panic!("a stopped replay buffer saved") };
    assert_eq!(e.to_string(), "the replay buffer has stopped");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A writer that fails once it has taken `room` bytes.
struct Full {
    inner: Cursor<Vec<u8>>,
    room: usize,
}

impl Write for Full {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.inner.get_ref().len() + buf.len() > self.room {
            return Err(std::io::Error::other("no space left"));
        }
        self.inner.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Seek for Full {
    fn seek(&mut self, to: std::io::SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(to)
    }
}

#[test]
fn a_full_disk_fails_the_save() {
    let mut ring = Ring::new(60 * S, usize::MAX);
    for h in stream(3, 1000, 100) {
        ring.push(h);
    }
    let clip = ring.snapshot().unwrap();
    let aac = studiocraft_encode::AacSession::new(128).unwrap();
    let tracks = || crate::native::tracks(&job(RateControl::Quality(20)), Vec::new(), &aac);
    let Err(e) = mux_clip(Full { inner: Cursor::new(Vec::new()), room: 10_000 }, tracks(), &clip) else { panic!("a full disk took the replay") };
    assert!(e.contains("no space left"), "{e}");
    let whole = mux_clip(Full { inner: Cursor::new(Vec::new()), room: usize::MAX }, tracks(), &clip).unwrap();
    assert!(whole.inner.get_ref().len() > 30_000);
}

#[test]
fn the_output_checks_its_encoder_and_settings() {
    let j = job(RateControl::Quality(20));
    for (key, bad) in [("seconds", 1), ("seconds", 3601), ("memory_mb", 16), ("memory_mb", 100_000)] {
        let Err(e) = Replay.start(&Settings::new().with(key, bad), j.clone()) else { panic!("{key} {bad} was taken") };
        assert!(e.to_string().contains(&format!("`{key}` must be a whole number")), "{e}");
    }
    let Err(e) = Replay.start(&Settings::new().with("seconds", "20"), j.clone()) else { panic!("text was taken") };
    assert!(e.to_string().contains("`seconds`"), "{e}");
    let mut ffmpeg = j.clone();
    ffmpeg.video_encoder = Arc::new(crate::encoders::all().into_iter().next().unwrap());
    let Err(e) = Replay.start(&Settings::new(), ffmpeg) else { panic!("an ffmpeg encoder ran the replay buffer") };
    assert!(e.to_string().starts_with("the replay buffer runs on the built-in H.264 encoder"), "{e}");
    let mut out = Replay.start(&Settings::new().with("seconds", 5).with("memory_mb", 32), j).unwrap();
    out.stop().unwrap();
}
