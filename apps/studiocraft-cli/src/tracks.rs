//! `studiocraft-cli audio-tracks repair <folder>` and `studiocraft-cli bench audio-tracks`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use studiocraft_audio_tracks::collect::BLOCK_TICKS;
use studiocraft_audio_tracks::{DiskConfig, Faults, Gate, Take, TakeConfig, TrackSpec, recover, space, wav};
use studiocraft_media::Tick;
use studiocraft_media::audio::BLOCK_FRAMES;
use studiocraft_media::tap::TapSlot;
use studiocraft_model::SourceId;

use crate::CliError;

/// `audio-tracks <args>`.
pub fn run(args: &[String]) -> Result<(), CliError> {
    match args {
        [cmd, folder] if cmd == "repair" => repair(Path::new(folder)).map_err(CliError::Failed),
        [cmd, ..] if cmd == "repair" => Err(CliError::Usage("`audio-tracks repair` takes one folder".into())),
        _ => Err(CliError::Usage("the audio-tracks command is `audio-tracks repair <folder>`".into())),
    }
}

fn repair(folder: &Path) -> Result<(), String> {
    if !folder.is_dir() {
        return Err(format!("{} is not a folder", folder.display()));
    }
    let out = recover::repair_each(folder);
    if out.repaired.is_empty() && out.failed.is_empty() {
        println!("{}: no take needs repair", folder.display());
    }
    for r in &out.repaired {
        for (file, frames) in &r.files {
            println!("{}: {file}: {:.3} s", r.dir.display(), *frames as f64 / 48_000.0);
        }
    }
    if out.failed.is_empty() {
        return Ok(());
    }
    let why: Vec<String> = out.failed.iter().map(|(d, e)| format!("{}: {e}", d.display())).collect();
    Err(format!("could not repair {}", why.join("; ")))
}

/// What `bench audio-tracks` runs.
#[derive(Debug, PartialEq)]
pub struct BenchArgs {
    pub seconds: f64,
    /// The disk takes nothing for this long, from `stall_at`.
    pub stall: f64,
    pub stall_at: f64,
    pub buffer_mib: u32,
    pub tracks: usize,
    pub folder: Option<PathBuf>,
}

/// The options of `bench audio-tracks`.
pub fn bench_args(args: &[String]) -> Result<BenchArgs, CliError> {
    let mut b = BenchArgs { seconds: 60.0, stall: 40.0, stall_at: 5.0, buffer_mib: 96, tracks: 9, folder: None };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = |name: &str| it.next().cloned().ok_or_else(|| CliError::Usage(format!("{name} needs a value")));
        let num = |name: &str, v: String, lo: f64, hi: f64| match v.parse::<f64>() {
            Ok(n) if (lo..=hi).contains(&n) => Ok(n),
            _ => Err(CliError::Usage(format!("{name} `{v}` is not a number from {lo} to {hi}"))),
        };
        let whole = |name: &str, v: String, lo: u32, hi: u32| match v.parse::<u32>() {
            Ok(n) if (lo..=hi).contains(&n) => Ok(n),
            _ => Err(CliError::Usage(format!("{name} `{v}` is not a whole number from {lo} to {hi}"))),
        };
        match a.as_str() {
            "--seconds" => b.seconds = num("--seconds", value("--seconds")?, 1.0, 3600.0)?,
            "--stall" => b.stall = num("--stall", value("--stall")?, 0.0, 3600.0)?,
            "--stall-at" => b.stall_at = num("--stall-at", value("--stall-at")?, 0.0, 3600.0)?,
            "--buffer" => b.buffer_mib = whole("--buffer", value("--buffer")?, 16, 1024)?,
            "--tracks" => b.tracks = whole("--tracks", value("--tracks")?, 1, 64)? as usize,
            "--folder" => b.folder = Some(PathBuf::from(value("--folder")?)),
            other => return Err(CliError::Usage(format!("unknown option `{other}` for `bench audio-tracks`"))),
        }
    }
    Ok(b)
}

/// Feed `tracks` taps at the mixer's pace while the disk stalls, and report what was lost and
/// whether every file kept time.
pub fn bench(b: &BenchArgs) -> Result<(), String> {
    let base = b.folder.clone().unwrap_or_else(|| std::env::temp_dir().join(format!("studiocraft-bench-tracks-{}", std::process::id())));
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let dir = base.join(format!("Bench {stamp} Audio")).join("Audio Files");
    let gate = Gate::closed();
    gate.open();
    let specs: Vec<TrackSpec> = (0..b.tracks)
        .map(|i| TrackSpec {
            source: (i > 0).then_some(SourceId(i as u64)),
            name: if i == 0 { "00 Program".into() } else { format!("Track {i}") },
            slot: Arc::new(TapSlot::new()),
        })
        .collect();
    let cfg = TakeConfig {
        disk: DiskConfig {
            dir: dir.clone(),
            folder_stem: None,
            timing: Default::default(),
            segment_bytes: 1 << 30,
            reserve_bytes: space::reserve(0),
            warn_bytes: space::warning(0, b.tracks as u64),
            faults: Faults { gate: Some(Arc::clone(&gate)), ..Faults::default() },
        },
        buffer_mib: b.buffer_mib,
    };
    let take = Take::start(cfg, specs.clone()).map_err(|e| e.to_string())?;
    let block = Duration::from_secs_f64(BLOCK_FRAMES as f64 / 48_000.0);
    let blocks = (b.seconds * 48_000.0 / BLOCK_FRAMES as f64).ceil() as i64;
    let (stall_from, stall_to) = (b.stall_at, b.stall_at + b.stall);
    let start = Instant::now();
    let mut stalled = false;
    let mut late = Duration::ZERO;
    for n in 0..blocks {
        let at = n as f64 * BLOCK_FRAMES as f64 / 48_000.0;
        match (stalled, at >= stall_from && at < stall_to) {
            (false, true) => {
                gate.close();
                stalled = true;
            }
            (true, false) => {
                gate.open();
                stalled = false;
            }
            _ => {}
        }
        for s in &specs {
            if let Some(mut blk) = s.slot.claim() {
                blk.clear();
                blk.samples.fill(0.25);
                blk.pts = Tick(n * BLOCK_TICKS);
                s.slot.push(blk);
            }
        }
        let due = start + block * u32::try_from(n + 1).unwrap_or(u32::MAX);
        match due.checked_duration_since(Instant::now()) {
            Some(wait) => std::thread::sleep(wait),
            None => late = late.max(Instant::now() - due),
        }
    }
    gate.open();
    let status = take.join();
    let head = wav::header_len().map_err(|e| e.to_string())?;
    let mut frames = Vec::new();
    for e in std::fs::read_dir(&dir).map_err(|e| e.to_string())?.flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "wav") {
            let len = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            frames.push(len.saturating_sub(head) / 8);
        }
    }
    let fed = blocks as u64 * BLOCK_FRAMES as u64;
    let report = json!({
        "tracks": b.tracks, "seconds": b.seconds, "buffer_mib": b.buffer_mib, "stall_seconds": b.stall,
        "buffer_seconds": f64::from(b.buffer_mib) * 1_048_576.0 / (b.tracks as f64 * 384_000.0),
        "dropped_seconds": status.dropped_seconds(), "dropped_per_track_seconds": status.dropped_seconds() / b.tracks as f64,
        "every_file_keeps_time": frames.len() == b.tracks && frames.iter().all(|f| *f == fed),
        "bytes": status.bytes(), "state": format!("{:?}", status.state()), "feeder_late_ms_max": late.as_secs_f64() * 1000.0,
        "folder": dir.display().to_string(),
    });
    println!("{}", serde_json::to_string_pretty(&report).unwrap_or_default());
    if b.folder.is_none() {
        // Only this run's take folder, then the base folder if nothing else is in it.
        if let Some(take) = dir.parent() {
            let _ = std::fs::remove_dir_all(take);
        }
        let _ = std::fs::remove_dir(&base);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn bench_options_are_read_and_checked() {
        let b = bench_args(&strings(&["--seconds", "10", "--stall", "4", "--buffer", "32", "--tracks", "3"])).unwrap();
        assert_eq!((b.seconds, b.stall, b.buffer_mib, b.tracks), (10.0, 4.0, 32, 3));
        assert_eq!(bench_args(&[]).unwrap().stall, 40.0, "a 40 s stall by default");
        for bad in [
            &["--buffer", "8"][..],
            &["--tracks", "0"],
            &["--seconds"],
            &["--speed", "2"],
            &["--stall", "NaN"],
            &["--tracks", "1.9"],
            &["--buffer", "16.9"],
        ] {
            assert!(matches!(bench_args(&strings(bad)), Err(CliError::Usage(_))), "{bad:?}");
        }
    }

    #[test]
    fn repair_needs_one_folder() {
        assert!(matches!(run(&strings(&["repair"])), Err(CliError::Usage(_))));
        assert!(matches!(run(&strings(&["fix", "x"])), Err(CliError::Usage(_))));
        let missing = std::env::temp_dir().join(format!("studiocraft-cli-no-such-{}", std::process::id()));
        assert!(matches!(run(&strings(&["repair", &missing.to_string_lossy()])), Err(CliError::Failed(_))));
    }
}
