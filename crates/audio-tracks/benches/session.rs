//! The session written when a take ends: 9 tracks (8 sources and the program mix) of a 24 hour
//! take in 1 GiB parts (25 parts a track, 225 files), with 1000 markers and 100 dropouts. The
//! disk thread builds it once per take, after the files are complete, so it is off the per-block
//! path; the case measures building it and building and writing it to a temporary folder.

use std::path::{Path, PathBuf};

use criterion::{Criterion, criterion_group, criterion_main};
use studiocraft_audio_tracks::journal::Line;
use studiocraft_audio_tracks::session::{self, FILES_FOLDER};

/// Frames in one 1 GiB part of 32-bit float stereo.
const PART_FRAMES: u64 = (1 << 30) / 8;
/// Frames in 24 hours at 48 kHz.
const DAY_FRAMES: u64 = 24 * 3600 * 48_000;

fn day_take() -> (Vec<Line>, Vec<(String, u64)>) {
    let mut lines = vec![Line::Start {
        version: 1,
        sample_rate: 48_000,
        channels: 2,
        anchor: "recording".into(),
        origin_tick: Some(0),
        offset_frames: 0,
        started_unix_ms: None,
        session: true,
    }];
    let mut lens = Vec::new();
    for track in 0..9usize {
        let name = if track == 0 { "00 Program".to_string() } else { format!("Source {track}") };
        let (mut first, mut segment) = (0u64, 1u32);
        while first < DAY_FRAMES {
            let file = if segment == 1 { format!("{name}.wav") } else { format!("{name} part {segment}.wav") };
            let n = PART_FRAMES.min(DAY_FRAMES - first);
            lines.push(Line::File { track, name: name.clone(), file: file.clone(), segment, first_frame: first, program: track == 0 });
            lens.push((file, n));
            first += n;
            segment += 1;
        }
    }
    lines.extend((0..1000u64).map(|n| Line::Marker { at: n * (DAY_FRAMES / 1000), name: format!("Marker {n}"), number: Some(n + 1) }));
    lines.extend((0..100u64).map(|n| Line::Dropout { track: (n % 9) as usize, at: n * 863_999, frames: 1024 }));
    (lines, lens)
}

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("studiocraft-session-bench-{}", std::process::id())).join("Day Audio").join(FILES_FOLDER);
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn session_24h(c: &mut Criterion) {
    let (lines, lens) = day_take();
    let lens: std::collections::HashMap<String, u64> = lens.into_iter().collect();
    let frames = |f: &str| lens.get(f).copied();
    c.bench_function("session_build_9_tracks_24h_1000_markers", |b| b.iter(|| session::build("Day", &lines, frames)));
    let dir = scratch();
    c.bench_function("session_write_9_tracks_24h_1000_markers", |b| b.iter(|| session::write_from(&dir, &lines, 0, frames)));
    if let Some(root) = dir.parent().and_then(Path::parent) {
        let _ = std::fs::remove_dir_all(root);
    }
}

criterion_group!(benches, session_24h);
criterion_main!(benches);
