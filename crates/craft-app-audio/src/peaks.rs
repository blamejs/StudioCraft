//! Peak levels of the listed applications, written by the discovery thread and read by any thread
//! without a lock.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Rows in the peak table: the applications past the 64th are listed without a meter.
pub const PEAK_SLOTS: usize = 64;
/// A level is current for this long after the meter reading that wrote it. Meters are read every
/// 33 ms while a caller asks for them, so a row older than this was written before the reading
/// stopped.
pub const PEAK_FRESH_MS: u64 = 250;

struct Slot {
    /// The [`crate::AppKey::id`] of the application in this row, 0 when the row is free.
    key: AtomicU64,
    /// The peak level as `f32` bits.
    peak: AtomicU32,
    /// When the level was read ([`crate::now_ms`] plus 1), 0 before the first reading.
    stamp: AtomicU64,
}

/// A fixed table of peak levels, one row per listed application. It never allocates after it is
/// made, and a reader takes no lock.
pub struct PeakTable {
    slots: [Slot; PEAK_SLOTS],
}

impl Default for PeakTable {
    fn default() -> Self {
        Self { slots: std::array::from_fn(|_| Slot { key: AtomicU64::new(0), peak: AtomicU32::new(0), stamp: AtomicU64::new(0) }) }
    }
}

impl PeakTable {
    /// Give row `slot` to the application with key id `id`, with no reading yet.
    pub(crate) fn assign(&self, slot: usize, id: u64) {
        if let Some(s) = self.slots.get(slot) {
            s.stamp.store(0, Ordering::Relaxed);
            s.peak.store(0, Ordering::Relaxed);
            s.key.store(id, Ordering::Release);
        }
    }

    /// Free row `slot`.
    pub(crate) fn free(&self, slot: usize) {
        if let Some(s) = self.slots.get(slot) {
            s.key.store(0, Ordering::Release);
            s.stamp.store(0, Ordering::Relaxed);
            s.peak.store(0, Ordering::Relaxed);
        }
    }

    /// Leave row `slot` with no reading, keeping its application: the meter gave none.
    pub(crate) fn clear(&self, slot: usize) {
        if let Some(s) = self.slots.get(slot) {
            s.stamp.store(0, Ordering::Release);
            s.peak.store(0, Ordering::Relaxed);
        }
    }

    /// Record `peak` (clamped to 0 to 1) in row `slot`, read at `now` ([`crate::now_ms`]).
    pub(crate) fn set(&self, slot: usize, peak: f32, now: u64) {
        if let Some(s) = self.slots.get(slot) {
            let p = if peak.is_finite() { peak.clamp(0.0, 1.0) } else { 0.0 };
            s.peak.store(p.to_bits(), Ordering::Relaxed);
            // Released, so a reader that sees this stamp also sees the level and the key the row
            // had then.
            s.stamp.store(now.saturating_add(1), Ordering::Release);
        }
    }

    /// The peak level in row `slot` at `now`, when that row belongs to the key with id `id`
    /// before and after the level is read and the level was read within [`PEAK_FRESH_MS`].
    /// `None` for a row given to another application, a row with no reading yet, and a level
    /// left from a reading that has stopped.
    pub fn get(&self, slot: u8, id: u64, now: u64) -> Option<f32> {
        let s = self.slots.get(usize::from(slot))?;
        if id == 0 || s.key.load(Ordering::Acquire) != id {
            return None;
        }
        let stamp = s.stamp.load(Ordering::Acquire);
        let peak = f32::from_bits(s.peak.load(Ordering::Relaxed));
        let fresh = stamp != 0 && now.saturating_add(1).saturating_sub(stamp) <= PEAK_FRESH_MS;
        (fresh && s.key.load(Ordering::Acquire) == id && s.stamp.load(Ordering::Acquire) == stamp).then_some(peak)
    }
}
