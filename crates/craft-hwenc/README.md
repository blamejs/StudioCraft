# craft-hwenc

craft-hwenc runs hardware video encoders for the Craft apps through FFmpeg, as the separate
`ffmpeg` program. The crate lists the hardware encoders FFmpeg offers, tests each one by encoding a
single frame, keeps the results for the session or in a file between sessions, and picks one
automatically by the graphics adapter's vendor, with the software encoder as the fallback. It runs
`ffmpeg` as a child process, has no dependencies and contains no `unsafe` code.

## Contents

| Item | Description |
|---|---|
| `CANDIDATES`, `candidate(id)` | These list 17 encoders: NVENC, AMF, Quick Sync, Media Foundation, VideoToolbox and VA-API, each for H.264, HEVC and AV1 where FFmpeg has the codec. |
| `Candidate::runs_on(os)` | NVENC and Quick Sync run on Windows and Linux, AMF and Media Foundation on Windows, VideoToolbox on macOS, and VA-API on Linux. |
| `Candidate::accepts(input)` | Every encoder accepts NV12, and HEVC and AV1 encoders except Media Foundation also accept P010. |
| `Candidate::prelude(input)` | It returns the arguments before the codec arguments. VA-API opens `vaapi_device()` and uploads each frame with `-vf format=<input>,hwupload`. |
| `vaapi_devices()`, `vaapi_device(candidate)` | These return the `/dev/dri/renderD*` nodes in number order with NVIDIA's last, or `/dev/dri/renderD128` when none is listed. A VA-API test tries each node, and the encodes that follow for that candidate use the node it passed on. |
| `auto_order(vendor, os, codec)` | It returns the order the automatic choice tries: the vendor's own API, then the system's API (Media Foundation, VideoToolbox, VA-API), then the other vendors' APIs. |
| `FfmpegProber` | It runs the one-frame test: a gray 256 × 144 frame of the input format on standard input, `-frames:v 1 -c:v <codec> -f null -`, stopped after a timeout (15 s by default) or when a `Cancel` is set. A test that runs past its timeout gives `Outcome::TimedOut`. |
| `FfmpegProber::with_encode_args` | It tests each candidate with the encode arguments an output gives it (prelude, `-c:v` and options). |
| `FfmpegProber::with_clock`, `Clock`, `SystemClock` | The timeouts are measured on a `Clock`. `SystemClock` is the system's; a test passes a clock it moves forward itself. |
| `probe_codec` | It runs the same test for any encode arguments. |
| `classify(log)` | It sorts FFmpeg's error output into `Busy` (no free encoding session), `NoDevice` (no device or driver) or `Failed`. |
| `ProbeCache` | It keeps outcomes per `ffmpeg` program, encoder and input format for the session. `mark_failed` records an encoder that failed while it ran. Canceled tests, a missing `ffmpeg`, a busy device, an encoder that was not tested and a test stopped before the prober's own timeout are not kept. `without_outcomes` keeps only the failures, for starting an output while another output holds an encoder. |
| `ProbeCache::save`, `ProbeCache::load`, `Fingerprint` | `save` writes the outcomes to a text file for one `Fingerprint`: the `ffmpeg` program, its version and the graphics driver. `load` reads them back only for the same fingerprint, and returns `Loaded::Stale` for another one, `Loaded::Corrupt` for a damaged file or one larger than 1 MiB, and `Loaded::Missing` when there is no file. Timeouts, busy devices and encoders that failed while they ran are not saved. |
| `ffmpeg_version`, `Fingerprint::of`, `program_stamp` | The version is the first line of `ffmpeg -version`, stopped after a timeout, with the program file's size and modification time. `program_stamp` gives the size and modification time alone, without running the program. |
| `Candidate::lossless()` | It reports whether FFmpeg runs the encoder in its lossless mode, which NVENC H.264 and HEVC have. |
| `Preference`, `resolve` | A preference is `auto`, `software` or an encoder id. `resolve` turns it into `Choice::Hardware` or `Choice::Software` with what each test found. A named encoder that fails its test, writes another codec than the target's or cannot run losslessly for a lossless target is an `Error`. |
| `resolve_within`, `Budget`, `DEFAULT_BUDGET` | `resolve_within` gives the tests of one choice a total time. A running test stops when the time is used, and the encoders that are left are reported as `Outcome::NotTested`. `resolve` uses `DEFAULT_BUDGET`, 30 s. A `Prober` stops a running test at the budget through `Prober::probe_within`; one that implements only `probe` stops it at its own limit. |

## Usage

```rust
use craft_hwenc::{Cancel, Codec, FfmpegProber, Fingerprint, Input, Os, Preference, ProbeCache, Prober, Target, Vendor};
use std::time::Duration;

let prober = FfmpegProber::new("/usr/bin/ffmpeg", listed_encoders);
let cache = ProbeCache::new();
let file = config_dir.join("hardware-encoder-tests.txt");
let fingerprint = Fingerprint::of(prober.program(), driver_description, &Cancel::new(), Duration::from_secs(5));
cache.load(&file, &fingerprint);
let target = Target { vendor: Vendor::from_pci_id(adapter_vendor_id), os: Os::current(), codec: Codec::H264, input: Input::Nv12, lossless: false };
match craft_hwenc::resolve(&Preference::Auto, target, &prober, &cache, &Cancel::new())? {
    craft_hwenc::Choice::Hardware(c) => println!("encoding with {} ({})", c.name, c.ffmpeg_codec),
    craft_hwenc::Choice::Software { tried } => println!("software; {} hardware encoders failed their test", tried.len()),
}
cache.save(&file, &fingerprint)?;
```

`listed_encoders` is the codec list of `ffmpeg -encoders`. An empty list tries every encoder.
`driver_description` is any text that changes with the graphics adapter or its driver version,
and with the app's version when the app tests encoders of its own.

## Limits and failures

- The crate does not pass P010 frames to H.264 encoders. FFmpeg converts such frames to 8 bits
  without reporting an error.
- Measured on a Linux machine without a GPU, with FFmpeg 8.0.1: an automatic choice tests 3
  encoders in 150 to 220 ms, and the next session reads the file and runs `ffmpeg -version` in
  46 to 56 ms. With an `ffmpeg` that never answers, a choice with a 5 s timeout took 15 s
  without a total limit and 10 s with a 10 s budget.

## License

The crate is licensed under MIT OR Apache-2.0, at your option.
