//! How fast the built-in H.264 encoder codes moving pictures at each size, frame rate and preset.
//!
//! ```text
//! cargo run --release -p studiocraft-encode --example encode_bench [<seconds> [<threads>]]
//! ```
//!
//! Each case codes `<seconds>` of frames (default 3) after one second of warm-up, in quality mode
//! at level 23, and prints the frames coded per second of wall time and the share of real time
//! that took. A share under 100% keeps up with live recording. The encoder runs on `<threads>`
//! threads, by default as many as a recording gives it on this computer.

use std::time::Instant;

use studiocraft_encode::h264::{self, H264Session};
use studiocraft_model::Settings;
use studiocraft_plugin::{Packet, RateControl, VideoParams, VideoSession};

/// An NV12 frame with a diagonal gradient, a box that moves 7 pixels a frame and a noise band, so
/// every frame has motion and detail to code.
fn frame(width: usize, height: usize, n: usize, out: &mut [u8]) {
    let (bx, by) = ((n * 7) % width.saturating_sub(96).max(1), height / 4);
    let mut seed = (n as u32).wrapping_mul(2_654_435_761) | 1;
    for y in 0..height {
        for x in 0..width {
            let inside = x >= bx && x < bx + 96 && y >= by && y < by + 96;
            let v = if inside {
                220
            } else if y > height * 3 / 4 {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                16 + (seed % 200) as u8
            } else {
                (16 + (x + y + n) * 200 / (width + height + n)) as u8
            };
            out[y * width + x] = v;
        }
    }
    for (i, c) in out[width * height..].iter_mut().enumerate() {
        *c = (64 + (i + n) % 128) as u8;
    }
}

fn run(width: u32, height: u32, fps: u32, preset: &str, seconds: f64, threads: Option<usize>) -> Result<(f64, f64), String> {
    let params = VideoParams { width, height, fps_num: fps, fps_den: 1, rate: RateControl::Quality(23) };
    let mut config = h264::config(&Settings::new().with("preset", preset), &params).map_err(|e| e.to_string())?;
    if let Some(threads) = threads {
        config.threads = threads;
    }
    let mut session = H264Session::new(config).map_err(|e| e.to_string())?;
    let (w, h) = (width as usize, height as usize);
    // One second of different frames is made ahead and cycled, so the timing is the encoder's
    // alone and no frame repeats within the frames the encoder refers to.
    let frames: Vec<Vec<u8>> = (0..fps as usize)
        .map(|n| {
            let mut f = vec![0u8; w * h * 3 / 2];
            frame(w, h, n, &mut f);
            f
        })
        .collect();
    let warm = fps as usize;
    let timed = (f64::from(fps) * seconds).round().max(1.0) as usize;
    let mut out: Vec<Packet> = Vec::new();
    let mut start = Instant::now();
    for i in 0..warm + timed {
        if i == warm {
            start = Instant::now();
        }
        session.encode(&frames[i % frames.len()], i as i64, &mut out).map_err(|e| e.to_string())?;
        out.clear();
    }
    session.flush(&mut out).map_err(|e| e.to_string())?;
    let wall = start.elapsed().as_secs_f64();
    let coded_per_s = timed as f64 / wall;
    Ok((coded_per_s, 100.0 * f64::from(fps) / coded_per_s))
}

fn main() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let seconds = match args.next() {
        Some(s) => {
            s.parse::<f64>().ok().filter(|s| (0.1..=600.0).contains(s)).ok_or_else(|| format!("`{s}` is not a length from 0.1 to 600 seconds"))?
        }
        None => 3.0,
    };
    let threads = match args.next() {
        Some(s) => {
            Some(s.parse::<usize>().ok().filter(|t| (1..=1024).contains(t)).ok_or_else(|| format!("`{s}` is not a thread count from 1 to 1024"))?)
        }
        None => None,
    };
    let hardware = std::thread::available_parallelism().map_or(1, |n| n.get());
    let used = threads.unwrap_or_else(|| hardware.saturating_sub(2).max(1));
    println!("{hardware} hardware threads; the encoder uses {used}");
    println!("| Size | Frame rate | Preset | Frames coded per second | Share of real time |");
    println!("| --- | --- | --- | --- | --- |");
    for (width, height) in [(1280, 720), (1920, 1080)] {
        for fps in [30, 60] {
            for preset in ["speed", "balanced", "quality"] {
                let (rate, share) = run(width, height, fps, preset, seconds, threads)?;
                println!("| {width}×{height} | {fps} | {preset} | {rate:.1} | {share:.0}% |");
            }
        }
    }
    Ok(())
}
