# craft-hwenc-native

craft-hwenc-native runs hardware video encoders for the Craft apps through the drivers' own encode
APIs, without the `ffmpeg` program. It loads the drivers at run time. Building the crate does not
require a vendor SDK, and on a system without the hardware an encoder returns an error and does not
crash.

Only the private `nvenc_ffi`, `mf_ffi`, `vt_ffi` and `vaapi_ffi` modules contain `unsafe` code. The
Media Foundation calls go through the `windows` crate's bindings, and the VideoToolbox calls go
through the `objc2-video-toolbox`, `objc2-core-media`, `objc2-core-video` and
`objc2-core-foundation` bindings.

## Contents

| Item | Description |
|---|---|
| `nvenc::Encoder` | This is NVIDIA's NVENC encoder: H.264 High and HEVC Main from NV12 frames, and HEVC Main 10 from P010 frames. It uses Direct3D 11 on Windows (`nvEncodeAPI64.dll`) and the CUDA driver on 64-bit Linux (`libnvidia-encode.so.1`). |
| `nvenc::Config` | It sets the codec, input layout, size (even, within the GPU's limits), frame rate, `Rate::Quality(1..=51)` or `Rate::Cbr(kbps)`, and keyframe interval. |
| `nvenc::Encoder::encode`, `flush` | Packed frames go in, and 4-byte length-prefixed packets come out in input order, each with its timestamp and keyframe flag. The encoder uses preset P5, high-quality tuning, no B-frames and BT.709 limited range. |
| `nvenc::Encoder::codec_private` | It returns the `avcC` or `hvcC` record for the container. |
| `nvenc::available(codec)` | It reports whether this system can open an NVENC session that writes `codec`. |
| `mf::Encoder` | This runs the hardware H.264 and HEVC encoder MFTs (Media Foundation transforms) that Windows Media Foundation lists (`mfplat.dll`), from NV12 frames. It has the same `Config`, `encode`, `flush` and `codec_private` as NVENC. Quality is `100 - 2 × level`, constant bitrate uses a one-second buffer, and it uses no B-frames and BT.709 limited range. |
| `mf::available(codec)` | It reports whether Media Foundation lists a hardware encoder that writes `codec` and opens. |
| `vt::Encoder` | This runs the hardware H.264 High and HEVC Main encoders of macOS VideoToolbox, from NV12 frames. It has the same `Config`, `encode`, `flush` and `codec_private` as NVENC. A session that VideoToolbox reports as software is not used. Quality is `(100 - 2 × level) / 100`. Bitrate is constant on macOS 13 and later, and otherwise an average bitrate with a one-second ceiling. It uses no B-frames and BT.709 limited range. |
| `vt::available(codec)` | It reports whether VideoToolbox opens a hardware encoder that writes `codec`. |
| `vaapi::Encoder` | This runs the VA-API hardware H.264 (High, Main or Constrained Baseline) and HEVC Main encoders on 64-bit Linux (`libva.so.2`, `libva-drm.so.2`), from NV12 frames. It uses the first `/dev/dri/renderD*` node whose driver encodes the codec. It has the same `Config`, `encode`, `flush` and `codec_private` as NVENC. Quality is a constant QP at the level, or a variable bitrate where the driver has no constant QP. Constant bitrate uses a one-second buffer. It writes IDR and P pictures only, and HEVC sizes are multiples of the driver's smallest coding block. |
| `vaapi::available(codec)` | It reports whether a VA-API driver opens an encoder that writes `codec`. |
| `vaapi::copy_nv12`, `vaapi::packet` | These are the two passes the VA-API encoder makes over each frame on the CPU: `copy_nv12` copies the frame into a surface's pitched planes, and `packet` turns the coded picture into a packet. |
| `annexb::nals`, `to_length_prefixed`, `parameter_sets` | `nals` splits an Annex B stream into network abstraction layer (NAL) units. `to_length_prefixed` writes a sample without parameter sets or delimiters, and `parameter_sets` returns the parameter sets. |
| `avc::avcc`, `hevc::hvcc` | These build the decoder configuration records of ISO/IEC 14496-15 and check them against the stream that was asked for. |

## Usage

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

`mf::Encoder`, `vt::Encoder` and `vaapi::Encoder` take the same `Config` with `Input::Nv12`.

`vaapi::Encoder` keeps up to four frames in flight, each in its own surface and coded buffer.
`encode` copies the frame into a free surface and submits it, and waits for the GPU only when all
four are taken, for the frame submitted three frames before. With libva 2.9 or later and a
driver that has `vaSyncSurface2`, a frame the GPU has not finished after two seconds is an error;
with an older libva the wait has no limit. The surfaces, coded buffers and the buffer that
collects coded data are made once in `Encoder::new`. Each frame still allocates on the CPU side:
the packet it returns, and the `Vec` of packets when one comes out. In the driver, each frame
creates and destroys its picture and slice parameter buffers (and on an IDR picture the sequence,
rate and frame rate buffers) and maps its surface with `vaDeriveImage`. Coded data is copied out
of the mapped coded buffer and then into the length-prefixed packet. `cargo bench -p
craft-hwenc-native` measures the copy and the packet, and a 1080p encode where a VA-API H.264
encoder is present; the copy bench writes into heap memory, not a mapped surface.

## Limits and failures

- Off Windows, and on Windows without a hardware encoder MFT, `mf::Encoder::new` returns
  `Error::NoDevice`.
- Off macOS, and on a Mac without a hardware encoder for the codec, `vt::Encoder::new` returns
  `Error::NoDevice`.
- Off 64-bit Linux, without libva, and without a driver that encodes the codec,
  `vaapi::Encoder::new` returns `Error::NoDevice`.

## Tests

The NVENC and VA-API layout tests compare every structure the crate passes to the driver with the
C header's sizes and offsets, and they run on every platform.

## Sources

- ISO/IEC 14496-15 for the `avcC` and `hvcC` decoder configuration records.
- NVIDIA's `nvEncodeAPI.h` (Video Codec SDK, API 12.1) for the NVENC structures.

## License

The crate is licensed under MIT OR Apache-2.0, at your option.

`NOTICE` lists the code this crate took from FilmCraft's `crates/platform`, with the FilmCraft
commit and copyright: the NVENC backend, the Annex B helpers, the HEVC parameter-set reading and the
layout tests; the Media Foundation loading, COM setup, sample creation, MFT enumeration and output
reading; and the VideoToolbox session setup, pixel buffer filling, output callback and
parameter-set reading. It also names NVIDIA's `nvEncodeAPI.h` as the source of the NVENC
declarations, with NVIDIA's copyright and MIT license.
