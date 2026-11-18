# craft-hwenc-native

Hardware video encoders for the Craft apps, called through the drivers' own encode APIs instead of
the ffmpeg program. The drivers are loaded at run time, so building needs no vendor SDK, and a
system without the hardware gets an error instead of a crash.

## Contents

| Item | What |
|---|---|
| `nvenc::Encoder` | NVENC: H.264 High and HEVC Main from NV12 frames, and HEVC Main 10 from P010 frames. It uses Direct3D 11 on Windows (`nvEncodeAPI64.dll`) and the CUDA driver on 64-bit Linux (`libnvidia-encode.so.1`). |
| `nvenc::Config` | Codec, input layout, size (even, within the GPU's limits), frame rate, `Rate::Quality(1..=51)` or `Rate::Cbr(kbps)`, and keyframe interval. |
| `nvenc::Encoder::encode`, `flush` | Packed frames go in, and 4-byte length-prefixed packets come out in input order, each with its timestamp and keyframe flag. The encoder uses preset P5, high-quality tuning, no B-frames and BT.709 limited range. |
| `nvenc::Encoder::codec_private` | The `avcC` or `hvcC` record for the container. |
| `nvenc::available(codec)` | Whether this system can open an NVENC session that writes `codec`. |
| `mf::Encoder` | The hardware H.264 and HEVC encoder MFTs (Media Foundation transforms) that Windows Media Foundation lists (`mfplat.dll`), from NV12 frames. It has the same `Config`, `encode`, `flush` and `codec_private` as NVENC. Quality is `100 - 2 × level`, constant bitrate uses a one-second buffer, and it uses no B-frames and BT.709 limited range. |
| `mf::available(codec)` | Whether Media Foundation lists a hardware encoder that writes `codec` and opens. |
| `vt::Encoder` | The hardware H.264 High and HEVC Main encoders of macOS VideoToolbox, from NV12 frames. It has the same `Config`, `encode`, `flush` and `codec_private` as NVENC. A session that VideoToolbox reports as software is not used. Quality is `(100 - 2 × level) / 100`. Bitrate is constant on macOS 13 and later, and otherwise an average bitrate with a one-second ceiling. It uses no B-frames and BT.709 limited range. |
| `vt::available(codec)` | Whether VideoToolbox opens a hardware encoder that writes `codec`. |
| `annexb::nals`, `to_length_prefixed`, `parameter_sets` | Annex B streams split into network abstraction layer (NAL) units, and samples without parameter sets or delimiters. |
| `avc::avcc`, `hevc::hvcc` | The decoder configuration records of ISO/IEC 14496-15, checked against the stream that was asked for. |

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

`mf::Encoder` and `vt::Encoder` take the same `Config` with `Input::Nv12`.

## Limits and failures

- Off Windows, and on Windows without a hardware encoder MFT, `mf::Encoder::new` returns
  `Error::NoDevice`.
- Off macOS, and on a Mac without a hardware encoder for the codec, `vt::Encoder::new` returns
  `Error::NoDevice`.
- Only the private `nvenc_ffi`, `mf_ffi` and `vt_ffi` modules contain `unsafe` code. The Media
  Foundation calls go through the `windows` crate's bindings, and the VideoToolbox calls go through
  the `objc2-video-toolbox`, `objc2-core-media`, `objc2-core-video` and `objc2-core-foundation`
  bindings.

## Tests

The NVENC layout tests compare every structure the crate passes to the driver with the C header's
sizes and offsets, and they run on every platform.

## Sources

- ISO/IEC 14496-15 for the `avcC` and `hvcC` decoder configuration records.
- NVIDIA's `nvEncodeAPI.h` (Video Codec SDK, API 12.1) for the NVENC structures.

## License

Licensed under either of MIT or Apache-2.0, at your option.

`NOTICE` lists the code this crate took from FilmCraft's `crates/platform` (the NVENC backend, the
Annex B helpers, the HEVC parameter-set reading, the layout tests, and the Media Foundation and
VideoToolbox setup), with the FilmCraft commit and copyright. It also names NVIDIA's
`nvEncodeAPI.h` as the source of the NVENC declarations, with NVIDIA's copyright and MIT license.
