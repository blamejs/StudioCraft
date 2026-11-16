//! Mixing and level metering.
//!
//! Levels are published through atomics ([`Levels`]) so the interface reads them every frame
//! without locking the mixer thread.

use std::sync::atomic::{AtomicU32, Ordering};

use crate::audio::{BLOCK_SAMPLES, CHANNELS};

/// Lowest level the meters show, in dBFS.
pub const METER_FLOOR_DB: f32 = -60.0;

/// Linear amplitude to dBFS ([`f32::NEG_INFINITY`] for silence).
pub fn to_db(lin: f32) -> f32 {
    if lin > 0.0 && lin.is_finite() { 20.0 * lin.log10() } else { f32::NEG_INFINITY }
}

/// dBFS to linear amplitude.
pub fn from_db(db: f32) -> f32 {
    if db.is_finite() { 10f32.powf(db / 20.0) } else { 0.0 }
}

/// Peak and RMS of each channel of one block, linear.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BlockLevels {
    pub peak: [f32; CHANNELS],
    pub rms: [f32; CHANNELS],
}

/// Add `input * gain` into `out` (both interleaved stereo blocks) and return the levels of what
/// was added.
pub fn mix_into(out: &mut [f32], input: &[f32], gain: f32) -> BlockLevels {
    let gain = if gain.is_finite() { gain.max(0.0) } else { 0.0 };
    let mut peak = [0f32; CHANNELS];
    let mut sum = [0f32; CHANNELS];
    for (frame_out, frame_in) in out.as_chunks_mut::<CHANNELS>().0.iter_mut().zip(input.as_chunks::<CHANNELS>().0) {
        for c in 0..CHANNELS {
            let s = frame_in[c] * gain;
            frame_out[c] += s;
            peak[c] = peak[c].max(s.abs());
            sum[c] += s * s;
        }
    }
    let frames = (input.len().min(out.len()) / CHANNELS).max(1) as f32;
    BlockLevels { peak, rms: sum.map(|s| (s / frames).sqrt()) }
}

/// The levels of `block` without mixing it anywhere.
pub fn measure(block: &[f32], gain: f32) -> BlockLevels {
    let mut scratch = [0f32; BLOCK_SAMPLES];
    let n = block.len().min(BLOCK_SAMPLES);
    mix_into(&mut scratch[..n], &block[..n], gain)
}

/// Keep every sample of `block` within -1..1 (hard clip of the final mix).
pub fn clip(block: &mut [f32]) {
    for s in block {
        *s = if s.is_finite() { s.clamp(-1.0, 1.0) } else { 0.0 };
    }
}

/// The latest levels of one meter, written by the mixer and read by the interface.
#[derive(Debug, Default)]
pub struct Levels {
    peak: [AtomicU32; CHANNELS],
    rms: [AtomicU32; CHANNELS],
}

impl Levels {
    pub fn store(&self, l: BlockLevels) {
        for c in 0..CHANNELS {
            self.peak[c].store(l.peak[c].to_bits(), Ordering::Relaxed);
            self.rms[c].store(l.rms[c].to_bits(), Ordering::Relaxed);
        }
    }

    pub fn load(&self) -> BlockLevels {
        let mut l = BlockLevels::default();
        for c in 0..CHANNELS {
            l.peak[c] = f32::from_bits(self.peak[c].load(Ordering::Relaxed));
            l.rms[c] = f32::from_bits(self.rms[c].load(Ordering::Relaxed));
        }
        l
    }
}

/// A meter's display state: the level falls back at a fixed rate and the peak is held for a
/// while, as on hardware meters.
#[derive(Clone, Copy, Debug)]
pub struct MeterBallistics {
    pub level_db: [f32; CHANNELS],
    pub rms_db: [f32; CHANNELS],
    pub hold_db: [f32; CHANNELS],
    hold_age: [f32; CHANNELS],
}

impl Default for MeterBallistics {
    fn default() -> Self {
        Self {
            level_db: [METER_FLOOR_DB; CHANNELS],
            rms_db: [METER_FLOOR_DB; CHANNELS],
            hold_db: [METER_FLOOR_DB; CHANNELS],
            hold_age: [0.0; CHANNELS],
        }
    }
}

/// How fast a meter falls, in dB per second.
pub const METER_FALL_DB_PER_S: f32 = 24.0;
/// How long the peak mark stays before it falls, in seconds.
pub const PEAK_HOLD_S: f32 = 1.5;

impl MeterBallistics {
    /// Advance by `dt` seconds with new levels `l`.
    pub fn update(&mut self, l: BlockLevels, dt: f32) {
        let dt = if dt.is_finite() { dt.clamp(0.0, 1.0) } else { 0.0 };
        for c in 0..CHANNELS {
            let peak = to_db(l.peak[c]).max(METER_FLOOR_DB);
            let rms = to_db(l.rms[c]).max(METER_FLOOR_DB);
            self.level_db[c] = peak.max(self.level_db[c] - METER_FALL_DB_PER_S * dt);
            self.rms_db[c] = rms.max(self.rms_db[c] - METER_FALL_DB_PER_S * dt);
            if peak >= self.hold_db[c] {
                self.hold_db[c] = peak;
                self.hold_age[c] = 0.0;
            } else {
                self.hold_age[c] += dt;
                if self.hold_age[c] > PEAK_HOLD_S {
                    self.hold_db[c] = (self.hold_db[c] - METER_FALL_DB_PER_S * dt).max(METER_FLOOR_DB);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_conversions() {
        assert!((to_db(1.0)).abs() < 1e-6);
        assert!((to_db(0.5) + 6.0206).abs() < 1e-3);
        assert_eq!(to_db(0.0), f32::NEG_INFINITY);
        assert!((from_db(-6.0206) - 0.5).abs() < 1e-4);
        assert_eq!(from_db(f32::NEG_INFINITY), 0.0);
    }

    #[test]
    fn mixing_adds_with_gain_and_measures() {
        let mut out = vec![0.1f32; BLOCK_SAMPLES];
        let input = vec![0.5f32; BLOCK_SAMPLES];
        let l = mix_into(&mut out, &input, 0.5);
        assert!((out[0] - 0.35).abs() < 1e-6);
        assert!((l.peak[0] - 0.25).abs() < 1e-6);
        assert!((l.rms[1] - 0.25).abs() < 1e-6);
        let none = mix_into(&mut out, &input, f32::NAN);
        assert_eq!(none.peak, [0.0, 0.0]);
    }

    #[test]
    fn levels_round_trip_through_atomics() {
        let lv = Levels::default();
        let l = BlockLevels { peak: [0.5, 0.25], rms: [0.1, 0.2] };
        lv.store(l);
        assert_eq!(lv.load(), l);
    }

    #[test]
    fn ballistics_fall_and_hold() {
        let mut m = MeterBallistics::default();
        m.update(BlockLevels { peak: [1.0, 1.0], rms: [0.5, 0.5] }, 0.02);
        assert!(m.level_db[0].abs() < 1e-3);
        m.update(BlockLevels::default(), 0.5);
        assert!((m.level_db[0] + 12.0).abs() < 1e-3, "{}", m.level_db[0]);
        assert!(m.hold_db[0].abs() < 1e-3, "held");
        m.update(BlockLevels::default(), 0.6);
        assert!(m.hold_db[0].abs() < 1e-3, "still held after 1.1 s");
        m.update(BlockLevels::default(), 0.6);
        assert!(m.hold_db[0] < 0.0, "falls after the hold time");
    }

    #[test]
    fn clipping_bounds_the_mix() {
        let mut b = vec![2.0, -3.0, f32::NAN, 0.5];
        clip(&mut b);
        assert_eq!(b, vec![1.0, -1.0, 0.0, 0.5]);
    }
}
