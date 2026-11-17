# craft-tsmux

An MPEG-2 transport stream writer (ITU-T H.222.0 | ISO/IEC 13818-1) for the Craft apps. It writes
one program to any `std::io::Write` as frames arrive, so the same code writes `.ts` files, HLS
segments and SRT payloads. It has no dependencies beyond `std`, contains no `unsafe` code, and
builds for `wasm32-unknown-unknown`.

License: MIT OR Apache-2.0.

## What it writes

- A program association section and a program map section, each ending with its CRC-32. They are
  written at the start, every `Config::psi_interval`, before every video keyframe when
  `Config::psi_before_keyframes` is set, and before the first frame after `Muxer::replace_writer`.
- One PES packet per frame, with PTS and, when it differs from the PTS, DTS:

  | `Codec` | stream_type | stream_id | Frames passed in |
  | --- | --- | --- | --- |
  | `H264` | 0x1B | 0xE0 | Annex B access units; an access unit delimiter is added when missing |
  | `Hevc` | 0x24 | 0xE0 | Annex B access units; an access unit delimiter is added when missing |
  | `Mpeg2Video` | 0x02 | 0xE0 | pictures from their first start code |
  | `AacAdts` | 0x0F | 0xC0 | ADTS frames |
  | `Aac { .. }` | 0x0F | 0xC0 | raw AAC frames; the writer adds the ADTS header |
  | `Opus { channels }` | 0x06 | 0xBD | Opus packets; the writer adds the Opus control header |
  | `Ac3` | 0x81 | 0xBD | AC-3 sync frames |
  | `MpegAudio { low_sample_rate }` | 0x03 or 0x04 | 0xC0 | MPEG audio frames, layers I to III |

  AC-3 streams carry a registration descriptor with the format identifier `AC-3`. Opus streams
  carry a registration descriptor `Opus` and an extension descriptor (tag 0x7F, extension tag
  0x80) whose channel_config_code is the channel count. A stream may also carry an ISO 639
  language descriptor.
- The PCR on `Config::pcr_pid`: one of the streams' PIDs, or a PID that carries only PCRs. On a
  stream's PID the PCR goes in the adaptation field of each of its frames, and in packets with
  only an adaptation field when its frames are further apart than `Config::pcr_interval`. PCRs
  are never more than `pcr_interval` apart, and the interval is at most 100 ms (§2.7.2). The PCR
  is the largest DTS so far minus `Config::pcr_delay`. A live output calls `Muxer::advance`
  while no frames arrive.
- A continuity counter per PID, which counts only packets with a payload; the random access
  indicator in the first packet of each keyframe; and adaptation field stuffing in each frame's
  last packet.

Timestamps are 90 kHz ticks and are written modulo 2^33. Each packet is built in a 188-byte
buffer and written; the writer does not allocate per frame or per packet.

## Limits and errors

- 1 to 32 streams. PIDs are 0x0010 to 0x1FFE and must differ.
- Audio frames must fit in one PES packet (65 535 bytes). Video frames have no size limit.
- A frame whose DTS is after its PTS, before its stream's previous DTS, or behind the clock the
  PCRs already sent (more than `pcr_delay` behind the largest DTS, or behind a time given to
  `Muxer::advance`) is refused.
- After `Muxer::replace_writer`, the new output starts with the PAT and PMT, and the next frame or
  `advance` writes a PCR into it.
- Frames are checked for their codec's start code or sync word before anything is written.
- A gap in the timeline longer than `MAX_PCR_FILL` PCR intervals gets one PCR at its end and is
  counted in `Stats::pcr_gaps`.

## Defaults

| Setting | Value | Source |
| --- | --- | --- |
| Program number, PMT PID | 1, 0x1000 | estimated |
| PCR PID | the first video stream, or the first stream | estimated |
| PCR interval | 40 ms (largest 100 ms) | estimated (the limit: H.222.0 §2.7.2) |
| PCR delay | 700 ms | estimated |
| PAT and PMT interval | 100 ms, and before keyframes | estimated |

## Example

```rust
use craft_tsmux::{Codec, Config, Frame, Muxer, Stream};

let streams = [Stream::new(0x100, Codec::H264), Stream::new(0x101, Codec::AacAdts)];
let mut ts = Muxer::new(Vec::new(), Config::default(), &streams)?;
let idr = [0, 0, 0, 1, 0x65, 0x88, 0x84];
ts.write_frame(0, &Frame { data: &idr, pts: 90_000, dts: None, key: true })?;
let bytes = ts.finish()?;
assert_eq!(bytes.len() % 188, 0);
# Ok::<(), craft_tsmux::Error>(())
```

## Tests

`cargo test -p craft-tsmux` reads the output back with craft-media's transport stream demuxer
(`tests/readback.rs`): streams, codecs, languages, every frame's bytes, PTS and DTS, keyframes and
the PCR range, with no continuity or CRC warnings. `tests/fuzz.rs` muxes random programs and
timelines and reads them back, and feeds random bytes to every codec.

## Specifications

- ITU-T H.222.0 | ISO/IEC 13818-1, Information technology: Generic coding of moving pictures
  and associated audio information: Systems.
- ISO/IEC 13818-7, Advanced Audio Coding, for the ADTS header.
- Opus Interactive Audio Codec Transport Multiplexing Standard, draft TS V0.1.3 (2015-10), for
  the Opus control header and descriptors.
- ATSC A/52, Digital Audio Compression (AC-3, E-AC-3) Standard, for the AC-3 stream type and
  registration descriptor.
