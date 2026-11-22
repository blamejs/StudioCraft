//! Pipeline counters. Every thread writes its own counters with relaxed atomics; the stats window
//! reads a [`Snapshot`] once per frame.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
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
    /// Nanoseconds of long gaps left out of the output so far (the computer was suspended).
    pub skipped_ns: AtomicU64,
    /// One entry for each gap left out: the studio clock's nanoseconds from the output's start to
    /// the first input after the gap, and [`EncoderStats::skipped_ns`] from then on.
    pub skips: Mutex<Vec<(u64, u64)>>,
}

impl EncoderStats {
    /// Note that `total_ns` are left out of the output since the input `at_ns` after its start.
    /// A writer calls it when the total it sees changes. A gap that both writers see is noted
    /// once, at the earlier of the two inputs.
    pub fn note_skipped(&self, at_ns: u64, total_ns: u64) {
        self.skipped_ns.fetch_max(total_ns, Ordering::Relaxed);
        if total_ns == 0 {
            return;
        }
        let mut skips = self.skips.lock().unwrap_or_else(PoisonError::into_inner);
        match skips.iter_mut().find(|(_, total)| *total == total_ns) {
            Some((at, _)) => *at = (*at).min(at_ns),
            None => skips.push((at_ns, total_ns)),
        }
    }

    /// The nanoseconds left out of the output before `at_ns` after its start on the studio clock,
    /// so the time `at_ns` lies this much earlier in the output. Each gap is the stretch it left
    /// out just before the input after it; a time inside that stretch counts the part of the gap
    /// before it, which puts it where the output goes on. A gap the writers have not reached yet
    /// is not counted.
    pub fn skipped_before(&self, at_ns: u64) -> u64 {
        let mut skips = self.skips.lock().unwrap_or_else(PoisonError::into_inner).clone();
        skips.sort_unstable_by_key(|&(_, total)| total);
        let mut before = 0;
        let mut counted = 0;
        for (end, total) in skips {
            let gap = total - counted;
            counted = total;
            before += at_ns.saturating_sub(end.saturating_sub(gap)).min(gap);
        }
        before
    }
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

    #[test]
    fn each_gap_is_noted_once_and_counts_from_where_it_ends() {
        let e = EncoderStats::default();
        e.note_skipped(5, 0);
        // A later gap noted first by one writer, then the earlier gap by the other writer.
        e.note_skipped(100, 25);
        e.note_skipped(30, 10);
        // The other writer sees the same gaps: one later, one at an input placed earlier.
        e.note_skipped(31, 10);
        e.note_skipped(90, 25);
        assert_eq!(e.skips.lock().unwrap().as_slice(), [(90, 25), (30, 10)]);
        assert_eq!(e.skipped_ns.load(Ordering::Relaxed), 25);
        // The first gap left out 20 to 30 and the second 75 to 90.
        assert_eq!([0, 20, 25, 30, 75, 80, 90, 1000].map(|at| e.skipped_before(at)), [0, 0, 5, 10, 10, 15, 25, 25]);
    }
}
