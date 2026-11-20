//! Audio at any rate and channel count, down to the mono 16 kHz the recognizers take.
//!
//! The channels are averaged. When the rate is a whole multiple of 16 kHz (32, 48 or 96 kHz), a
//! windowed-sinc low-pass at 7.2 kHz (Blackman window, 16 taps per output sample) runs before every
//! `n`th sample is kept. Any other rate is averaged over each output sample's span of input and
//! linearly interpolated. The output is the same however the input is cut into pieces.

use crate::SAMPLE_RATE;

/// Most input channels read; samples of channels past it are skipped.
pub const MAX_CHANNELS: usize = 64;
/// Highest input rate accepted (768 kHz).
pub const MAX_RATE: u32 = 768_000;

enum Mode {
    /// Keep every `factor`th sample after the low-pass `taps`.
    Decimate { factor: usize, taps: Vec<f32>, phase: usize },
    /// Average and interpolate: `pos_num` is the next output sample's place in input samples
    /// times 16000, `read` counts the averaged input samples so far, and `last` is the latest.
    Interpolate { step_num: u64, pos_num: u64, read: u64, last: f32 },
}

/// A streaming converter to mono 16 kHz.
pub struct Downsampler {
    channels: usize,
    mode: Mode,
    /// Mono input kept for the filter: the last `taps.len() - 1` samples.
    history: Vec<f32>,
    /// A partial frame of interleaved input carried to the next call.
    carry: Vec<f32>,
}

impl Downsampler {
    /// A converter from `rate` Hz with `channels` interleaved channels. `rate` is clamped to
    /// 16 kHz ..= [`MAX_RATE`] and `channels` to 1 ..= [`MAX_CHANNELS`].
    pub fn new(rate: u32, channels: usize) -> Downsampler {
        let rate = rate.clamp(SAMPLE_RATE, MAX_RATE);
        let channels = channels.clamp(1, MAX_CHANNELS);
        let mode = if rate.is_multiple_of(SAMPLE_RATE) {
            let factor = (rate / SAMPLE_RATE) as usize;
            Mode::Decimate { factor, taps: lowpass(factor), phase: 0 }
        } else {
            Mode::Interpolate { step_num: u64::from(rate), pos_num: 0, read: 0, last: 0.0 }
        };
        Downsampler { channels, mode, history: Vec::new(), carry: Vec::new() }
    }

    /// Convert interleaved `input`, appending the 16 kHz samples to `out`.
    pub fn push(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let c = self.channels;
        let mut mono = Vec::with_capacity(input.len() / c + 1);
        let clean = |v: f32| if v.is_finite() { v } else { 0.0 };
        let mut rest = input;
        if !self.carry.is_empty() {
            let need = c - self.carry.len();
            let (head, tail) = rest.split_at(need.min(rest.len()));
            self.carry.extend_from_slice(head);
            rest = tail;
            if self.carry.len() < c {
                return;
            }
            mono.push(self.carry.iter().copied().map(clean).sum::<f32>() / c as f32);
            self.carry.clear();
        }
        let mut frames = rest.chunks_exact(c);
        for f in &mut frames {
            mono.push(f.iter().copied().map(clean).sum::<f32>() / c as f32);
        }
        self.carry.extend_from_slice(frames.remainder());
        match &mut self.mode {
            Mode::Decimate { factor, taps, phase } => {
                let keep = taps.len().saturating_sub(1);
                if self.history.is_empty() {
                    self.history = vec![0.0; keep];
                }
                self.history.extend_from_slice(&mono);
                // Sample `i` of the history (past the first `keep`) is input sample number
                // `consumed + i - keep`; an output falls on every `factor`th input sample.
                let mut i = keep + *phase;
                while i < self.history.len() {
                    let window = &self.history[i - keep..=i];
                    out.push(window.iter().zip(taps.iter()).map(|(a, b)| a * b).sum());
                    i += *factor;
                }
                *phase = i - self.history.len();
                let drop = self.history.len() - keep;
                self.history.drain(..drop);
            }
            Mode::Interpolate { step_num, pos_num, read, last } => {
                // Positions are in units of 1/16000 input samples.
                let out_rate = u64::from(SAMPLE_RATE);
                let span = (*step_num).div_ceil(out_rate).max(1) as usize;
                for &x in &mono {
                    // A moving average over one output sample's span of input.
                    self.history.push(x);
                    if self.history.len() > span {
                        self.history.remove(0);
                    }
                    let avg = self.history.iter().sum::<f32>() / self.history.len() as f32;
                    // Averaged sample `read` has arrived: every output whose place lies between
                    // the sample before it and this one can be interpolated now.
                    if *read > 0 {
                        let before = (*read - 1).saturating_mul(out_rate);
                        while *pos_num < read.saturating_mul(out_rate) {
                            let frac = pos_num.saturating_sub(before) as f32 / out_rate as f32;
                            out.push(*last + (avg - *last) * frac.clamp(0.0, 1.0));
                            *pos_num = pos_num.saturating_add(*step_num);
                        }
                    }
                    *last = avg;
                    *read += 1;
                }
            }
        }
    }
}

/// A windowed-sinc low-pass for decimation by `factor`: cutoff at 0.45 of the output rate,
/// Blackman window, unity gain at 0 Hz.
fn lowpass(factor: usize) -> Vec<f32> {
    if factor <= 1 {
        return vec![1.0];
    }
    let n = 16 * factor + 1;
    let fc = 0.45 / factor as f64;
    let mid = (n - 1) as f64 / 2.0;
    let mut taps: Vec<f64> = (0..n)
        .map(|i| {
            let x = i as f64 - mid;
            let sinc = if x == 0.0 { 2.0 * fc } else { (2.0 * std::f64::consts::PI * fc * x).sin() / (std::f64::consts::PI * x) };
            let t = i as f64 / (n - 1) as f64;
            let w = 0.42 - 0.5 * (2.0 * std::f64::consts::PI * t).cos() + 0.08 * (4.0 * std::f64::consts::PI * t).cos();
            sinc * w
        })
        .collect();
    let sum: f64 = taps.iter().sum();
    if sum.abs() > f64::EPSILON {
        taps.iter_mut().for_each(|t| *t /= sum);
    }
    taps.into_iter().map(|t| t as f32).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(rate: u32, hz: f32, seconds: f32, channels: usize) -> Vec<f32> {
        let n = (rate as f32 * seconds) as usize;
        (0..n).flat_map(|i| std::iter::repeat_n((2.0 * std::f32::consts::PI * hz * i as f32 / rate as f32).sin() * 0.5, channels)).collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32).sqrt()
    }

    #[test]
    fn a_48k_stereo_second_gives_16000_samples_however_it_is_cut() {
        let input = tone(48_000, 440.0, 1.0, 2);
        let mut whole = Vec::new();
        Downsampler::new(48_000, 2).push(&input, &mut whole);
        assert_eq!(whole.len(), 16_000);
        let mut parts = Vec::new();
        let mut d = Downsampler::new(48_000, 2);
        for piece in input.chunks(777) {
            d.push(piece, &mut parts);
        }
        assert_eq!(parts, whole);
    }

    #[test]
    fn speech_band_passes_and_content_above_8k_is_removed() {
        let mut low = Vec::new();
        Downsampler::new(48_000, 1).push(&tone(48_000, 1_000.0, 1.0, 1), &mut low);
        assert!((rms(&low[2000..]) - 0.5 / 2f32.sqrt()).abs() < 0.02, "{}", rms(&low[2000..]));
        let mut high = Vec::new();
        Downsampler::new(48_000, 1).push(&tone(48_000, 12_000.0, 1.0, 1), &mut high);
        assert!(rms(&high[2000..]) < 0.01, "{}", rms(&high[2000..]));
    }

    #[test]
    fn other_rates_give_the_right_count() {
        let mut out = Vec::new();
        Downsampler::new(44_100, 2).push(&tone(44_100, 440.0, 2.0, 2), &mut out);
        assert!((out.len() as i64 - 32_000).abs() <= 1, "{}", out.len());
        assert!((rms(&out[2000..]) - 0.5 / 2f32.sqrt()).abs() < 0.05);
        let mut same = Vec::new();
        Downsampler::new(16_000, 1).push(&[0.25; 100], &mut same);
        assert_eq!(same.len(), 100);
    }

    #[test]
    fn hostile_input_never_panics() {
        let mut out = Vec::new();
        let mut d = Downsampler::new(0, 0);
        d.push(&[f32::NAN, f32::INFINITY, 1.0], &mut out);
        assert!(out.iter().all(|v| v.is_finite()));
        let mut d = Downsampler::new(u32::MAX, usize::MAX);
        d.push(&[0.5; 7], &mut out);
        d.push(&[], &mut out);
    }
}
