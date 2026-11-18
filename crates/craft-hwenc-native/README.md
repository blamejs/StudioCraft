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
| `mf::Encoder` | the hardware H.264 and HEVC encoder MFTs that Windows Media Foundation lists (`mfplat.dll`), from NV12 frames; the same `Config`, `encode`, `flush` and `codec_private` as NVENC; quality `100 - 2 × level`, constant bitrate with a one-second buffer, no B-frames, BT.709 limited range |
| `mf::available(codec)` | whether Media Foundation lists a hardware encoder that writes `codec` and opens |
| `vt::Encoder` | the hardware H.264 High and HEVC Main encoders of macOS VideoToolbox, from NV12 frames; the same `Config`, `encode`, `flush` and `codec_private` as NVENC; a session that VideoToolbox reports as software is not used; quality `(100 - 2 × level) / 100`, constant bitrate on macOS 13 and later and otherwise an average bitrate with a one-second ceiling, no B-frames, BT.709 limited range |
| `vt::available(codec)` | whether VideoToolbox opens a hardware encoder that writes `codec` |
| `vaapi::Encoder` | the VA-API hardware H.264 (High, Main or Constrained Baseline) and HEVC Main encoders on 64-bit Linux (`libva.so.2`, `libva-drm.so.2`), on the first `/dev/dri/renderD*` node whose driver encodes the codec, from NV12 frames; the same `Config`, `encode`, `flush` and `codec_private` as NVENC; constant QP at the level (a variable bitrate where the driver has no constant QP), constant bitrate with a one-second buffer, IDR and P pictures only; HEVC sizes are multiples of the driver's smallest coding block |
| `vaapi::available(codec)` | whether a VA-API driver opens an encoder that writes `codec` |
| `vaapi::copy_nv12`, `vaapi::packet` | the two passes the VA-API encoder makes over each frame on the CPU: the copy into a surface's pitched planes, and the coded picture to a packet |
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

`mf::Encoder` takes the same `Config` with `Input::Nv12`. Off Windows, and on Windows without a
hardware encoder MFT, `Encoder::new` returns `Error::NoDevice`. `vt::Encoder` also takes
`Input::Nv12`. Off macOS, and on a Mac without a hardware encoder for the codec, `Encoder::new`
returns `Error::NoDevice`. `vaapi::Encoder` also takes `Input::Nv12`. Off 64-bit Linux, without
libva, and without a driver that encodes the codec, `Encoder::new` returns `Error::NoDevice`.

`vaapi::Encoder` keeps up to four frames in flight, each in its own surface and coded buffer.
`encode` copies the frame into a free surface and submits it, and waits for the GPU only when all
four are taken, for the frame submitted three frames before. The surfaces, coded buffers and the
buffer that collects coded data are made once in `Encoder::new`; each frame allocates only the
packet it returns. `cargo bench -p craft-hwenc-native` measures the copy and the packet, and a
1080p encode where a VA-API H.264 encoder is present.

Only the private `nvenc_ffi`, `mf_ffi`, `vt_ffi` and `vaapi_ffi` modules contain `unsafe` code.
The NVENC and VA-API layout tests compare every structure the crate passes to the driver with the
C header's sizes and offsets, and they run on every platform. The Media Foundation calls go through the `windows`
crate's bindings, and the VideoToolbox calls go through the `objc2-video-toolbox`,
`objc2-core-media`, `objc2-core-video` and `objc2-core-foundation` bindings.
