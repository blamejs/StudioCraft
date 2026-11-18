//! How long to wait before opening a failed capture again.

use std::time::Duration;

/// The waits after each failure in a row, in milliseconds; the last one repeats.
const STEPS_MS: [u64; 5] = [250, 500, 1_000, 2_000, 5_000];

/// Waits that grow with each failure in a row: 250 ms, 500 ms, 1 s, 2 s, then 5 s.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Backoff {
    failures: usize,
}

impl Backoff {
    /// The wait after one more failure.
    pub fn fail(&mut self) -> Duration {
        let ms = STEPS_MS.get(self.failures).or(STEPS_MS.last()).copied().unwrap_or(5_000);
        self.failures = self.failures.saturating_add(1);
        Duration::from_millis(ms)
    }

    /// The capture works again: the next failure waits the shortest time.
    pub fn reset(&mut self) {
        self.failures = 0;
    }

    /// Failures in a row so far.
    pub fn failures(&self) -> usize {
        self.failures
    }
}
