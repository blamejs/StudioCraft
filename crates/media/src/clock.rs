//! The master clock and frame pacing.
//!
//! One [`MasterClock`] is created when the pipeline starts. The compositor, the audio mixer and
//! every source stamp what they produce with [`MasterClock::now`], so audio and video in a
//! recording line up. A [`Pacer`] wakes a thread at a fixed rate on that clock: the compositor at
//! the frame rate, the mixer once per audio block.

use std::time::{Duration, Instant};

use craft_time::{FrameRate, Tick};

/// Monotonic time since the pipeline started.
#[derive(Clone, Copy, Debug)]
pub struct MasterClock {
    start: Instant,
}

impl Default for MasterClock {
    fn default() -> Self {
        Self::new()
    }
}

impl MasterClock {
    pub fn new() -> Self {
        Self { start: Instant::now() }
    }

    /// Time since start.
    pub fn now(&self) -> Tick {
        tick_of(self.start.elapsed())
    }

    pub fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }

    /// The instant `t` after start (negative times are the start).
    pub fn instant_of(&self, t: Tick) -> Instant {
        self.start + duration_of(t)
    }
}

/// A duration as a tick count.
pub fn tick_of(d: Duration) -> Tick {
    Tick::from_units(i64::try_from(d.as_nanos()).unwrap_or(i64::MAX), 1_000_000_000)
}

/// A tick count as a duration (negative is zero).
pub fn duration_of(t: Tick) -> Duration {
    Duration::from_nanos(u64::try_from(t.to_units_floor(1_000_000_000)).unwrap_or(0))
}

/// One wake-up of a [`Pacer`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaceTick {
    /// Index of this frame (or audio block) since the clock started.
    pub index: i64,
    /// Its presentation time.
    pub pts: Tick,
    /// Frames skipped because the thread woke too late for them.
    pub skipped: u64,
}

/// Wakes a thread once per period of `rate` on the master clock.
pub struct Pacer {
    clock: MasterClock,
    rate: FrameRate,
    next: i64,
}

/// How long before a deadline the pacer stops sleeping and yields instead, so timer coarseness
/// (about 1 ms on Windows with a high-resolution timer, up to 15.6 ms without) does not make
/// frames late.
const SPIN: Duration = Duration::from_micros(1500);

impl Pacer {
    /// A pacer whose first tick is the next period boundary after now.
    pub fn new(clock: MasterClock, rate: FrameRate) -> Self {
        let rate = rate.sane();
        let next = rate.frame_at(clock.now()).saturating_add(1);
        Self { clock, rate, next }
    }

    pub fn rate(&self) -> FrameRate {
        self.rate
    }

    /// The tick that is due next, without waiting.
    pub fn peek(&self) -> PaceTick {
        PaceTick { index: self.next, pts: self.rate.tick_of(self.next), skipped: 0 }
    }

    /// Advance to the next tick without waiting; when the clock is already past it, ticks that
    /// can no longer be met are skipped and counted.
    pub fn advance(&mut self) -> PaceTick {
        let now_frame = self.rate.frame_at(self.clock.now());
        let mut skipped = 0;
        if now_frame > self.next {
            skipped = u64::try_from(now_frame - self.next).unwrap_or(0);
            self.next = now_frame;
        }
        let tick = PaceTick { index: self.next, pts: self.rate.tick_of(self.next), skipped };
        self.next = self.next.saturating_add(1);
        tick
    }

    /// Sleep until the next tick is due, then return it.
    pub fn wait(&mut self) -> PaceTick {
        let due = self.clock.instant_of(self.rate.tick_of(self.next));
        loop {
            let now = Instant::now();
            if now >= due {
                break;
            }
            let left = due - now;
            if left > SPIN {
                std::thread::sleep(left - SPIN);
            } else {
                std::thread::yield_now();
            }
        }
        self.advance()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_and_duration_round_trip() {
        let d = Duration::from_millis(1234);
        assert_eq!(duration_of(tick_of(d)), d);
        assert_eq!(duration_of(Tick(-5)), Duration::ZERO);
    }

    #[test]
    fn pacer_ticks_at_the_rate() {
        let clock = MasterClock::new();
        let mut p = Pacer::new(clock, FrameRate::new(100, 1));
        let start = Instant::now();
        let first = p.wait();
        let mut last = first;
        for _ in 0..10 {
            last = p.wait();
        }
        assert_eq!(last.index - first.index, 10 + i64::try_from(last.skipped).unwrap_or(0));
        // 10 periods of 10 ms, allowing for a slow test machine.
        let took = start.elapsed();
        assert!(took >= Duration::from_millis(90), "{took:?}");
    }

    #[test]
    fn a_late_pacer_skips_and_counts() {
        let clock = MasterClock::new();
        let mut p = Pacer::new(clock, FrameRate::new(1000, 1));
        std::thread::sleep(Duration::from_millis(20));
        let t = p.advance();
        assert!(t.skipped >= 10, "{t:?}");
        let next = p.advance();
        assert_eq!(next.index, t.index + 1);
    }
}
