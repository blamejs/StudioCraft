# craft-audio-dsp

craft-audio-dsp provides audio digital signal processing (DSP) for the Craft apps. It is written in
Rust with no dependencies, builds for `wasm32-unknown-unknown` and contains no `unsafe` code.

## Contents

- `AudioEffect` is the trait every effect implements. `effects()` is the registry that lists each
  effect's id, name, category and parameters (range, unit, default), and `create_effect(id, rate,
  channels)` builds one by id.
- The effects are equalizers (parametric, graphic, notch, scientific and FFT filters), dynamics
  (compressor, expander and gate, limiter, maximizer, multiband and tube-modeled compressors),
  delays and reverbs (including convolution and surround), modulation, restoration (denoise,
  dehum, click remover, de-esser, de-reverb, speech enhancement), channel tools, distortion,
  dither, a signal generator and a loudness meter.
- `loudness` is an ITU-R BS.1770-4 / EBU R 128 meter. It measures momentary, short-term and
  integrated loudness, loudness range (EBU Tech 3342), sample peak and true peak.
- `pan` holds stereo pan laws, a surround panner over any layout of up to 16 speakers, and
  first-order ambisonic encoding and decoding.
- `channels` holds channel layouts, ITU-R BS.775 upmix and downmix, and the 5.1 panner.
- `generator` holds band-limited sine, square, saw and triangle oscillators, and white and pink
  noise.
- `resample` does streaming rate conversion for playback. `offline::resample` converts whole
  buffers.
- `stretch` stretches time and keeps the pitch.
- `sync` finds the offset between two recordings of one event.
- `ducking` finds speech-activity regions in a trigger track and builds the volume keyframes that
  lower a music track under them.
- `biquad` and `design` provide biquad filters with their magnitude response, and IIR filter design
  from Butterworth, Bessel, Chebyshev and elliptic prototypes.

## Usage

Audio is planar `f32`: one slice per channel, all the same length. Compressor, gate and
limiter-type effects report their gain reduction in dB as a non-negative number through
`AudioEffect::gain_reduction_db`. The compressor and the gate take a key (sidechain) input of up to
two channels through `AudioEffect::process_keyed`.

```rust
use craft_audio_dsp::{AudioEffect, effects::Compressor, loudness::LoudnessMeter};

let mut comp = Compressor::new(48_000.0, 2);
comp.set_param("threshold", -24.0);
let (mut left, mut right) = (vec![0.0f32; 1024], vec![0.0f32; 1024]);
comp.process(&mut [&mut left, &mut right]);

let mut meter = LoudnessMeter::new(48_000.0, 2);
meter.process(&[&left, &right]);
let _integrated = meter.integrated();
```

When a parameter changes, gains, mix amounts and equalizer bands move to the new value per sample.
Other parameters, such as a compressor's threshold, ratio, knee and times or a limiter's ceiling
and release, take effect from the next sample. With the parameters held still, the output does not
depend on the block size.

## API stability

The items the crate exports are its API. Removing or changing one is a breaking change and bumps
the major version (the minor version while it is below 1.0). Effect ids and parameter ids are part
of the API. Hosts save these ids in their files, and a renamed id no longer matches the files that
use it.

## Limits and failures

- `AudioEffect::process`, `process_keyed`, `set_param` and `reset` do not allocate. Every buffer is
  sized when the effect is created.
- The convolution reverb is the one effect whose `process` call can take much longer than a block.
  After a change to its impulse, room size or damping, the next call rebuilds the impulse's
  partitions in buffers sized when the effect was created.
- An effect whose input holds non-finite samples does not keep NaN in its state.
- Constructors clamp the sample rate to `MIN_SAMPLE_RATE ..= MAX_SAMPLE_RATE` (1 kHz to 1.536 MHz)
  and use 48 kHz for a non-finite rate. They clamp the channel count to `1 ..= MAX_CHANNELS` (64).

## Sources

- ITU-R BS.1770-4 and EBU R 128 for loudness, and EBU Tech 3342 for loudness range.
- ITU-R BS.775 for upmix and downmix.

## License

The crate is licensed under MIT OR Apache-2.0, at your option.

The crate combines two crates from the ArtCraft apps, with changes:

- [storytold/filmcraft](https://github.com/storytold/filmcraft) `crates/audio-dsp` at
  `3f137a7ed420cac79b32145f5243b7f98b31f71c` supplied the `AudioEffect` trait, the effects and
  their registry, the loudness meter, the streaming resampler, time stretch, offset search and
  ducking.
- [storytold/soundcraft](https://github.com/storytold/soundcraft) `crates/dsp` at
  `648ed9db9a0e98cc55643babffac381521a556c8` supplied the tail length, offline mode, events, saved
  state and editor parts of the trait, the maximizer, dither, panning, the signal generator and the
  offline resampler.

`NOTICE` keeps the copyright notices of both projects.

Copyright (c) 2026 ArtCraft Team and the FilmCraft contributors.
Copyright (c) 2026 ArtCraft Team and the SoundCraft contributors.
Copyright (c) 2026 The Craft contributors.
