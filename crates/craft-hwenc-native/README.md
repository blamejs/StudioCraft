# craft-hwenc-native

Hardware video encoders for the Craft apps, called through the drivers' own encode APIs instead of
the ffmpeg program. The drivers are loaded at run time, so building needs no vendor SDK, and a
system without the hardware gets an error instead of a crash.

Licensed under MIT OR Apache-2.0. See NOTICE for the sources.

## Contents

| Item | What |
|---|---|
| `nvenc::Encoder` | NVENC: H.264 High and HEVC Main from NV12 frames, HEVC Main 10 from P010 frames; Direct3D 11 on Windows (`nvEncodeAPI64.dll`), the CUDA driver on 64-bit Linux (`libnvidia-encode.so.1`) |
| `nvenc::Config` | codec, input layout, size (even, within the GPU's limits), frame rate, `Rate::Quality(1..=51)` or `Rate::Cbr(kbps)`, keyframe interval |
| `nvenc::Encoder::encode`, `flush` | packed frames in, 4-byte length-prefixed packets out in input order, each with its timestamp and keyframe flag; preset P5, high-quality tuning, no B-frames, BT.709 limited range |
| `nvenc::Encoder::codec_private` | the `avcC` or `hvcC` record for the container |
| `nvenc::available(codec)` | whether this system can open an NVENC session that writes `codec` |
| `annexb::nals`, `to_length_prefixed`, `parameter_sets` | Annex B streams split into NAL units; samples without parameter sets or delimiters |
| `avc::avcc`, `hevc::hvcc` | the decoder configuration records of ISO/IEC 14496-15, checked against the stream that was asked for |

## Use

```rust
use craft_hwenc_native::Codec;
use craft_hwenc_native::nvenc::{Config, Encoder, Input, Rate};

let cfg = Config { codec: Codec::Hevc, input: Input::Nv12, width: 1920, height: 1080, fps: (60, 1), rate: Rate::Quality(23), keyint: 120 };
let mut enc = Encoder::new(&cfg)?;
let record = enc.codec_private().to_vec();
for (pts, frame) in frames.enumerate() {
    for packet in enc.encode(&frame, pts as i64)? {
        write(packet);
    }
}
for packet in enc.flush()? {
    write(packet);
}
```

Only the private `nvenc_ffi` module contains `unsafe` code. Its layout tests compare every
structure the crate passes to the driver with the C header's sizes and offsets, and they run on
every platform.
