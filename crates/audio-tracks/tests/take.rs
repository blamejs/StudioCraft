//! Takes written to disk: what the files hold, how they split, how they survive a crash and a
//! full disk.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crossbeam_queue::ArrayQueue;
use studiocraft_audio_tracks::collect::FRAME_TICKS;
use studiocraft_audio_tracks::collect::{BLOCK_TICKS, CHUNK_BLOCKS, ToDisk};
use studiocraft_audio_tracks::disk::{Failure, Writer};
use studiocraft_audio_tracks::journal::{self, Line};
use studiocraft_audio_tracks::{Collector, DiskConfig, Faults, State, StopReason, Take, TakeConfig, Timing, TrackSpec, recover, wav};
use studiocraft_media::Tick;
use studiocraft_media::audio::{BLOCK_FRAMES, BLOCK_SAMPLES};
use studiocraft_media::tap::TapSlot;
use studiocraft_model::SourceId;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("studiocraft-audio-tracks-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir.join("Take Audio").join("Audio Files")
}

fn config(dir: &Path) -> DiskConfig {
    DiskConfig {
        dir: dir.to_path_buf(),
        folder_stem: None,
        timing: Timing::default(),
        segment_bytes: 2 << 30,
        reserve_bytes: 0,
        warn_bytes: 0,
        faults: Faults::default(),
    }
}

fn spec(source: u64, name: &str) -> TrackSpec {
    TrackSpec { source: Some(SourceId(source)), name: name.into(), slot: Arc::new(TapSlot::new()) }
}

/// The samples of block `n` of a track: a ramp no other block repeats.
fn samples(track: u64, n: i64) -> Vec<f32> {
    (0..BLOCK_SAMPLES).map(|i| (track as f32) + (n as f32 * BLOCK_SAMPLES as f32 + i as f32) / 1e7).collect()
}

fn feed(slot: &TapSlot, track: u64, n: i64) {
    if let Some(mut b) = slot.claim() {
        b.samples.copy_from_slice(&samples(track, n));
        b.pts = Tick(500 + n * BLOCK_TICKS);
        slot.push(b);
    }
}

/// A collector and a writer driven on the test's thread.
struct Rig {
    c: Collector,
    w: Writer,
    out: Arc<ArrayQueue<ToDisk>>,
}

impl Rig {
    fn new(cfg: DiskConfig, chunks: usize) -> Rig {
        let (c, free, out) = Collector::new(chunks, chunks + 64).unwrap();
        let w = Writer::create(cfg, free, c.dropout_queue()).unwrap();
        Rig { c, w, out }
    }

    fn write(&mut self) -> Result<bool, Failure> {
        while let Some(m) = self.out.pop() {
            if self.w.handle(m)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn pump(&mut self) -> Result<bool, Failure> {
        self.c.poll();
        self.write()
    }

    fn end(&mut self) -> Result<(), Failure> {
        loop {
            let sent = self.c.finish();
            if self.write()? || sent && self.out.is_empty() {
                return Ok(());
            }
        }
    }
}

/// Interleaved samples of the WAV at `path`, read as the audio editor reads them.
fn read(path: &Path) -> Vec<f32> {
    let bytes = std::fs::read(path).unwrap();
    let (info, buf) = craft_audio::decode(&bytes, Some("wav")).unwrap();
    assert_eq!((info.sample_rate, buf.channels.len()), (48_000, 2), "{}", path.display());
    buf.interleaved()
}

#[test]
fn every_track_holds_its_audio_at_its_place_in_the_take_with_silence_for_gaps() {
    let dir = scratch("places");
    let mut rig = Rig::new(config(&dir), 8);
    let (a, b) = (spec(1, "Mic"), spec(2, "Game: \"Left\""));
    rig.c.set_tracks(vec![a.clone(), b.clone()]);
    feed(&a.slot, 1, 0);
    feed(&b.slot, 2, 2);
    rig.pump().unwrap();
    // The mic's tap lost blocks 2 and 3.
    for n in [1, 4] {
        feed(&a.slot, 1, n);
    }
    feed(&b.slot, 2, 3);
    rig.pump().unwrap();
    rig.end().unwrap();
    let silence = vec![0.0; BLOCK_SAMPLES];
    let want_a = [samples(1, 0), samples(1, 1), silence.clone(), silence.clone(), samples(1, 4)].concat();
    assert_eq!(read(&dir.join("Mic.wav")), want_a);
    // B starts at the take's start and, still recorded, ends with the take.
    let want_b = [silence.clone(), silence.clone(), samples(2, 2), samples(2, 3), silence].concat();
    assert_eq!(read(&dir.join("Game Left.wav")), want_b, "the name lost its path characters; the track starts at the take's start");
    let lines = journal::read(&dir).unwrap();
    assert!(matches!(lines.last(), Some(Line::End { frames }) if *frames == 5 * BLOCK_FRAMES as u64));
}

#[test]
fn a_take_that_ends_before_any_audio_still_has_a_file_per_track() {
    let dir = scratch("no-audio");
    let mut rig = Rig::new(config(&dir), 4);
    rig.c.set_tracks(vec![spec(1, "Mic"), spec(2, "Desk")]);
    rig.pump().unwrap();
    rig.end().unwrap();
    for name in ["Mic.wav", "Desk.wav"] {
        assert!(read(&dir.join(name)).is_empty(), "{name} is an empty WAV");
    }
    let lines = journal::read(&dir).unwrap();
    assert!(matches!(lines.last(), Some(Line::End { frames: 0 })), "{lines:?}");
    let _ = std::fs::remove_dir_all(dir.parent().and_then(Path::parent).unwrap());
}

#[test]
fn a_repaired_take_split_into_parts_ends_after_its_last_part() {
    let dir = scratch("segments-crash");
    let head = wav::header_len().unwrap();
    let cfg = DiskConfig { segment_bytes: head + 3_000 * 8, ..config(&dir) };
    let mut rig = Rig::new(cfg, 4);
    let a = spec(1, "Mic");
    rig.c.set_tracks(vec![a.clone()]);
    for n in 0..10 {
        feed(&a.slot, 1, n);
        rig.pump().unwrap();
    }
    rig.c.flush();
    rig.write().unwrap();
    // The app stops here, without ending the take.
    drop(rig);
    let recording = dir.parent().and_then(Path::parent).unwrap();
    let done = recover::repair_all(recording).unwrap();
    assert_eq!(done.len(), 1);
    let lines = journal::read(&dir).unwrap();
    assert!(matches!(lines.last(), Some(Line::End { frames }) if *frames == 10 * BLOCK_FRAMES as u64), "{lines:?}");
}

#[test]
fn a_take_split_into_parts_holds_the_same_audio_end_to_end() {
    let dir = scratch("segments");
    let head = wav::header_len().unwrap();
    // Parts of 3,000 frames: a block of 1,024 crosses a part's end.
    let cfg = DiskConfig { segment_bytes: head + 3_000 * 8, ..config(&dir) };
    let mut rig = Rig::new(cfg, 4);
    let a = spec(1, "Mic");
    rig.c.set_tracks(vec![a.clone()]);
    for n in 0..10 {
        feed(&a.slot, 1, n);
        rig.pump().unwrap();
    }
    rig.end().unwrap();
    let parts: Vec<String> = journal::read(&dir)
        .unwrap()
        .into_iter()
        .filter_map(|l| match l {
            Line::File { file, segment, first_frame, .. } => Some(format!("{file} {segment} {first_frame}")),
            _ => None,
        })
        .collect();
    assert_eq!(parts, ["Mic.wav 1 0", "Mic part 2.wav 2 3000", "Mic part 3.wav 3 6000", "Mic part 4.wav 4 9000"]);
    let mut joined = Vec::new();
    for p in ["Mic.wav", "Mic part 2.wav", "Mic part 3.wav", "Mic part 4.wav"] {
        joined.extend(read(&dir.join(p)));
    }
    let want: Vec<f32> = (0..10).flat_map(|n| samples(1, n)).collect();
    assert_eq!(joined, want);
}

#[test]
fn with_the_disk_too_slow_the_lost_audio_is_silence_of_exactly_its_length() {
    let dir = scratch("slow");
    let mut rig = Rig::new(config(&dir), 2);
    let a = spec(1, "Mic");
    rig.c.set_tracks(vec![a.clone()]);
    // The disk takes nothing while 300 blocks arrive: two chunks hold 256 of them.
    for n in 0..300 {
        feed(&a.slot, 1, n);
        rig.c.poll();
    }
    let f = BLOCK_FRAMES as u64;
    assert_eq!(rig.c.dropped_frames(), 44 * f);
    rig.write().unwrap();
    for n in 300..310 {
        feed(&a.slot, 1, n);
        rig.pump().unwrap();
    }
    rig.end().unwrap();
    let got = read(&dir.join("Mic.wav"));
    assert_eq!(got.len(), 310 * BLOCK_SAMPLES);
    let block = |n: usize| &got[n * BLOCK_SAMPLES..(n + 1) * BLOCK_SAMPLES];
    assert_eq!(block(255), samples(1, 255).as_slice());
    assert!((256..300).all(|n| block(n).iter().all(|s| *s == 0.0)), "the dropped blocks are silence");
    assert_eq!(block(300), samples(1, 300).as_slice(), "the audio after the dropout is at its own time");
    let drops: Vec<Line> = journal::read(&dir).unwrap().into_iter().filter(|l| matches!(l, Line::Dropout { .. })).collect();
    assert_eq!(drops, [Line::Dropout { track: 0, at: 256 * f, frames: 44 * f }]);
    assert_eq!(CHUNK_BLOCKS, 128);
}

#[test]
fn a_source_removed_and_added_back_during_a_take_continues_its_file() {
    let dir = scratch("readd");
    let mut rig = Rig::new(config(&dir), 8);
    let (a, clock) = (spec(1, "Mic"), spec(9, "Desk"));
    rig.c.set_tracks(vec![a.clone(), clock.clone()]);
    feed(&a.slot, 1, 0);
    feed(&clock.slot, 9, 0);
    rig.pump().unwrap();
    rig.c.set_tracks(vec![clock.clone()]);
    feed(&clock.slot, 9, 1);
    rig.pump().unwrap();
    let back = spec(1, "Mic renamed");
    rig.c.set_tracks(vec![clock.clone(), back.clone()]);
    feed(&clock.slot, 9, 2);
    feed(&back.slot, 1, 2);
    rig.pump().unwrap();
    rig.end().unwrap();
    let want = [samples(1, 0), vec![0.0; BLOCK_SAMPLES], samples(1, 2)].concat();
    assert_eq!(read(&dir.join("Mic.wav")), want);
    assert!(!dir.join("Mic renamed.wav").exists(), "the track keeps its name from the start of the take");
}

#[test]
fn a_full_disk_stops_the_take_with_its_reason_and_leaves_readable_files() {
    let dir = scratch("full");
    let cfg = DiskConfig { faults: Faults { full_after_bytes: Some(3 * 8192), ..Faults::default() }, ..config(&dir) };
    let a = spec(1, "Mic");
    let take = Take::start(TakeConfig { disk: cfg, buffer_mib: 4 }, vec![a.clone()]).unwrap();
    for n in 0..10 {
        feed(&a.slot, 1, n);
    }
    let status = take.join();
    assert_eq!((status.state(), status.reason()), (Some(State::Stopped), Some(StopReason::DiskFull)));
    let lines = journal::read(&dir).unwrap();
    assert!(lines.contains(&Line::Stop { reason: "the disk is full".into() }));
    assert_eq!(read(&dir.join("Mic.wav")).len(), 3 * BLOCK_SAMPLES, "the header matches what was written");
    // The files were finished after the failure, so the take is not left for repair.
    assert!(matches!(lines.last(), Some(Line::End { .. })), "{lines:?}");
    assert!(recover::unfinished(dir.parent().unwrap().parent().unwrap()).is_empty());
}

#[test]
fn low_free_space_stops_the_take_before_it_writes() {
    let dir = scratch("reserve");
    let cfg =
        DiskConfig { reserve_bytes: 1 << 30, warn_bytes: 2 << 30, faults: Faults { free_space: Some(1 << 29), ..Faults::default() }, ..config(&dir) };
    let take = Take::start(TakeConfig { disk: cfg, buffer_mib: 4 }, vec![spec(1, "Mic")]).unwrap();
    let status = take.join();
    assert_eq!((status.state(), status.reason()), (Some(State::Stopped), Some(StopReason::LowSpace)));
    assert!(!dir.exists(), "a take that could not start leaves no folder or journal");
}

#[test]
fn a_take_on_its_threads_writes_every_block_it_was_given() {
    let dir = scratch("threads");
    let (a, p) = (spec(1, "Mic"), TrackSpec { source: None, name: "00 Program".into(), slot: Arc::new(TapSlot::new()) });
    let take = Take::start(TakeConfig { disk: config(&dir), buffer_mib: 4 }, vec![p.clone(), a.clone()]).unwrap();
    for n in 0..20 {
        feed(&p.slot, 0, n);
        feed(&a.slot, 1, n);
    }
    let status = take.join();
    assert_eq!(status.state(), Some(State::Done));
    assert_eq!(status.dropped_frames(), 0);
    assert_eq!(read(&dir.join("00 Program.wav")), (0..20).flat_map(|n| samples(0, n)).collect::<Vec<f32>>());
    assert_eq!(read(&dir.join("Mic.wav")).len(), 20 * BLOCK_SAMPLES);
}

#[test]
fn a_source_that_stops_recording_keeps_its_blocks_until_the_mixer_drops_its_tap() {
    use std::sync::atomic::{AtomicU64, Ordering};
    let dir = scratch("leave-after");
    let (p, a) = (TrackSpec { source: None, name: "00 Program".into(), slot: Arc::new(TapSlot::new()) }, spec(1, "Mic"));
    let take = Take::start(TakeConfig { disk: config(&dir), buffer_mib: 4 }, vec![p.clone(), a.clone()]).unwrap();
    for n in 0..3 {
        feed(&p.slot, 0, n);
        feed(&a.slot, 1, n);
    }
    // The studio stops recording Mic and asks the mixer to drop its tap; the mixer has not yet.
    let applied = Arc::new(AtomicU64::new(0));
    assert!(take.set_tracks(vec![p.clone()], Tick(500 + 3 * BLOCK_TICKS), Some((Arc::clone(&applied), 1))));
    // Give the take every chance to apply the list early: it must not, while the tap is in use.
    for _ in 0..200_000 {
        std::thread::yield_now();
    }
    for n in 3..6 {
        feed(&p.slot, 0, n);
        feed(&a.slot, 1, n);
    }
    applied.store(1, Ordering::Release);
    let status = take.join();
    assert_eq!(status.state(), Some(State::Done));
    assert_eq!(read(&dir.join("Mic.wav")), (0..6).flat_map(|n| samples(1, n)).collect::<Vec<f32>>(), "Mic keeps every block the mixer stored");
    let _ = std::fs::remove_dir_all(dir.parent().and_then(Path::parent).unwrap());
}

#[test]
fn a_source_left_without_a_file_at_the_track_limit_shows_in_the_status() {
    let dir = scratch("track-limit");
    let specs: Vec<TrackSpec> = (0..=studiocraft_audio_tracks::MAX_TAKE_TRACKS as u64).map(|n| spec(n + 1, &format!("Source {n}"))).collect();
    let take = Take::start(TakeConfig { disk: config(&dir), buffer_mib: 4 }, specs).unwrap();
    let status = take.join();
    assert_eq!(status.tracks(), studiocraft_audio_tracks::MAX_TAKE_TRACKS as u64);
    assert!(status.track_limit(), "the source past the limit is reported");
    let _ = std::fs::remove_dir_all(dir.parent().and_then(Path::parent).unwrap());
}

#[test]
fn a_take_stopped_after_the_taps_go_keeps_the_blocks_stored_until_then() {
    use std::sync::atomic::{AtomicU64, Ordering};
    let dir = scratch("stop-after");
    let p = TrackSpec { source: None, name: "00 Program".into(), slot: Arc::new(TapSlot::new()) };
    let take = Take::start(TakeConfig { disk: config(&dir), buffer_mib: 4 }, vec![p.clone()]).unwrap();
    for n in 0..5 {
        feed(&p.slot, 0, n);
    }
    // The studio asked the mixer to remove the taps; the mixer has not applied it yet.
    let applied = Arc::new(AtomicU64::new(0));
    take.stop_after(Arc::clone(&applied), 1, None);
    // Give the take every chance to end early: it must not, while the mixer still holds the taps.
    for _ in 0..200_000 {
        if take.finished() {
            break;
        }
        std::thread::yield_now();
    }
    assert!(!take.finished(), "the take ended while the mixer could still store blocks");
    for n in 5..8 {
        feed(&p.slot, 0, n);
    }
    applied.store(1, Ordering::Release);
    let status = take.join();
    assert_eq!(status.state(), Some(State::Done));
    assert_eq!(read(&dir.join("00 Program.wav")), (0..8).flat_map(|n| samples(0, n)).collect::<Vec<f32>>());
}

#[test]
fn a_file_cut_anywhere_after_a_header_refresh_reads_to_the_refreshed_length() {
    let dir = scratch("cut");
    let mut rig = Rig::new(config(&dir), 4);
    let a = spec(1, "Mic");
    rig.c.set_tracks(vec![a.clone()]);
    for n in 0..2 {
        feed(&a.slot, 1, n);
    }
    rig.c.poll();
    rig.c.flush();
    rig.write().unwrap();
    rig.w.refresh_headers().unwrap();
    feed(&a.slot, 1, 2);
    rig.c.poll();
    rig.c.flush();
    rig.write().unwrap();
    let path = dir.join("Mic.wav");
    let full = std::fs::read(&path).unwrap();
    let head = wav::header_len().unwrap() as usize;
    let refreshed = head + 2 * BLOCK_SAMPLES * 4;
    assert_eq!(full.len(), head + 3 * BLOCK_SAMPLES * 4);
    let mut cuts: Vec<usize> = (refreshed..=full.len()).step_by(997).collect();
    cuts.extend([refreshed, refreshed + 1, refreshed + 3, full.len() - 1, full.len()]);
    for cut in cuts {
        let (_, buf) = craft_audio::decode(&full[..cut], Some("wav")).unwrap();
        assert_eq!(buf.frames(), 2 * BLOCK_FRAMES, "cut at byte {cut}");
    }
}

#[test]
fn a_take_that_ended_in_a_crash_is_repaired_from_its_files_and_journal() {
    let dir = scratch("crash");
    let mut rig = Rig::new(config(&dir), 8);
    let (a, b) = (spec(1, "Mic"), spec(2, "Game"));
    rig.c.set_tracks(vec![a.clone(), b.clone()]);
    for n in 0..3 {
        feed(&a.slot, 1, n);
        feed(&b.slot, 2, n);
    }
    rig.c.poll();
    rig.c.flush();
    rig.write().unwrap();
    rig.w.sync().unwrap();
    for n in 3..5 {
        feed(&a.slot, 1, n);
        feed(&b.slot, 2, n);
    }
    rig.c.poll();
    rig.c.flush();
    rig.write().unwrap();
    // While the take is being written, repair leaves it and its files alone.
    let recording = dir.parent().and_then(Path::parent).unwrap();
    let before = std::fs::metadata(dir.join("Mic.wav")).unwrap().len();
    assert_eq!(recover::repair(&dir).unwrap(), None);
    assert!(recover::repair_all(recording).unwrap().is_empty());
    assert_eq!(std::fs::metadata(dir.join("Mic.wav")).unwrap().len(), before);
    assert!(!journal::read(&dir).is_ok_and(|l| l.iter().any(|l| matches!(l, Line::End { .. }))));
    // The app stops here: no last header, no end line, the files are cut mid-frame, and the
    // journal's last line is cut short.
    drop(rig);
    let j = std::fs::OpenOptions::new().append(true).open(dir.join(journal::FILE_NAME)).unwrap();
    std::io::Write::write_all(&mut &j, b"{\"sync\":{\"fil").unwrap();
    drop(j);
    let head = wav::header_len().unwrap();
    let block = (BLOCK_SAMPLES * 4) as u64;
    for (name, keep) in [("Mic.wav", head + 4 * block + 5), ("Game.wav", head + 3 * block + 3)] {
        let f = std::fs::OpenOptions::new().write(true).open(dir.join(name)).unwrap();
        f.set_len(keep).unwrap();
    }
    assert_eq!(recover::unfinished(recording), vec![dir.clone()]);
    let done = recover::repair_all(recording).unwrap();
    let f = BLOCK_FRAMES as u64;
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].files, vec![("Mic.wav".to_string(), 4 * f), ("Game.wav".to_string(), 3 * f)]);
    assert_eq!(done[0].synced, vec![("Mic.wav".to_string(), 3 * f), ("Game.wav".to_string(), 3 * f)]);
    assert!(done[0].files.iter().zip(&done[0].synced).all(|(r, s)| r.1 >= s.1), "a repaired file holds what was synced");
    assert_eq!(read(&dir.join("Mic.wav")), (0..4).flat_map(|n| samples(1, n)).collect::<Vec<f32>>());
    assert_eq!(read(&dir.join("Game.wav")), (0..3).flat_map(|n| samples(2, n)).collect::<Vec<f32>>());
    assert!(recover::unfinished(recording).is_empty(), "the journal now ends");
    let lines = journal::read(&dir).unwrap();
    assert!(matches!(lines.last(), Some(Line::End { .. })), "the repaired journal reads back: {lines:?}");
    assert_eq!(recover::repair(&dir).unwrap(), None);
}

#[test]
fn a_track_whose_first_block_comes_after_the_start_begins_with_silence_from_the_start() {
    let dir = scratch("anchored");
    let mut rig = Rig::new(config(&dir), 8);
    // The take started at the time of block 0; the only track's audio begins at block 3.
    rig.c.set_start(Tick(500));
    let a = spec(1, "Mic");
    rig.c.set_tracks(vec![a.clone()]);
    for n in 3..5 {
        feed(&a.slot, 1, n);
    }
    rig.pump().unwrap();
    rig.end().unwrap();
    let silence = vec![0.0; 3 * BLOCK_SAMPLES];
    assert_eq!(read(&dir.join("Mic.wav")), [silence, samples(1, 3), samples(1, 4)].concat());
}

#[test]
fn a_track_whose_source_gives_no_audio_is_silence_for_the_whole_take() {
    let dir = scratch("silent-track");
    let mut rig = Rig::new(config(&dir), 8);
    let (a, b) = (spec(1, "Mic"), spec(2, "Unplugged"));
    rig.c.set_tracks(vec![a.clone(), b.clone()]);
    for n in 0..3 {
        feed(&a.slot, 1, n);
        rig.pump().unwrap();
    }
    rig.end().unwrap();
    assert_eq!(read(&dir.join("Mic.wav")), (0..3).flat_map(|n| samples(1, n)).collect::<Vec<f32>>());
    assert_eq!(read(&dir.join("Unplugged.wav")), vec![0.0; 3 * BLOCK_SAMPLES], "the file keeps time with the take");
}

#[test]
fn audio_lost_at_the_end_of_a_take_is_silence_to_the_end() {
    let dir = scratch("lost-at-end");
    // One chunk set, and two for the one track: the first 256 blocks fill them, and the last 3
    // find no free chunk.
    let mut rig = Rig::new(config(&dir), 1);
    let a = spec(1, "Mic");
    rig.c.set_tracks(vec![a.clone()]);
    let filled = 2 * CHUNK_BLOCKS as i64;
    for n in 0..filled + 3 {
        feed(&a.slot, 1, n);
        rig.c.poll();
    }
    assert_eq!(rig.c.dropped_frames(), 3 * BLOCK_FRAMES as u64);
    rig.end().unwrap();
    let silence = vec![0.0; 3 * BLOCK_SAMPLES];
    let want = [(0..filled).flat_map(|n| samples(1, n)).collect::<Vec<f32>>(), silence].concat();
    assert_eq!(read(&dir.join("Mic.wav")), want);
}

#[test]
fn a_take_whose_free_space_cannot_be_read_stops_with_that_reason() {
    let dir = scratch("space-unknown");
    let cfg = DiskConfig { reserve_bytes: 1 << 30, faults: Faults { space_unknown: true, ..Faults::default() }, ..config(&dir) };
    let (c, free, _) = Collector::new(1, 4).unwrap();
    match Writer::create(cfg, free, c.dropout_queue()) {
        Err(f) => assert_eq!(f.reason, StopReason::SpaceUnknown),
        Ok(_) => panic!("the take started without knowing the free space"),
    }
}

#[test]
fn a_take_whose_write_buffer_cannot_be_made_stops_with_that_reason_and_writes_nothing() {
    let dir = scratch("no-memory");
    let cfg = DiskConfig { faults: Faults { no_memory: true, ..Faults::default() }, ..config(&dir) };
    let take = Take::start(TakeConfig { disk: cfg, buffer_mib: 4 }, vec![spec(1, "Mic")]).unwrap();
    let status = take.join();
    assert_eq!((status.state(), status.reason()), (Some(State::Stopped), Some(StopReason::NoMemory)));
    assert!(!dir.exists(), "the take made its folder without a write buffer");
}

/// An unfinished take in `dir` whose `Mic.wav` ends mid-frame.
fn crashed_take(dir: &Path) {
    let mut rig = Rig::new(config(dir), 4);
    let a = spec(1, "Mic");
    rig.c.set_tracks(vec![a.clone()]);
    feed(&a.slot, 1, 0);
    rig.c.poll();
    rig.c.flush();
    rig.write().unwrap();
    drop(rig);
    let f = std::fs::OpenOptions::new().append(true).open(dir.join("Mic.wav")).unwrap();
    std::io::Write::write_all(&mut &f, &[1, 2, 3]).unwrap();
}

#[test]
fn a_journal_cut_inside_a_character_is_still_repaired() {
    let dir = scratch("cut-char");
    crashed_take(&dir);
    // The crash cut the last line inside the two bytes of "é".
    let j = std::fs::OpenOptions::new().append(true).open(dir.join(journal::FILE_NAME)).unwrap();
    std::io::Write::write_all(&mut &j, b"{\"stop\":{\"reason\":\"caf\xC3").unwrap();
    drop(j);
    let recording = dir.parent().and_then(Path::parent).unwrap();
    assert_eq!(recover::unfinished(recording), vec![dir.clone()]);
    let done = recover::repair_all(recording).unwrap();
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].files, vec![("Mic.wav".to_string(), BLOCK_FRAMES as u64)]);
    let lines = journal::read(&dir).unwrap();
    assert!(matches!(lines.last(), Some(Line::End { .. })), "the repaired journal reads back: {lines:?}");
}

#[cfg(unix)]
#[test]
fn a_journal_that_cannot_be_opened_is_reported_instead_of_skipped() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch("unreadable-journal");
    crashed_take(&dir);
    let path = dir.join(journal::FILE_NAME);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::File::open(&path).is_ok() {
        // Running as root, which reads the file anyway.
        return;
    }
    let recording = dir.parent().and_then(Path::parent).unwrap();
    assert_eq!(recover::unfinished(recording), vec![dir.clone()]);
    let out = recover::repair_each(recording);
    assert!(out.repaired.is_empty());
    assert_eq!(out.failed.len(), 1);
    assert_eq!(out.failed[0].1.kind(), std::io::ErrorKind::PermissionDenied);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
}

#[test]
fn a_journal_that_does_not_read_is_reported_and_the_other_takes_are_repaired() {
    let bad = scratch("bad-journal");
    crashed_take(&bad);
    let j = std::fs::OpenOptions::new().append(true).open(bad.join(journal::FILE_NAME)).unwrap();
    std::io::Write::write_all(&mut &j, b"not a journal line\n").unwrap();
    drop(j);
    let recording = bad.parent().and_then(Path::parent).unwrap().to_path_buf();
    let good = recording.join("Good Audio").join("Audio Files");
    crashed_take(&good);
    assert_eq!(recover::unfinished(&recording), vec![good.clone(), bad.clone()]);
    let out = recover::repair_each(&recording);
    assert_eq!(out.repaired.iter().map(|r| r.dir.clone()).collect::<Vec<_>>(), vec![good.clone()]);
    assert_eq!(out.failed.len(), 1);
    assert_eq!(out.failed[0].0, bad);
    assert!(out.failed[0].1.to_string().contains("journal line"), "{}", out.failed[0].1);
    let err = recover::repair_all(&recording).unwrap_err().to_string();
    assert!(err.contains(&bad.display().to_string()), "{err}");
}

#[test]
fn a_journal_that_names_a_path_leaves_the_take_unfinished_and_its_files_untouched() {
    let dir = scratch("path-in-journal");
    crashed_take(&dir);
    let mic_len = std::fs::metadata(dir.join("Mic.wav")).unwrap().len();
    let mut j = std::fs::OpenOptions::new().append(true).open(dir.join(journal::FILE_NAME)).unwrap();
    let line = Line::File { track: 1, name: "Desk".into(), file: "../Desk.wav".into(), segment: 1, first_frame: 0 };
    std::io::Write::write_all(&mut j, format!("{}\n", serde_json::to_string(&line).unwrap()).as_bytes()).unwrap();
    drop(j);
    let err = recover::repair(&dir).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    assert!(err.to_string().contains("../Desk.wav"), "{err}");
    assert_eq!(std::fs::metadata(dir.join("Mic.wav")).unwrap().len(), mic_len, "Mic.wav was not cut before the error");
    let recording = dir.parent().and_then(Path::parent).unwrap();
    assert_eq!(recover::unfinished(recording), vec![dir.clone()], "the journal was not ended");
    let _ = std::fs::remove_dir_all(recording);
}

#[test]
fn a_file_shorter_than_its_last_sync_leaves_the_take_unfinished_and_uncut() {
    let dir = scratch("short-file");
    crashed_take(&dir);
    let mic_len = std::fs::metadata(dir.join("Mic.wav")).unwrap().len();
    let mut j = std::fs::OpenOptions::new().append(true).open(dir.join(journal::FILE_NAME)).unwrap();
    let line = Line::Sync { tracks: vec![(0, 10 * BLOCK_FRAMES as u64)] };
    std::io::Write::write_all(&mut j, format!("{}\n", serde_json::to_string(&line).unwrap()).as_bytes()).unwrap();
    drop(j);
    let err = recover::repair(&dir).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    assert!(err.to_string().contains("fewer than"), "{err}");
    assert_eq!(std::fs::metadata(dir.join("Mic.wav")).unwrap().len(), mic_len, "Mic.wav was not cut before the error");
    let recording = dir.parent().and_then(Path::parent).unwrap();
    assert_eq!(recover::unfinished(recording), vec![dir.clone()], "the journal was not ended");
    let _ = std::fs::remove_dir_all(recording);
}

#[test]
fn a_file_that_is_not_a_wav_leaves_the_other_files_uncut() {
    let dir = scratch("bad-header");
    let mut rig = Rig::new(config(&dir), 4);
    let (a, b) = (spec(1, "Mic"), spec(2, "Desk"));
    rig.c.set_tracks(vec![a.clone(), b.clone()]);
    feed(&a.slot, 1, 0);
    feed(&b.slot, 2, 0);
    rig.c.poll();
    rig.c.flush();
    rig.write().unwrap();
    drop(rig);
    // Mic has a partial frame that repair would cut; Desk no longer starts like a WAV file.
    let f = std::fs::OpenOptions::new().append(true).open(dir.join("Mic.wav")).unwrap();
    std::io::Write::write_all(&mut &f, &[1, 2, 3]).unwrap();
    drop(f);
    let mut d = std::fs::OpenOptions::new().write(true).open(dir.join("Desk.wav")).unwrap();
    std::io::Write::write_all(&mut d, b"XXXX").unwrap();
    drop(d);
    let mic_len = std::fs::metadata(dir.join("Mic.wav")).unwrap().len();
    let err = recover::repair(&dir).unwrap_err();
    assert!(err.to_string().contains("Desk.wav"), "{err}");
    assert_eq!(std::fs::metadata(dir.join("Mic.wav")).unwrap().len(), mic_len, "Mic.wav was not cut before the error");
    let recording = dir.parent().and_then(Path::parent).unwrap();
    assert_eq!(recover::unfinished(recording), vec![dir.clone()], "the journal was not ended");
    let _ = std::fs::remove_dir_all(recording);
}

#[test]
fn a_journal_larger_than_the_limit_is_refused_before_it_is_read() {
    let dir = scratch("huge-journal");
    crashed_take(&dir);
    // A sparse file: its length is past the limit, and no data is written.
    let j = std::fs::OpenOptions::new().write(true).open(dir.join(journal::FILE_NAME)).unwrap();
    j.set_len(journal::MAX_BYTES + 1).unwrap();
    drop(j);
    let err = journal::read(&dir).unwrap_err();
    assert!(err.to_string().contains("larger than"), "{err}");
    let err = recover::repair(&dir).unwrap_err();
    assert!(err.to_string().contains("larger than"), "{err}");
    let _ = std::fs::remove_dir_all(dir.parent().and_then(Path::parent).unwrap());
}

#[test]
fn a_take_whose_file_is_missing_stays_unfinished_until_the_file_is_back() {
    let dir = scratch("missing-file");
    crashed_take(&dir);
    let away = dir.with_file_name("Mic.wav.away");
    std::fs::rename(dir.join("Mic.wav"), &away).unwrap();
    let err = recover::repair(&dir).unwrap_err();
    assert!(err.to_string().contains("Mic.wav is missing"), "{err}");
    let recording = dir.parent().and_then(Path::parent).unwrap();
    assert_eq!(recover::unfinished(recording), vec![dir.clone()], "the journal was not ended");
    std::fs::rename(&away, dir.join("Mic.wav")).unwrap();
    let done = recover::repair_all(recording).unwrap();
    assert_eq!(done[0].files, vec![("Mic.wav".to_string(), BLOCK_FRAMES as u64)]);
}

#[cfg(unix)]
#[test]
fn repair_follows_no_links_out_of_the_folder() {
    use std::os::unix::fs::symlink;
    let outside = scratch("links-outside");
    crashed_take(&outside);
    let outside_len = std::fs::metadata(outside.join("Mic.wav")).unwrap().len();
    // A recording folder holding a take folder whose `Audio Files` is a link to the take outside.
    let inside = scratch("links-inside");
    let recording = inside.parent().and_then(Path::parent).unwrap().to_path_buf();
    std::fs::create_dir_all(recording.join("Linked Audio")).unwrap();
    symlink(&outside, recording.join("Linked Audio").join("Audio Files")).unwrap();
    // And a real take whose journal names a file that is a link to the file outside.
    crashed_take(&inside);
    std::fs::remove_file(inside.join("Mic.wav")).unwrap();
    symlink(outside.join("Mic.wav"), inside.join("Mic.wav")).unwrap();
    assert_eq!(recover::unfinished(&recording), vec![inside.clone()], "the linked folder is left out");
    let err = recover::repair_all(&recording).unwrap_err();
    assert!(err.to_string().contains("Mic.wav is missing"), "the linked file is not repaired: {err}");
    assert_eq!(std::fs::metadata(outside.join("Mic.wav")).unwrap().len(), outside_len, "nothing outside was written");
    let _ = std::fs::remove_dir_all(&recording);
    let _ = std::fs::remove_dir_all(outside.parent().and_then(Path::parent).unwrap());
}

/// A track file or journal swapped for a link after repair looked at the folder is still not
/// opened: every open refuses a link.
#[cfg(unix)]
#[test]
fn track_files_and_journals_that_are_links_are_not_opened() {
    use std::os::unix::fs::symlink;
    let outside = scratch("swap-outside");
    crashed_take(&outside);
    let mic_len = std::fs::metadata(outside.join("Mic.wav")).unwrap().len();
    let journal_bytes = std::fs::read(outside.join(journal::FILE_NAME)).unwrap();
    let inside = scratch("swap-inside");
    std::fs::create_dir_all(&inside).unwrap();
    symlink(outside.join("Mic.wav"), inside.join("Mic.wav")).unwrap();
    symlink(outside.join(journal::FILE_NAME), inside.join(journal::FILE_NAME)).unwrap();
    let mic = inside.join("Mic.wav");
    assert!(wav::check(&mic).is_err(), "check opened a link");
    assert!(wav::cut(&mic, 0).is_err(), "cut opened a link");
    assert!(wav::repair(&mic).is_err(), "repair opened a link");
    assert!(journal::read(&inside).is_err(), "the journal was read through a link");
    assert!(journal::ended(&inside).is_err(), "the journal's end was read through a link");
    assert!(journal::Journal::open_idle(&inside).is_err(), "the journal was opened through a link");
    assert_eq!(std::fs::metadata(outside.join("Mic.wav")).unwrap().len(), mic_len, "nothing outside was written");
    assert_eq!(std::fs::read(outside.join(journal::FILE_NAME)).unwrap(), journal_bytes, "the journal outside was not changed");
    let _ = std::fs::remove_dir_all(inside.parent().and_then(Path::parent).unwrap());
    let _ = std::fs::remove_dir_all(outside.parent().and_then(Path::parent).unwrap());
}

/// The `bext` time reference of the WAV at `path`, read as the audio editor reads it.
fn time_reference(path: &Path) -> u64 {
    craft_audio::probe(&std::fs::read(path).unwrap(), Some("wav")).unwrap().bwf.unwrap().time_reference
}

#[test]
fn every_file_and_part_carries_its_place_on_the_output_timeline() {
    let dir = scratch("timeline");
    let head = wav::header_len().unwrap();
    // The take started 2 s into the recording; parts of 3,000 frames.
    let timing = Timing {
        anchor: "recording".into(),
        origin_tick: Some(500),
        offset_frames: 96_000,
        started_unix_ms: Some(1_800_000_000_000),
        date: "2026-11-18".into(),
        time: "21:00:02".into(),
    };
    let cfg = DiskConfig { segment_bytes: head + 3_000 * 8, timing, ..config(&dir) };
    let mut rig = Rig::new(cfg, 4);
    rig.c.set_start(Tick(500));
    let (a, b) = (spec(1, "Mic"), spec(2, "Late"));
    rig.c.set_tracks(vec![a.clone(), b.clone()]);
    for n in 0..4 {
        feed(&a.slot, 1, n);
        rig.pump().unwrap();
    }
    feed(&b.slot, 2, 4);
    feed(&a.slot, 1, 4);
    rig.pump().unwrap();
    rig.end().unwrap();
    let lines = journal::read(&dir).unwrap();
    let want = Line::Start {
        version: 1,
        sample_rate: 48_000,
        channels: 2,
        anchor: "recording".into(),
        origin_tick: Some(500),
        offset_frames: 96_000,
        started_unix_ms: Some(1_800_000_000_000),
    };
    assert_eq!(lines.first(), Some(&want));
    for (file, first_frame) in [("Mic.wav", 0), ("Mic part 2.wav", 3_000), ("Late.wav", 0), ("Late part 2.wav", 3_000)] {
        assert_eq!(time_reference(&dir.join(file)), 96_000 + first_frame, "{file}");
    }
    // Late's file starts at the take's start, with silence before its first block.
    let late: Vec<f32> = [read(&dir.join("Late.wav")), read(&dir.join("Late part 2.wav"))].concat();
    assert_eq!(late.len(), 5 * BLOCK_SAMPLES);
    assert_eq!(&late[4 * BLOCK_SAMPLES..], samples(2, 4).as_slice());
}

#[test]
fn a_take_ended_at_its_end_time_has_files_of_exactly_that_length() {
    let dir = scratch("end-time");
    let (p, quiet) = (TrackSpec { source: None, name: "00 Program".into(), slot: Arc::new(TapSlot::new()) }, spec(2, "Quiet"));
    let take = Take::start_from(TakeConfig { disk: config(&dir), buffer_mib: 4 }, vec![p.clone(), quiet.clone()], Some(Tick(500))).unwrap();
    for n in 0..10 {
        feed(&p.slot, 0, n);
    }
    // The recording ended 100 frames into block 5; the mixer still made blocks after it.
    take.stop_at(Tick(500 + 5 * BLOCK_TICKS + 100 * FRAME_TICKS));
    let status = take.join();
    assert_eq!(status.state(), Some(State::Done));
    let frames = 5 * BLOCK_FRAMES + 100;
    let program = read(&dir.join("00 Program.wav"));
    assert_eq!(program, (0..10).flat_map(|n| samples(0, n)).take(frames * 2).collect::<Vec<f32>>());
    assert_eq!(read(&dir.join("Quiet.wav")), vec![0.0; frames * 2], "a silent track ends at the end time too");
    assert_eq!(status.dir(), Some(dir.as_path()));
}

/// The end time can arrive after a whole chunk past it reached the disk; the files are still cut
/// at the end time.
#[test]
fn audio_already_written_past_the_end_time_is_cut() {
    let dir = scratch("end-after-write");
    let mut rig = Rig::new(config(&dir), 4);
    let a = spec(1, "Mic");
    rig.c.set_start(Tick(500));
    rig.c.set_tracks(vec![a.clone()]);
    let blocks = CHUNK_BLOCKS as i64 + 8;
    for n in 0..blocks {
        feed(&a.slot, 1, n);
        if n % 16 == 15 {
            rig.pump().unwrap();
        }
    }
    rig.pump().unwrap();
    rig.c.set_end(Tick(500 + 5 * BLOCK_TICKS + 100 * FRAME_TICKS));
    rig.end().unwrap();
    let frames = 5 * BLOCK_FRAMES + 100;
    assert_eq!(read(&dir.join("Mic.wav")), (0..blocks).flat_map(|n| samples(1, n)).take(frames * 2).collect::<Vec<f32>>());
    let lines = journal::read(&dir).unwrap();
    assert!(matches!(lines.last(), Some(Line::End { frames: f }) if *f == frames as u64), "{:?}", lines.last());
}

/// The same across parts: the parts that start after the end time are removed, and the part the
/// end time falls in is cut.
#[test]
fn parts_written_past_the_end_time_are_removed() {
    let dir = scratch("end-after-parts");
    let head = wav::header_len().unwrap();
    let cfg = DiskConfig { segment_bytes: head + 3_000 * 8, ..config(&dir) };
    let mut rig = Rig::new(cfg, 4);
    let a = spec(1, "Mic");
    rig.c.set_start(Tick(500));
    rig.c.set_tracks(vec![a.clone()]);
    let blocks = CHUNK_BLOCKS as i64 + 8;
    for n in 0..blocks {
        feed(&a.slot, 1, n);
        if n % 16 == 15 {
            rig.pump().unwrap();
        }
    }
    rig.pump().unwrap();
    assert!(dir.join("Mic part 3.wav").exists(), "a whole chunk reached the disk before the end time");
    rig.c.set_end(Tick(500 + 3 * BLOCK_TICKS + 100 * FRAME_TICKS));
    rig.end().unwrap();
    let frames = 3 * BLOCK_FRAMES + 100;
    let mut files: Vec<String> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    files.sort();
    assert_eq!(files, ["Mic part 2.wav", "Mic.wav", journal::FILE_NAME]);
    let mut joined = read(&dir.join("Mic.wav"));
    joined.extend(read(&dir.join("Mic part 2.wav")));
    assert_eq!(joined, (0..blocks).flat_map(|n| samples(1, n)).take(frames * 2).collect::<Vec<f32>>());
    let lines = journal::read(&dir).unwrap();
    assert!(matches!(lines.last(), Some(Line::End { frames: f }) if *f == frames as u64), "{:?}", lines.last());
}

/// A take that stopped part way through the cut at its end time: the journal holds the cut, the
/// last part is already gone, and the others are not cut yet. Repair finishes the cut.
#[test]
fn repair_finishes_a_cut_the_take_stopped_during() {
    let dir = scratch("cut-repair");
    let head = wav::header_len().unwrap();
    let cfg = DiskConfig { segment_bytes: head + 3_000 * 8, ..config(&dir) };
    let blocks = CHUNK_BLOCKS as i64 + 8;
    {
        let mut rig = Rig::new(cfg, 4);
        let a = spec(1, "Mic");
        rig.c.set_start(Tick(500));
        rig.c.set_tracks(vec![a.clone()]);
        for n in 0..blocks {
            feed(&a.slot, 1, n);
            if n % 16 == 15 {
                rig.pump().unwrap();
            }
        }
        rig.pump().unwrap();
        rig.w.sync().unwrap();
    }
    let frames = 3 * BLOCK_FRAMES + 100;
    let (mut j, _) = journal::Journal::open_idle(&dir).unwrap().unwrap();
    j.write(&Line::Cut { track: 0, frames: frames as u64 }).unwrap();
    j.sync().unwrap();
    drop(j);
    let last = (CHUNK_BLOCKS * BLOCK_FRAMES).div_ceil(3_000);
    std::fs::remove_file(dir.join(format!("Mic part {last}.wav"))).unwrap();
    let r = recover::repair(&dir).unwrap().unwrap();
    assert_eq!(r.files, [("Mic.wav".to_string(), 3_000), ("Mic part 2.wav".to_string(), frames as u64 - 3_000)]);
    let mut files: Vec<String> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    files.sort();
    assert_eq!(files, ["Mic part 2.wav", "Mic.wav", journal::FILE_NAME]);
    let mut joined = read(&dir.join("Mic.wav"));
    joined.extend(read(&dir.join("Mic part 2.wav")));
    assert_eq!(joined, (0..blocks).flat_map(|n| samples(1, n)).take(frames * 2).collect::<Vec<f32>>());
}

/// A write that fails after the cuts are journaled and before every file is cut leaves the take
/// for repair, which makes the cuts.
#[test]
fn a_take_that_fails_before_its_cuts_are_made_is_left_for_repair() {
    // Mic gets no audio, so finishing pads its file from 0; Game has audio past the end.
    let run = |dir: &Path, faults: Faults| {
        let mut rig = Rig::new(DiskConfig { faults, ..config(dir) }, 4);
        let (mic, game) = (spec(1, "Mic"), spec(2, "Game"));
        rig.c.set_start(Tick(500));
        rig.c.set_tracks(vec![mic.clone(), game.clone()]);
        for n in 0..CHUNK_BLOCKS as i64 {
            feed(&game.slot, 1, n);
            if n % 16 == 15 {
                rig.pump().unwrap();
            }
        }
        rig.pump().unwrap();
        rig.c.set_end(Tick(500 + 5 * BLOCK_TICKS));
        let written = rig.w.progress().bytes;
        let ended = rig.end();
        (rig, written, ended)
    };
    let (_, written, ended) = run(&scratch("cut-fails-measure"), Faults::default());
    ended.unwrap();
    let dir = scratch("cut-fails");
    let (mut rig, _, ended) = run(&dir, Faults { full_after_bytes: Some(written), ..Faults::default() });
    assert!(ended.is_err(), "padding Mic fills the disk");
    rig.w.abandon(StopReason::DiskFull);
    drop(rig);
    let lines = journal::read(&dir).unwrap();
    assert!(lines.contains(&Line::Cut { track: 1, frames: 5 * BLOCK_FRAMES as u64 }), "{lines:?}");
    assert!(!lines.iter().any(|l| matches!(l, Line::End { .. })), "{lines:?}");
    let recording = dir.parent().and_then(Path::parent).unwrap();
    assert_eq!(recover::unfinished(recording), vec![dir.clone()]);
    recover::repair(&dir).unwrap().unwrap();
    assert_eq!(read(&dir.join("Game.wav")), (0..5).flat_map(|n| samples(1, n)).collect::<Vec<f32>>());
}

/// A take stopped by a failed write keeps every whole frame on the disk, including frames a
/// failed write got out, and its journal ends.
#[test]
fn a_failed_write_keeps_the_whole_frames_it_got_out() {
    let dir = scratch("partial-write");
    let mut rig = Rig::new(DiskConfig { faults: Faults { full_after_bytes: Some(3 * 8192), ..Faults::default() }, ..config(&dir) }, 4);
    let a = spec(1, "Mic");
    rig.c.set_start(Tick(500));
    rig.c.set_tracks(vec![a.clone()]);
    let mut failed = false;
    for n in 0..CHUNK_BLOCKS as i64 {
        feed(&a.slot, 1, n);
        if n % 16 == 15 {
            failed |= rig.pump().is_err();
        }
    }
    assert!(failed || rig.pump().is_err() || rig.end().is_err(), "the disk fills");
    // The failed write got two more frames and part of a third out.
    let mut f = std::fs::OpenOptions::new().append(true).open(dir.join("Mic.wav")).unwrap();
    std::io::Write::write_all(&mut f, &[0u8; 2 * 8 + 3]).unwrap();
    drop(f);
    rig.w.abandon(StopReason::DiskFull);
    drop(rig);
    let lines = journal::read(&dir).unwrap();
    assert!(matches!(lines.last(), Some(Line::End { .. })), "{lines:?}");
    let want = [(0..3).flat_map(|n| samples(1, n)).collect::<Vec<f32>>(), vec![0.0; 4]].concat();
    assert_eq!(read(&dir.join("Mic.wav")), want);
}

/// Append `lines` to the journal of the take in `dir`.
fn append_lines(dir: &Path, lines: &[Line]) {
    let mut j = std::fs::OpenOptions::new().append(true).open(dir.join(journal::FILE_NAME)).unwrap();
    for line in lines {
        std::io::Write::write_all(&mut j, format!("{}\n", serde_json::to_string(line).unwrap()).as_bytes()).unwrap();
    }
}

#[test]
fn a_cut_part_that_is_not_a_track_file_leaves_the_take_unfinished_and_the_file_in_place() {
    let dir = scratch("cut-not-wav");
    crashed_take(&dir);
    std::fs::write(dir.join("notes.txt"), b"not audio").unwrap();
    let mic_len = std::fs::metadata(dir.join("Mic.wav")).unwrap().len();
    append_lines(
        &dir,
        &[Line::File { track: 0, name: "Mic".into(), file: "notes.txt".into(), segment: 2, first_frame: 4_096 }, Line::Cut { track: 0, frames: 10 }],
    );
    let err = recover::repair(&dir).unwrap_err();
    assert!(err.to_string().contains("notes.txt"), "{err}");
    assert_eq!(std::fs::read(dir.join("notes.txt")).unwrap(), b"not audio", "the file was not removed");
    assert_eq!(std::fs::metadata(dir.join("Mic.wav")).unwrap().len(), mic_len, "Mic.wav was not cut before the error");
    let recording = dir.parent().and_then(Path::parent).unwrap();
    assert_eq!(recover::unfinished(recording), vec![dir.clone()], "the journal was not ended");
    let _ = std::fs::remove_dir_all(recording);
}

#[test]
fn a_cut_never_removes_the_first_file_of_a_track() {
    let dir = scratch("cut-first-file");
    crashed_take(&dir);
    // A second track whose only file the journal says starts after the cut.
    std::fs::copy(dir.join("Mic.wav"), dir.join("Desk.wav")).unwrap();
    append_lines(
        &dir,
        &[Line::File { track: 1, name: "Desk".into(), file: "Desk.wav".into(), segment: 1, first_frame: 4_096 }, Line::Cut { track: 1, frames: 10 }],
    );
    let r = recover::repair(&dir).unwrap().unwrap();
    assert!(dir.join("Desk.wav").exists(), "the track's first file was removed");
    assert!(r.files.iter().any(|(f, _)| f == "Desk.wav"), "{:?}", r.files);
    let _ = std::fs::remove_dir_all(dir.parent().and_then(Path::parent).unwrap());
}

/// Repair of the crashed take in `dir` after `lines` fails with an error naming `named`, and
/// leaves `Mic.wav` as it was and the take unfinished.
fn repair_refused(dir: &Path, lines: &[Line], named: &str) {
    let mic = std::fs::read(dir.join("Mic.wav")).unwrap();
    append_lines(dir, lines);
    let err = recover::repair(dir).unwrap_err();
    assert!(err.to_string().contains(named), "{err}");
    assert_eq!(std::fs::read(dir.join("Mic.wav")).unwrap(), mic, "Mic.wav was changed before the error");
    let recording = dir.parent().and_then(Path::parent).unwrap();
    assert_eq!(recover::unfinished(recording), vec![dir.to_path_buf()], "the journal was not ended");
}

#[test]
fn a_cut_part_named_like_a_kept_file_in_other_letter_case_is_refused() {
    for (tag, part) in [("upper", "MIC.WAV"), ("upper-stem", "MIC.wav")] {
        let dir = scratch(&format!("cut-case-{tag}"));
        crashed_take(&dir);
        let lines =
            [Line::File { track: 0, name: "Mic".into(), file: part.into(), segment: 2, first_frame: 4_096 }, Line::Cut { track: 0, frames: 10 }];
        repair_refused(&dir, &lines, part);
        assert!(dir.join("Mic.wav").exists(), "{part}: Mic.wav was removed");
        let _ = std::fs::remove_dir_all(dir.parent().and_then(Path::parent).unwrap());
    }
}

#[test]
fn a_cut_part_that_is_a_hard_link_to_a_kept_file_is_refused() {
    let dir = scratch("cut-hard-link");
    crashed_take(&dir);
    std::fs::hard_link(dir.join("Mic.wav"), dir.join("Mic part 2.wav")).unwrap();
    let lines = [
        Line::File { track: 0, name: "Mic".into(), file: "Mic part 2.wav".into(), segment: 2, first_frame: 4_096 },
        Line::Cut { track: 0, frames: 10 },
    ];
    repair_refused(&dir, &lines, "Mic part 2.wav");
    assert!(dir.join("Mic.wav").exists() && dir.join("Mic part 2.wav").exists());
    let _ = std::fs::remove_dir_all(dir.parent().and_then(Path::parent).unwrap());
}

#[test]
fn a_track_file_with_another_name_is_not_rewritten() {
    let dir = scratch("kept-hard-link");
    crashed_take(&dir);
    std::fs::hard_link(dir.join("Mic.wav"), dir.join("elsewhere.bin")).unwrap();
    repair_refused(&dir, &[], "Mic.wav");
    let _ = std::fs::remove_dir_all(dir.parent().and_then(Path::parent).unwrap());
}

#[test]
fn a_track_file_named_with_a_tilde_by_an_earlier_writer_is_repaired() {
    let dir = scratch("tilde-name");
    crashed_take(&dir);
    std::fs::copy(dir.join("Mic.wav"), dir.join("Desk~1.wav")).unwrap();
    append_lines(&dir, &[Line::File { track: 1, name: "Desk~1".into(), file: "Desk~1.wav".into(), segment: 1, first_frame: 0 }]);
    let done = recover::repair(&dir).unwrap().unwrap_or_else(|| panic!("the take was skipped"));
    let names: Vec<&str> = done.files.iter().map(|(f, _)| f.as_str()).collect();
    assert_eq!(names, ["Mic.wav", "Desk~1.wav"]);
    let _ = std::fs::remove_dir_all(dir.parent().and_then(Path::parent).unwrap());
}

#[cfg(windows)]
#[test]
fn a_track_file_named_as_another_files_short_name_is_refused() {
    let dir = scratch("short-name");
    crashed_take(&dir);
    std::fs::copy(dir.join("Mic.wav"), dir.join("Microphone track.wav")).unwrap();
    if !dir.join("MICROP~1.wav").exists() {
        eprintln!("skipped: the volume makes no short names");
        let _ = std::fs::remove_dir_all(dir.parent().and_then(Path::parent).unwrap());
        return;
    }
    let lines = [Line::File { track: 1, name: "Desk".into(), file: "MICROP~1.wav".into(), segment: 1, first_frame: 0 }];
    repair_refused(&dir, &lines, "MICROP~1.wav");
    let _ = std::fs::remove_dir_all(dir.parent().and_then(Path::parent).unwrap());
}

#[test]
fn takes_given_the_same_folder_name_each_get_their_own_folder() {
    let recording = scratch("folders").parent().and_then(Path::parent).unwrap().to_path_buf();
    let cfg = DiskConfig { folder_stem: Some("2026-11-18 21-00-00".into()), ..config(&recording) };
    let first = Take::start(TakeConfig { disk: cfg.clone(), buffer_mib: 1 }, vec![spec(1, "Mic")]).unwrap();
    let second = Take::start(TakeConfig { disk: cfg, buffer_mib: 1 }, vec![spec(1, "Mic")]).unwrap();
    let mut dirs = vec![first.join().dir().unwrap().to_path_buf(), second.join().dir().unwrap().to_path_buf()];
    dirs.sort();
    assert_eq!(
        dirs,
        [recording.join("2026-11-18 21-00-00 (2) Audio").join("Audio Files"), recording.join("2026-11-18 21-00-00 Audio").join("Audio Files")]
    );
    let _ = std::fs::remove_dir_all(&recording);
}

#[test]
fn a_take_thrown_away_leaves_no_folder() {
    let recording = scratch("discard").parent().and_then(Path::parent).unwrap().to_path_buf();
    let cfg = DiskConfig { folder_stem: Some("Never".into()), ..config(&recording) };
    let p = TrackSpec { source: None, name: "00 Program".into(), slot: Arc::new(TapSlot::new()) };
    let take = Take::start(TakeConfig { disk: cfg, buffer_mib: 1 }, vec![p.clone()]).unwrap();
    for n in 0..3 {
        feed(&p.slot, 0, n);
    }
    take.discard();
    let status = take.join();
    assert_eq!(status.state(), Some(State::Done));
    let left: Vec<PathBuf> = std::fs::read_dir(&recording).unwrap().flatten().map(|e| e.path()).collect();
    assert!(left.is_empty(), "{left:?}");
    let _ = std::fs::remove_dir_all(&recording);
}

#[test]
fn a_fault_on_the_disk_thread_stops_the_take_with_a_write_failure() {
    let dir = scratch("disk-fault");
    let cfg = DiskConfig { faults: Faults { panic_on_chunk: true, ..Faults::default() }, ..config(&dir) };
    let p = TrackSpec { source: None, name: "00 Program".into(), slot: Arc::new(TapSlot::new()) };
    let take = Take::start(TakeConfig { disk: cfg, buffer_mib: 1 }, vec![p.clone()]).unwrap();
    for n in 0..3 {
        feed(&p.slot, 0, n);
    }
    // The stop sends the chunk, and the disk thread fails on it; both threads end.
    let status = take.join();
    assert_eq!((status.state(), status.reason()), (Some(State::Stopped), Some(StopReason::WriteFailed)));
}

#[test]
fn a_sync_journals_the_tracks_that_grew_and_the_dropouts_that_ended() {
    let dir = scratch("sync-lines");
    let mut rig = Rig::new(config(&dir), 2);
    let (a, b) = (spec(1, "Mic"), spec(2, "Quiet"));
    rig.c.set_tracks(vec![a.clone(), b.clone()]);
    // Mic's tap loses blocks 1 and 2.
    feed(&a.slot, 1, 0);
    rig.pump().unwrap();
    feed(&a.slot, 1, 3);
    rig.pump().unwrap();
    rig.c.flush();
    rig.write().unwrap();
    rig.w.sync().unwrap();
    // Nothing grew since: the next sync adds no sync line.
    rig.w.sync().unwrap();
    let f = BLOCK_FRAMES as u64;
    let lines: Vec<Line> = journal::read(&dir).unwrap().into_iter().filter(|l| matches!(l, Line::Sync { .. } | Line::Dropout { .. })).collect();
    assert_eq!(lines, [Line::Dropout { track: 0, at: f, frames: 2 * f }, Line::Sync { tracks: vec![(0, 4 * f)] }]);
    rig.end().unwrap();
    let lines = journal::read(&dir).unwrap();
    assert_eq!(lines.iter().filter(|l| matches!(l, Line::Dropout { .. })).count(), 1, "the dropout is journaled once");
}
