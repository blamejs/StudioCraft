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
| `nvenc::Encoder` | This is NVIDIA's NVENC encoder: H.264 High, HEVC Main and 8-bit AV1 Main from NV12 frames, and HEVC Main 10 and 10-bit AV1 Main from P010 frames. It uses Direct3D 11 on Windows (`nvEncodeAPI64.dll`) and the CUDA driver on 64-bit Linux (`libnvidia-encode.so.1`). AV1 needs a GPU whose encoder writes AV1; on others `Encoder::new` returns `Error::Unsupported`. |
| `nvenc::Config` | It sets the codec, input layout, size (even, within the GPU's limits), frame rate, `Rate::Quality(1..=51)` or `Rate::Cbr(kbps)`, keyframe interval and `Color`. |
| `nvenc::Encoder::encode`, `flush` | Packed frames go in, and packets come out in input order, each with its timestamp and keyframe flag: 4-byte length-prefixed NAL units for H.264 and HEVC, OBUs with their sizes and without temporal delimiters for AV1. The encoder uses preset P5, high-quality tuning, no B-frames and limited range. The VUI declares the `Color` of the `Config`; AV1 is BT.709, in the sequence header's color description. |
| `nvenc::Encoder::on_d3d11`, `encode_texture` | On Windows, `on_d3d11` opens an encoder on the caller's Direct3D 11 device (an NVIDIA adapter's), and `encode_texture` encodes an NV12 or P010 `ID3D11Texture2D` of the encoder's size made on that device. The encoder reads the texture's first subresource on the GPU, so the frame is not copied to system memory. |
| `Color` | This is the color signal of a stream: `Color::Bt709`, `Color::Hlg` (BT.2020 with the HLG transfer) or `Color::Pq { mastering, light }` (BT.2020 with the PQ transfer). NVENC writes `Hlg` and `Pq` as HEVC Main 10 from P010 frames. For `Pq`, a `MasteringDisplay` and a `ContentLight` that are set go in front of every IDR picture as the mastering display colour volume (payload type 137) and content light level (payload type 144) SEI messages. The other encoders return `Error::Unsupported` for any color but `Bt709`. |
| `MasteringDisplay`, `ContentLight` | These hold the PQ static metadata: the mastering display's primaries and white point in units of 0.00002 and its peak and black luminance in units of 0.0001 cd/m², and MaxCLL and MaxFALL in cd/m². A chromaticity above 50,000, a black level at or above the peak, and a MaxFALL above a nonzero MaxCLL return `Error::Unsupported`. `MasteringDisplay::bt2020_d65` fills in the BT.2020 primaries and the D65 white point. |
| `nvenc::Encoder::codec_private` | It returns the `avcC`, `hvcC` or `av1C` record for the container. |
| `nvenc::available(codec)` | It reports whether this system can open an NVENC session that writes `codec`. |
| `mf::Encoder` | This runs the hardware H.264, HEVC and AV1 (Main, 8-bit) encoder MFTs (Media Foundation transforms) that Windows Media Foundation lists (`mfplat.dll`), from NV12 frames. It has the same `Config`, `encode`, `flush` and `codec_private` as NVENC. Quality is `100 - 2 × level`, constant bitrate uses a one-second buffer, and it uses no B-frames and BT.709 limited range. |
| `mf::available(codec)` | It reports whether Media Foundation lists a hardware encoder that writes `codec` and opens. |
| `vt::Encoder` | This runs the hardware H.264 High and HEVC Main encoders of macOS VideoToolbox, from NV12 frames. It has the same `Config`, `encode`, `flush` and `codec_private` as NVENC. A session that VideoToolbox reports as software is not used. Quality is `(100 - 2 × level) / 100`. Bitrate is constant on macOS 13 and later, and otherwise an average bitrate with a one-second ceiling. It uses no B-frames and BT.709 limited range. |
| `vt::available(codec)` | It reports whether VideoToolbox opens a hardware encoder that writes `codec`. |
| `vaapi::Encoder` | This runs the VA-API hardware H.264 (High, Main or Constrained Baseline) and HEVC Main encoders on 64-bit Linux (`libva.so.2`, `libva-drm.so.2`), from NV12 frames. It uses the first `/dev/dri/renderD*` node whose driver encodes the codec. It has the same `Config`, `encode`, `flush` and `codec_private` as NVENC. Quality is a constant QP at the level, or a variable bitrate where the driver has no constant QP. Constant bitrate uses a one-second buffer. It writes IDR and P pictures only, and HEVC sizes are multiples of the driver's smallest coding block. |
| `vaapi::available(codec)` | It reports whether a VA-API driver opens an encoder that writes `codec`. |
| `vaapi::copy_nv12`, `vaapi::packet` | These are the two passes the VA-API encoder makes over each frame on the CPU: `copy_nv12` copies the frame into a surface's pitched planes, and `packet` turns the coded picture into a packet. |
| `annexb::nals`, `to_length_prefixed`, `parameter_sets` | `nals` splits an Annex B stream into network abstraction layer (NAL) units. `to_length_prefixed` writes a sample without parameter sets or delimiters, and `parameter_sets` returns the parameter sets. |
| `avc::avcc`, `hevc::hvcc` | These build the decoder configuration records of ISO/IEC 14496-15 and check them against the stream that was asked for. |
| `av1::obus`, `SequenceHeader`, `Stream` | `obus` splits an AV1 temporal unit into OBUs. `SequenceHeader::parse` reads a sequence header. `Stream` builds the `av1C` record from the encoder's sequence header, checks that it is Main 4:2:0 at the bit depth that was asked for, and turns each temporal unit into a sample without temporal delimiters, padding or tile lists, with the keyframe flag read from its frame header. |

## Usage

```rust
use craft_hwenc_native::Codec;
use craft_hwenc_native::nvenc::{Color, Config, Encoder, Input, Rate};

let cfg = Config { codec: Codec::Hevc, input: Input::Nv12, width: 1920, height: 1080, fps: (60, 1), rate: Rate::Quality(23), keyint: 120, color: Color::Bt709 };
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

`mf::Encoder`, `vt::Encoder` and `vaapi::Encoder` take the same `Config` with `Input::Nv12` and
`Color::Bt709`.

An HDR10 stream on NVENC takes P010 frames and a PQ color with its metadata:

```rust
use craft_hwenc_native::nvenc::{Color, Config, ContentLight, Input, MasteringDisplay};

let color = Color::Pq {
    mastering: Some(MasteringDisplay::bt2020_d65(10_000_000, 50)),
    light: Some(ContentLight { max_cll: 1000, max_fall: 400 }),
};
let cfg = Config { input: Input::P010, color, ..cfg };
```

On Windows, frames that are already on the GPU go in as textures:

```rust
let mut enc = Encoder::on_d3d11(&cfg, &device)?;
for (pts, texture) in textures.enumerate() {
    for packet in enc.encode_texture(&texture, pts as i64)? {
        write(packet);
    }
}
```

The encoder registers each texture with the driver the first time it sees it and keeps the last 64
registered, so a pool of textures that the caller reuses is registered once. A texture whose
picture is still inside the encoder is refused with an error; keep a texture unchanged until the
packet with its timestamp has come out. A texture of another size, format or device is an error,
and so is a call to `encode` on a texture encoder or to `encode_texture` on one opened with
`Encoder::new`. The encoder opened with `on_d3d11` creates no input buffers in system memory.

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
- `vt::Encoder::new` and `vaapi::Encoder::new` return `Error::Unsupported` for `Codec::Av1`, and
  `nvenc::Encoder::new` returns it for AV1 with `Color::Hlg` or `Color::Pq`.
- An AV1 stream whose sequence header changes, or is not Main 4:2:0 at the requested bit depth, is
  an `Error::Failed`.

## Tests

The NVENC and VA-API layout tests compare every structure the crate passes to the driver with the
C header's sizes and offsets, and they run on every platform.

## Sources

- ISO/IEC 14496-15 for the `avcC` and `hvcC` decoder configuration records.
- NVIDIA's `nvEncodeAPI.h` (Video Codec SDK, API 12.1) for the NVENC structures.
- ITU-T H.273 for the color code points, and ITU-T H.265 Annex D (D.2.28 and D.2.35) for the
  mastering display colour volume and content light level SEI payloads.
- The AV1 Bitstream and Decoding Process Specification (https://aomediacodec.github.io/av1-spec/)
  for OBUs, sequence headers and frame headers.
- AV1 Codec ISO Media File Format Binding (https://aomediacodec.github.io/av1-isobmff/) for the
  `av1C` record and the sample format.

## License

The crate is licensed under MIT OR Apache-2.0, at your option.

`NOTICE` lists the code this crate took from FilmCraft's `crates/platform`, with the FilmCraft
commit and copyright: the NVENC backend, the Annex B helpers, the HEVC parameter-set reading and the
layout tests; the Media Foundation loading, COM setup, sample creation, MFT enumeration and output
reading; and the VideoToolbox session setup, pixel buffer filling, output callback and
parameter-set reading. It also names NVIDIA's `nvEncodeAPI.h` as the source of the NVENC
declarations, with NVIDIA's copyright and MIT license.
