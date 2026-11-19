//! Peak levels of the listed applications, written by the discovery thread and read by any thread
//! without a lock.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::AppKey;

/// Rows in the peak table: the applications past the 64th are listed without a meter.
pub const PEAK_SLOTS: usize = 64;

struct Slot {
    /// The [`AppKey::id`] of the application in this row, 0 when the row is free.
    key: AtomicU64,
    /// The peak level as `f32` bits.
    peak: AtomicU32,
}

/// A fixed table of peak levels, one row per listed application. It never allocates after it is
/// made, and a reader takes no lock.
pub struct PeakTable {
    slots: [Slot; PEAK_SLOTS],
}

impl Default for PeakTable {
    fn default() -> Self {
        Self { slots: std::array::from_fn(|_| Slot { key: AtomicU64::new(0), peak: AtomicU32::new(0) }) }
    }
}

impl PeakTable {
    /// Give row `slot` to `key`, with no reading yet.
    pub(crate) fn assign(&self, slot: usize, key: &AppKey) {
        if let Some(s) = self.slots.get(slot) {
            s.peak.store(0, Ordering::Relaxed);
            s.key.store(key.id(), Ordering::Release);
        }
    }

    /// Free row `slot`.
    pub(crate) fn free(&self, slot: usize) {
        if let Some(s) = self.slots.get(slot) {
            s.key.store(0, Ordering::Release);
            s.peak.store(0, Ordering::Relaxed);
        }
    }

    /// Record `peak` (clamped to 0 to 1) in row `slot`.
    pub(crate) fn set(&self, slot: usize, peak: f32) {
        if let Some(s) = self.slots.get(slot) {
            let p = if peak.is_finite() { peak.clamp(0.0, 1.0) } else { 0.0 };
            // Released, so a reader that sees this level also sees the key the row had then.
            s.peak.store(p.to_bits(), Ordering::Release);
        }
    }

    /// The peak level in row `slot`, when that row belongs to `key` both before and after the
    /// level is read, so a row given to another application meanwhile reads `None`.
    pub fn get(&self, slot: u8, key: &AppKey) -> Option<f32> {
        let s = self.slots.get(usize::from(slot))?;
        let id = key.id();
        if s.key.load(Ordering::Acquire) != id {
            return None;
        }
        let peak = f32::from_bits(s.peak.load(Ordering::Acquire));
        (s.key.load(Ordering::Acquire) == id).then_some(peak)
    }
}
