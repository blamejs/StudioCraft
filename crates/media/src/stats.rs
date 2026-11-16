//! Pipeline counters. Every thread writes its own counters with relaxed atomics; the stats window
//! reads a [`Snapshot`] once per frame.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Counters of the compositor thread.
#[derive(Debug, Default)]
pub struct RenderStats {
    /// Frames composited.
    pub frames: AtomicU64,
    /// Frames skipped because compositing could not keep up with the frame rate.
    pub lagged: AtomicU64,
    /// Time spent compositing the last frame, in nanoseconds.
    pub last_ns: AtomicU64,
    /// Smoothed compositing time, in nanoseconds.
    pub avg_ns: AtomicU64,
}

impl RenderStats {
    /// Record one composited frame that took `took`, after `skipped` missed frames.
    pub fn record(&self, took: Duration, skipped: u64) {
        let ns = u64::try_from(took.as_nanos()).unwrap_or(u64::MAX);
        self.frames.fetch_add(1, Ordering::Relaxed);
        self.lagged.fetch_add(skipped, Ordering::Relaxed);
        self.last_ns.store(ns, Ordering::Relaxed);
        let avg = self.avg_ns.load(Ordering::Relaxed);
        // Exponential moving average over about 32 frames.
        let next = if avg == 0 { ns } else { avg - avg / 32 + ns / 32 };
        self.avg_ns.store(next, Ordering::Relaxed);
    }
}

/// Counters of one encoder (one recording or stream).
#[derive(Debug, Default)]
pub struct EncoderStats {
    /// Frames handed to the encoder.
    pub frames: AtomicU64,
    /// Frames dropped because the encoder fell behind.
    pub dropped: AtomicU64,
    /// Frames waiting for the encoder right now.
    pub queued: AtomicU64,
    /// Bytes the encoder has written.
    pub bytes: AtomicU64,
}

/// What the stats window shows.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub frames: u64,
    pub lagged: u64,
    pub render_ms: f64,
    pub render_avg_ms: f64,
    pub encoder_frames: u64,
    pub encoder_dropped: u64,
    pub encoder_queued: u64,
}

impl Snapshot {
    pub fn read(render: &RenderStats, encoders: &[&EncoderStats]) -> Snapshot {
        let ms = |ns: u64| ns as f64 / 1e6;
        Snapshot {
            frames: render.frames.load(Ordering::Relaxed),
            lagged: render.lagged.load(Ordering::Relaxed),
            render_ms: ms(render.last_ns.load(Ordering::Relaxed)),
            render_avg_ms: ms(render.avg_ns.load(Ordering::Relaxed)),
            encoder_frames: encoders.iter().map(|e| e.frames.load(Ordering::Relaxed)).sum(),
            encoder_dropped: encoders.iter().map(|e| e.dropped.load(Ordering::Relaxed)).sum(),
            encoder_queued: encoders.iter().map(|e| e.queued.load(Ordering::Relaxed)).max().unwrap_or(0),
        }
    }

    /// Share of frames lost to lag, 0 to 1.
    pub fn lag_ratio(&self) -> f64 {
        let total = self.frames + self.lagged;
        if total == 0 { 0.0 } else { self.lagged as f64 / total as f64 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_averages() {
        let r = RenderStats::default();
        r.record(Duration::from_millis(4), 0);
        r.record(Duration::from_millis(4), 2);
        let s = Snapshot::read(&r, &[]);
        assert_eq!((s.frames, s.lagged), (2, 2));
        assert!((s.render_avg_ms - 4.0).abs() < 0.01);
        assert!((s.lag_ratio() - 0.5).abs() < 1e-9);
    }
}
