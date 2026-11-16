//! Audio in the pipeline: 48 kHz stereo `f32`, moved in blocks of [`BLOCK_FRAMES`] frames.
//! Capture devices deliver whatever chunk sizes, rates and channel counts they like; a
//! [`Reblocker`] turns that into pipeline blocks.

use std::sync::Arc;

use craft_time::{FrameRate, Tick};

use crate::pool::{Pool, Pooled};

/// Sample rate of the mix.
pub const SAMPLE_RATE: u32 = 48_000;
/// Channels of the mix (left, right).
pub const CHANNELS: usize = 2;
/// Sample frames per block (21.3 ms at 48 kHz).
pub const BLOCK_FRAMES: usize = 1024;
/// Interleaved samples per block.
pub const BLOCK_SAMPLES: usize = BLOCK_FRAMES * CHANNELS;

/// Blocks per second as a rate, for pacing the mixer.
pub fn block_rate() -> FrameRate {
    FrameRate::new(i64::from(SAMPLE_RATE), BLOCK_FRAMES as i64)
}

/// One block of interleaved stereo samples.
#[derive(Clone, Debug, Default)]
pub struct AudioBlock {
    /// [`BLOCK_SAMPLES`] samples once filled.
    pub samples: Vec<f32>,
    pub pts: Tick,
}

impl AudioBlock {
    /// Make the block full length and silent, reusing its buffer.
    pub fn clear(&mut self) {
        self.samples.clear();
        self.samples.resize(BLOCK_SAMPLES, 0.0);
    }
}

pub type AudioPool = Pool<AudioBlock>;
pub type PooledBlock = Pooled<AudioBlock>;

/// A pool of blocks for one source or output (about a second's worth may be in flight).
pub fn block_pool() -> Arc<AudioPool> {
    Pool::new(64, || AudioBlock { samples: vec![0.0; BLOCK_SAMPLES], pts: Tick(0) })
}

/// Turns device chunks of any size, channel count and rate into 48 kHz stereo blocks.
///
/// Rate conversion is linear interpolation: enough for speech and monitoring, below what a
/// mastering resampler does.
pub struct Reblocker {
    in_rate: u32,
    in_channels: usize,
    /// Position of the next output sample between input frames `prev` and `cur`.
    phase: f64,
    step: f64,
    prev: [f32; 2],
    cur: [f32; 2],
    have_cur: bool,
    block: Vec<f32>,
}

impl Reblocker {
    /// For a device at `in_rate` Hz with `in_channels` channels (both at least 1).
    pub fn new(in_rate: u32, in_channels: usize) -> Self {
        let in_rate = in_rate.max(1);
        Self {
            in_rate,
            in_channels: in_channels.max(1),
            phase: 0.0,
            step: f64::from(in_rate) / f64::from(SAMPLE_RATE),
            prev: [0.0; 2],
            cur: [0.0; 2],
            have_cur: false,
            block: Vec::with_capacity(BLOCK_SAMPLES),
        }
    }

    pub fn input_rate(&self) -> u32 {
        self.in_rate
    }

    /// Feed interleaved device samples; `emit` gets each completed block (a slice of
    /// [`BLOCK_SAMPLES`] samples).
    pub fn push(&mut self, input: &[f32], mut emit: impl FnMut(&[f32])) {
        let ch = self.in_channels;
        for frame in input.chunks_exact(ch) {
            let l = frame.first().copied().unwrap_or(0.0);
            let r = if ch >= 2 { frame.get(1).copied().unwrap_or(l) } else { l };
            let next = [sanitize(l), sanitize(r)];
            if !self.have_cur {
                self.cur = next;
                self.prev = next;
                self.have_cur = true;
                continue;
            }
            self.prev = self.cur;
            self.cur = next;
            // Emit every output sample that falls between prev (0) and cur (1).
            while self.phase < 1.0 {
                let t = self.phase as f32;
                self.block.push(self.prev[0] + (self.cur[0] - self.prev[0]) * t);
                self.block.push(self.prev[1] + (self.cur[1] - self.prev[1]) * t);
                if self.block.len() >= BLOCK_SAMPLES {
                    emit(&self.block);
                    self.block.clear();
                }
                self.phase += self.step;
            }
            self.phase -= 1.0;
        }
    }
}

/// Non-finite samples become silence; others are kept within -1..1 (with headroom up to 4 so a
/// hot input is clipped by the meter, not here).
fn sanitize(s: f32) -> f32 {
    if s.is_finite() { s.clamp(-4.0, 4.0) } else { 0.0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocks(r: &mut Reblocker, input: &[f32]) -> Vec<Vec<f32>> {
        let mut out = Vec::new();
        r.push(input, |b| out.push(b.to_vec()));
        out
    }

    #[test]
    fn same_rate_stereo_passes_through() {
        let mut r = Reblocker::new(48_000, 2);
        let input: Vec<f32> = (0..(BLOCK_SAMPLES * 2 + 2)).map(|i| (i % 7) as f32 * 0.1).collect();
        let out = blocks(&mut r, &input);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].len(), BLOCK_SAMPLES);
        assert_eq!(&out[0][..6], &input[..6]);
    }

    #[test]
    fn mono_is_copied_to_both_channels() {
        let mut r = Reblocker::new(48_000, 1);
        let input: Vec<f32> = (0..(BLOCK_FRAMES + 1)).map(|i| i as f32 / 2048.0).collect();
        let out = blocks(&mut r, &input);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0][10], out[0][11]);
    }

    #[test]
    fn resampling_keeps_duration() {
        // One second at 44.1 kHz makes about one second at 48 kHz.
        let mut r = Reblocker::new(44_100, 2);
        let input = vec![0.25f32; 44_100 * 2];
        let out = blocks(&mut r, &input);
        let frames = out.len() * BLOCK_FRAMES;
        assert!((47_000..=48_000).contains(&frames), "{frames}");
        assert!(out.iter().flatten().all(|s| (*s - 0.25).abs() < 1e-6));
    }

    #[test]
    fn many_channels_take_the_first_two_and_bad_samples_are_silenced() {
        let mut r = Reblocker::new(48_000, 6);
        let mut input = Vec::new();
        for _ in 0..=BLOCK_FRAMES {
            input.extend_from_slice(&[0.5, f32::NAN, 0.9, 0.9, 0.9, 0.9]);
        }
        let out = blocks(&mut r, &input);
        assert_eq!(out[0][0], 0.5);
        assert_eq!(out[0][1], 0.0);
    }
}
