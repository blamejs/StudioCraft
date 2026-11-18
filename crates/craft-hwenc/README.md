# craft-hwenc

craft-hwenc runs hardware video encoders for the Craft apps through FFmpeg, as the separate
`ffmpeg` program. The crate lists the hardware encoders FFmpeg offers, tests each one by encoding a
single frame, keeps the results for the session, and picks one automatically by the graphics
adapter's vendor, with the software encoder as the fallback. It runs `ffmpeg` as a child process,
has no dependencies and contains no `unsafe` code.

## Contents

| Item | Description |
|---|---|
| `CANDIDATES`, `candidate(id)` | These list 16 encoders: NVENC, AMF, Quick Sync, Media Foundation, VideoToolbox and VA-API, each for H.264, HEVC and AV1 where FFmpeg has the codec. |
| `Candidate::runs_on(os)` | NVENC and Quick Sync run on Windows and Linux, AMF and Media Foundation on Windows, VideoToolbox on macOS, and VA-API on Linux. |
| `Candidate::accepts(input)` | Every encoder accepts NV12, and HEVC and AV1 encoders except Media Foundation also accept P010. |
| `Candidate::prelude(input)` | It returns the arguments before the codec arguments. VA-API opens `vaapi_device()` and uploads each frame with `-vf format=<input>,hwupload`. |
| `vaapi_devices()`, `vaapi_device(candidate)` | These return the `/dev/dri/renderD*` nodes in number order with NVIDIA's last, or `/dev/dri/renderD128` when none is listed. A VA-API test tries each node, and the encodes that follow for that candidate use the node it passed on. |
| `auto_order(vendor, os, codec)` | It returns the order the automatic choice tries: the vendor's own API, then the system's API (Media Foundation, VideoToolbox, VA-API), then the other vendors' APIs. |
| `FfmpegProber` | It runs the one-frame test: a gray 256 × 144 frame of the input format on standard input, `-frames:v 1 -c:v <codec> -f null -`, stopped after a timeout (15 s by default) or when a `Cancel` is set. |
| `FfmpegProber::with_encode_args` | It tests each candidate with the encode arguments an output gives it (prelude, `-c:v` and options). |
| `probe_codec` | It runs the same test for any encode arguments. |
| `classify(log)` | It sorts FFmpeg's error output into `Busy` (no free encoding session), `NoDevice` (no device or driver) or `Failed`. |
| `ProbeCache` | It keeps outcomes per `ffmpeg` program, encoder and input format for the session. `mark_failed` records an encoder that failed while it ran. Canceled tests, a missing `ffmpeg` and a busy device are not kept. `without_outcomes` keeps only the failures, for starting an output while another output holds an encoder. |
| `Candidate::lossless()` | It reports whether FFmpeg runs the encoder in its lossless mode, which NVENC H.264 and HEVC have. |
| `Preference`, `resolve` | A preference is `auto`, `software` or an encoder id. `resolve` turns it into `Choice::Hardware` or `Choice::Software` with what each test found. A named encoder that fails its test, writes another codec than the target's or cannot run losslessly for a lossless target is an `Error`. |

## Usage

```rust
use craft_hwenc::{Cancel, Codec, FfmpegProber, Input, Os, Preference, ProbeCache, Target, Vendor};

let prober = FfmpegProber::new("/usr/bin/ffmpeg", listed_encoders);
let cache = ProbeCache::new();
let target = Target { vendor: Vendor::from_pci_id(adapter_vendor_id), os: Os::current(), codec: Codec::H264, input: Input::Nv12, lossless: false };
match craft_hwenc::resolve(&Preference::Auto, target, &prober, &cache, &Cancel::new())? {
    craft_hwenc::Choice::Hardware(c) => println!("encoding with {} ({})", c.name, c.ffmpeg_codec),
    craft_hwenc::Choice::Software { tried } => println!("software; {} hardware encoders failed their test", tried.len()),
}
```

`listed_encoders` is the codec list of `ffmpeg -encoders`. An empty list tries every encoder.

## Limits and failures

- The crate does not pass P010 frames to H.264 encoders. FFmpeg converts such frames to 8 bits
  without reporting an error.

## License

The crate is licensed under MIT OR Apache-2.0, at your option.
