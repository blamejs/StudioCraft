# craft-hwenc

Hardware video encoders for the Craft apps, run through the separate ffmpeg program. The crate
lists the hardware encoders ffmpeg offers, tests each one by encoding a single frame, keeps the
results for the session, and picks one automatically by the graphics adapter's vendor, with the
software encoder as the fallback. It runs ffmpeg as a child process, has no dependencies and
contains no unsafe code.

Licensed under MIT OR Apache-2.0.

## Contents

| Item | What |
|---|---|
| `CANDIDATES`, `candidate(id)` | 16 encoders: NVENC, AMF, Quick Sync, Media Foundation, VideoToolbox and VA-API, each for H.264, HEVC and AV1 where ffmpeg has the codec |
| `Candidate::runs_on(os)` | NVENC and Quick Sync on Windows and Linux, AMF and Media Foundation on Windows, VideoToolbox on macOS, VA-API on Linux |
| `Candidate::accepts(input)` | NV12 for every encoder; P010 for HEVC and AV1, except Media Foundation |
| `Candidate::prelude(input)` | the arguments before the codec arguments: VA-API opens `vaapi_device()` and uploads each frame with `-vf format=<input>,hwupload` |
| `vaapi_devices()`, `vaapi_device(candidate)` | the `/dev/dri/renderD*` nodes in number order with NVIDIA's last, or `/dev/dri/renderD128` when none is listed; a VA-API test tries each node, and the node it passed on is used for that candidate's encodes that follow |
| `auto_order(vendor, os, codec)` | the order the automatic choice tries: the vendor's own API, then the system's API (Media Foundation, VideoToolbox, VA-API), then the other vendors' APIs |
| `FfmpegProber` | the one-frame test: a gray 256 × 144 frame of the input format on standard input, `-frames:v 1 -c:v <codec> -f null -`, stopped after a timeout (15 s by default) or when a `Cancel` is set |
| `FfmpegProber::with_encode_args` | tests each candidate with the encode arguments an output gives it (prelude, `-c:v` and options) |
| `probe_codec` | the same test for any encode arguments |
| `classify(log)` | ffmpeg's error output as `Busy` (no free encoding session), `NoDevice` (no device or driver) or `Failed` |
| `ProbeCache` | outcomes per ffmpeg program, encoder and input format for the session; `mark_failed` records an encoder that failed while it ran; canceled tests, a missing ffmpeg and a busy device are not kept; `without_outcomes` keeps only the failures, for a start while another output holds an encoder |
| `Candidate::lossless()` | whether ffmpeg runs it losslessly: NVENC H.264 and HEVC |
| `Preference`, `resolve` | `auto`, `software` or an encoder id, resolved to `Choice::Hardware` or `Choice::Software` with what each test found; a named encoder that fails its test, writes another codec than the target's or cannot run losslessly for a lossless target is an `Error` |

## Use

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

`listed_encoders` is the codec list of `ffmpeg -encoders`; an empty list tries every encoder.
H.264 encoders are not given P010 frames: ffmpeg would convert them to 8 bits without an error.
