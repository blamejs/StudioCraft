//! The built-in AAC-LC encoder (ISO/IEC 14496-3) for the mix: 48 kHz stereo, constant bitrate.
//!
//! The mixer's blocks are 1024 sample frames, the length of one AAC frame. The first
//! [`AacSession::priming_samples`] decoded samples are the encoder's delay; a container signals
//! them (Matroska `CodecDelay`, an MP4 edit list) so the sound stays in step with the pictures.

use craft_audio::aac::{Encoder as Core, EncoderConfig};
use studiocraft_media::audio::{CHANNELS, SAMPLE_RATE};
use studiocraft_plugin::PluginError;

/// The bitrates the encoder is opened with, in kbit/s; others are clamped into this range.
pub const KBPS: std::ops::RangeInclusive<u32> = 32..=512;

/// A running AAC encoder.
pub struct AacSession {
    core: Core,
    /// The bitrate in bit/s.
    bps: u32,
    /// One block split into its channels.
    planar: Vec<Vec<f32>>,
}

impl AacSession {
    /// An encoder at `kbps` kbit/s, clamped to [`KBPS`].
    pub fn new(kbps: u32) -> Result<AacSession, PluginError> {
        let bps = kbps.clamp(*KBPS.start(), *KBPS.end()) * 1000;
        let core = Core::new(EncoderConfig::cbr(SAMPLE_RATE, CHANNELS, bps))
            .map_err(|e| PluginError::Failed(format!("the AAC encoder could not start at {bps} bit/s: {e}")))?;
        Ok(AacSession { core, bps, planar: vec![Vec::new(); CHANNELS] })
    }

    /// The `AudioSpecificConfig` a container stores with the track.
    pub fn audio_specific_config(&self) -> Vec<u8> {
        self.core.audio_specific_config()
    }

    /// Decoded samples at the start that are the encoder's delay, not sound.
    pub fn priming_samples(&self) -> u32 {
        self.core.priming_samples()
    }

    /// The bitrate in bit/s.
    pub fn bitrate(&self) -> u32 {
        self.bps
    }

    /// Encode interleaved samples (left, right, left, ...); the access units that are ready are
    /// appended to `out`. A trailing partial sample frame is ignored.
    pub fn encode(&mut self, interleaved: &[f32], out: &mut Vec<Vec<u8>>) {
        let frames = interleaved.len() / CHANNELS;
        for (c, plane) in self.planar.iter_mut().enumerate() {
            plane.clear();
            plane.extend(interleaved.iter().skip(c).step_by(CHANNELS).take(frames));
        }
        let planes: [&[f32]; CHANNELS] = [&self.planar[0], &self.planar[1]];
        out.extend(self.core.encode(&planes));
    }

    /// Append the access units that finish the input to `out`. No sample can follow.
    pub fn flush(&mut self, out: &mut Vec<Vec<u8>>) {
        out.extend(self.core.flush());
    }
}

#[cfg(test)]
mod tests {
    use craft_audio::aac::Decoder;

    use super::*;

    /// Interleaved stereo: a sine on the left, silence on the right, and one click at `click`.
    fn tone(frames: usize, click: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| {
                let left = if i == click { 0.9 } else { (i as f32 * 2.0 * std::f32::consts::PI * 440.0 / 48_000.0).sin() * 0.1 };
                [left, if i == click { 0.9 } else { 0.0 }]
            })
            .collect()
    }

    #[test]
    fn the_decoded_sound_starts_after_the_priming_and_keeps_its_timing() {
        let mut enc = AacSession::new(192).unwrap();
        assert_eq!(enc.bitrate(), 192_000);
        let click = 48_000; // one second in
        let input = tone(96_000, click);
        let mut aus = Vec::new();
        for block in input.chunks(1024 * CHANNELS) {
            enc.encode(block, &mut aus);
        }
        enc.flush(&mut aus);
        let mut dec = Decoder::new(&enc.audio_specific_config()).unwrap();
        let mut right = Vec::new();
        for au in &aus {
            let pcm = dec.decode(au).unwrap();
            assert_eq!(pcm.len(), 2);
            right.extend_from_slice(&pcm[1]);
        }
        let priming = enc.priming_samples() as usize;
        assert_eq!(priming, 1024);
        assert!(right.len() >= priming + 96_000, "{} samples", right.len());
        // The right channel is silent apart from the click, so its loudest sample marks the click.
        let loudest = (0..right.len()).max_by(|&a, &b| right[a].abs().total_cmp(&right[b].abs())).unwrap();
        let at = loudest as i64 - priming as i64;
        assert!((at - click as i64).abs() <= 64, "the click decodes at sample {at}, sent at {click}");
    }

    #[test]
    fn bitrates_are_clamped_to_what_the_encoder_takes() {
        assert_eq!(AacSession::new(8).unwrap().bitrate(), 32_000);
        assert_eq!(AacSession::new(512).unwrap().bitrate(), 512_000, "512 kbit/s fits 48 kHz stereo");
        assert_eq!(AacSession::new(u32::MAX).unwrap().bitrate(), 512_000);
    }

    #[test]
    fn a_partial_sample_frame_is_ignored() {
        let mut enc = AacSession::new(128).unwrap();
        let mut aus = Vec::new();
        enc.encode(&[0.1; 3], &mut aus);
        enc.encode(&[], &mut aus);
        assert!(aus.is_empty());
    }
}
