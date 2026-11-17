# craft-tsmux

An MPEG-2 transport stream writer (ITU-T H.222.0 | ISO/IEC 13818-1) for the Craft apps. It writes
one program to any `std::io::Write` as frames arrive, so the same code writes `.ts` files, HLS
segments and SRT payloads. It has no dependencies beyond `std`, contains no `unsafe` code, and
builds for `wasm32-unknown-unknown`.

License: MIT OR Apache-2.0.

## What it writes

- A program association section and a program map section, each ending with its CRC-32. They are
  written at the start, every `Config::psi_interval`, before every video keyframe when
  `Config::psi_before_keyframes` is set, and before the first frame after `Muxer::replace_writer`
  or `Muxer::discontinuity`.
- One PES packet per frame, with PTS and, when it differs from the PTS, DTS:

  | `Codec` | stream_type | stream_id | Frames passed in |
  | --- | --- | --- | --- |
  | `H264` | 0x1B | 0xE0 | Annex B access units; an access unit delimiter is added when missing |
  | `Hevc` | 0x24 | 0xE0 | Annex B access units; an access unit delimiter is added when missing |
  | `Mpeg2Video` | 0x02 | 0xE0 | MPEG-2 pictures from their first start code |
  | `Mpeg1Video` | 0x01 | 0xE0 | MPEG-1 pictures from their first start code |
  | `Mpeg4Part2` | 0x10 | 0xE0 | MPEG-4 Part 2 visual frames from their first start code |
  | `Av1(Av1Config)` | 0x06 | 0xBD | temporal units in the low overhead format (every OBU has `obu_size`) |
  | `AacAdts` | 0x0F | 0xC0 | ADTS frames; their `frame_length` fields must tile the frame |
  | `Aac { .. }` | 0x0F | 0xC0 | raw AAC frames; the writer adds the ADTS header |
  | `Opus { channels }` | 0x06 | 0xBD | Opus packets, 1 to 8 channels; the writer adds the Opus control header |
  | `OpusMapped(OpusMapping)` | 0x06 | 0xBD | Opus multistream access units with an explicit channel mapping |
  | `Ac3` | 0x81 | 0xBD | AC-3 sync frames |
  | `Eac3` | 0x87 | 0xBD | E-AC-3 sync frames (bsid 11 to 16) |
  | `MpegAudio { low_sample_rate }` | 0x03 or 0x04 | 0xC0 | MPEG audio frames, layers I to III, whose version matches the stream type |

  `Codec::aac_from_asc` turns an AudioSpecificConfig into the `Aac` fields.

  The program map section carries these descriptors:
  - AC-3 and E-AC-3 streams carry a registration descriptor with the format identifier `AC-3` or
    `EAC3`.
  - Opus streams carry a registration descriptor `Opus` and an extension descriptor (tag 0x7F,
    extension tag 0x80). For `Opus { channels }` its channel_config_code is the channel count.
    For `OpusMapped` it is 0x81, followed by the channel count, the mapping family, the stream
    and coupled stream counts and the channel mapping, each packed in the bit widths the draft
    gives.
  - AV1 streams carry a registration descriptor `AV01` and the AV1 video descriptor (tag 0x80)
    built from `Av1Config`. `Av1Config::from_av1c` reads the first four bytes of an
    AV1CodecConfigurationRecord.
  - A stream may also carry an ISO 639 language descriptor.
- For AV1, each OBU follows the start code 0x000001 with emulation prevention bytes, temporal
  delimiters are left out, and each PES packet holds one temporal unit with
  data_alignment_indicator set. A keyframe must carry a sequence header, and tile list OBUs are
  refused.
- For Opus, `Muxer::write_frame_with_trim` writes the control header's start and end trims. A
  start trim is allowed on the stream's first frame, and on a later frame only while every earlier
  frame was trimmed whole; the start trims add up to at most 65 535 samples. After
  `Muxer::discontinuity` the stream starts again and may carry a new start trim. Each trim is at most
  8 191 samples (`MAX_OPUS_TRIM`), and a frame's two trims together are no longer than the frame.
- The PCR on `Config::pcr_pid`: one of the streams' PIDs, or a PID that carries only PCRs. On a
  stream's PID the PCR goes in the adaptation field of each of its frames, and in packets with
  only an adaptation field when its frames are further apart than `Config::pcr_interval`. PCRs
  are never more than `pcr_interval` apart, and the interval is at most 100 ms (§2.7.2). The PCR
  is the largest DTS so far minus `Config::pcr_delay`, which is 1 to 90 000 ticks
  (`MAX_PCR_DELAY`, one second, the T-STD limit of §2.4.2.6). A live output calls
  `Muxer::advance` while no frames arrive. Its time is on the DTS scale, so the PCR it writes is
  that time minus `pcr_delay`.
- A continuity counter per PID, which counts only packets with a payload; the random access
  indicator in the first packet of each keyframe; and adaptation field stuffing in each frame's
  last packet.
- `Muxer::discontinuity` starts a new time base. The next packet of every stream PID and of the
  PCR PID sets the discontinuity_indicator (§2.4.3.5), and timestamps may then go back.

Timestamps are 90 kHz ticks and are written modulo 2^33.

## Output and failures

The writer builds packets in a buffer of `Config::batch_packets` packets (7 by default, 1 316
bytes, one UDP datagram for SRT) and hands each full buffer to the output in one `write` call,
repeated only when the output takes part of it. Each `write_frame`, `advance` and `finish` call also passes on what is left, so the output
holds every packet of a frame when the call returns. With `Config::hold_packets`, packets stay in
the buffer until it fills or `Muxer::flush` is called. Over a file or socket, wrap the output in a
`BufWriter` when `batch_packets` is small. The buffer is allocated once, in `Muxer::new`.

A call that returns `Error::Io` drops the packets of that call that the output did not take, and
the output may hold part of a frame or of a packet. `Stats::packets` and `Segment::packets` count
only the whole packets an output took. `Muxer::replace_writer` hands back the old output in a
`Segment`: its packet and frame counts, its first and last PTS, whether it starts with a keyframe,
and any error from passing on its last packets. The new output starts with the PAT, the PMT and a
PCR, and a reader can read it without the old one. `Muxer::replace_writer_continuing` swaps the
output without adding tables or a PCR, for an output that continues the same stream.

## Arrival order

Frames are written in the order they arrive. A stream's DTS must not go back, and a video
stream's DTS must increase. Across streams, a frame may be up to `pcr_delay` behind the largest
DTS written so far. A frame further behind, or behind a time given to `advance`, is refused with
`Error::Timestamp`. Interleave the streams within `pcr_delay` of each other, or raise it.

## Limits and errors

- 1 to 32 streams. PIDs are 0x0010 to 0x1FFE and must differ (`Error::BadPid`,
  `Error::DuplicatePid`).
- Audio frames must fit in one PES packet (65 535 bytes including the PES header). An Opus frame's
  limit leaves room for the control header and its trims. Video frames may be up to
  `MAX_VIDEO_FRAME` (128 MiB), counting the start codes, emulation prevention bytes and access
  unit delimiter the writer adds. A larger frame is refused with `Error::FrameTooLarge`, which names
  the limit.
- A frame whose DTS is after its PTS is refused.
- Frames are checked for their codec's start code, sync word or OBU structure before anything is
  written.
- A gap in the timeline longer than `MAX_PCR_FILL` PCR intervals gets one PCR at its end and is
  counted in `Stats::pcr_gaps`.

## Gaps

- One program per stream; there is no multi-program output.
- No constant bit rate output: no null packets are added.
- Not written: LPCM (H.222.0 defines no LPCM stream type for transport streams), AAC in LATM, DTS,
  subtitles and teletext.
- craft-media's demuxer at `7780b4f5` reads Opus and AV1 streams as `Unknown(6)` and MPEG-4 Part 2
  as `Unknown(0x10)`, one unit per PES packet. The tests read those units back byte for byte.

## Defaults

| Setting | Value | Source |
| --- | --- | --- |
| Program number, PMT PID | 1, 0x1000 | estimated |
| PCR PID | the first video stream, or the first stream | estimated |
| PCR interval | 40 ms (largest 100 ms) | estimated (the limit: H.222.0 §2.7.2) |
| PCR delay | 700 ms (largest 1 s) | estimated (the limit: H.222.0 §2.4.2.6) |
| PAT and PMT interval | 100 ms, and before keyframes | estimated |
| Packets per write | 7 (largest 1 024) | estimated |

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
the PCR range, with no continuity or CRC warnings. It also reads back AV1 units with their start
codes and emulation prevention bytes removed, Opus with an explicit mapping and a start trim, both
sides of a discontinuity, and the output that follows an I/O error. `tests/fuzz.rs` muxes random
programs and timelines with idle gaps and output swaps and reads them back, and feeds random bytes
to every codec.

## Specifications

- ITU-T H.222.0 | ISO/IEC 13818-1, Information technology: Generic coding of moving pictures
  and associated audio information: Systems.
- ISO/IEC 13818-7, Advanced Audio Coding, for the ADTS header, and ISO/IEC 14496-3, Audio, for
  the AudioSpecificConfig.
- Opus Interactive Audio Codec Transport Multiplexing Standard, draft TS V0.1.3 (2015-10), for
  the Opus control header and descriptors, and RFC 6716, Definition of the Opus Audio Codec, for
  the packet durations.
- Carriage of AV1 in MPEG-2 TS (Alliance for Open Media working draft), for the AV1 stream type,
  descriptors and start code format, and AV1 Bitstream & Decoding Process Specification for the
  OBU syntax.
- ATSC A/52, Digital Audio Compression (AC-3, E-AC-3) Standard, for the AC-3 and E-AC-3 stream
  types and registration descriptors.
