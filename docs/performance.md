# Performance

This file holds StudioCraft's time budgets, the measured baselines and the commands that measure
them. The rules for threads, queues and hot paths are in
[AGENTS.md section 6](../AGENTS.md#6-performance).

## Budgets

- At 60 fps the compositor has 16.7 ms per frame. Drawing the program scene must stay well under
  that, so encoders and the window have time too.
- One audio block is 1024 frames at 48 kHz, which is 21.3 ms. Mixing every source into one block
  must stay under 1 ms.

## Baselines

The criterion benches and the timing run below were measured on 2026-11-15 on Linux, in release
builds. The encoder and FLV sections name their own dates and hardware.

### Criterion benches

`cargo bench -p studiocraft-media` and `cargo bench -p studiocraft-render`:

| Bench | Median | Range (95%) |
| --- | --- | --- |
| `mix 8 sources, 1024-frame stereo block` | 11.63 µs | 11.627 to 11.632 µs |
| `compositor/1080p 9 items + 1080p upload` | 8.37 ms | 7.93 to 8.85 ms |
| `compositor/1080p 9 items + upload + NV12 read-back` | 11.14 ms | 11.07 to 11.20 ms |

Each compositor bench iteration uploads a new 1080p source frame, draws 9 items and waits until
the GPU has finished. The second bench also converts the program to NV12 and reads it back for an
encoder. In the app the compositor thread does not wait for the GPU after each frame.

### Application audio

Measured on 2026-11-18 on Linux, release build. `cargo bench -p studiocraft-sources --bench
audio_feed` times the work an application capture's thread does for each 10 ms packet: turning 480
stereo frames into 1024-frame blocks and queuing them for the mixer, with the mixer side taking
them.

| Bench | Median | Range (95%) |
| --- | --- | --- |
| `AudioFeed push, 480-frame stereo packet` | 3.77 µs | 3.76 to 3.78 µs |

At 100 packets a second that is about 0.04% of one core for each captured application.
`crates/sources/tests/alloc.rs` checks that a warm feed does not allocate or free memory over
10 seconds of packets. The discovery thread does not run on the audio path: it lists the
applications once a second, at most every 250 ms after system notifications, and reads their
meters 30 times a second only while a caller asks for them (`set_visible` or `read_peaks_for`).

### Audio filters

Measured on 2026-11-21 on Linux, release build. `cargo bench -p studiocraft-sources --bench
audio_filters` times the mixer thread's work for one 1024-frame stereo block through each filter
that reports gain reduction, including the write of its reading.

| Bench | Median | Range (95%) |
| --- | --- | --- |
| `Noise gate, 1024-frame stereo block` | 3.08 µs | 3.05 to 3.11 µs |
| `Compressor, 1024-frame stereo block` | 22.97 µs | 22.89 to 23.05 µs |
| `Limiter, 1024-frame stereo block` | 56.43 µs | 56.25 to 56.66 µs |

At 46.875 blocks a second the limiter, the most expensive of the three, takes about 0.26% of one
core for each source that carries it. Each filter writes its reading with one atomic store per
block, and readers load it without a lock. `crates/sources/tests/filter_meter_alloc.rs` checks
that the three filters do not allocate or free memory over about 10 seconds of blocks.

### Timing run

`studiocraft-cli bench 10` draws a 1080p scene with a background, four overlapping translucent
boxes (one rotated) and nothing else, at 60 fps for 10 seconds, without a window:

| Measure | Value |
| --- | --- |
| Frames drawn | 595 |
| Frames drawn late | 5 |
| Average frame time | 0.114 ms |
| Last frame time | 0.137 ms |

The frame time is the time the compositor thread spends on one frame after its tick, from the
start of drawing to the end of submitting it to the GPU.

### GPU YUV conversion

`cargo bench -p craft-gpu-yuv` times four conversions on the GPU: `gpu nv12 1080p`,
`gpu p010 1080p`, `gpu nv12 4k`, and `gpu nv12 4k to 1080p`, which scales a 4K picture down to
1080p. Each timing runs from the conversion passes to a mapped read-back buffer and includes the
read-back. The bench also times `cpu nv12 1080p`, a reference conversion on the CPU. On a machine
without a graphics adapter only the CPU case runs. This page records no baseline for these
benches yet.

### Audio tracks

Measured on 2026-11-17 on Linux with `cargo bench -p studiocraft-media --bench mixer`, in one run
on one machine:

| Bench | Median | Range (95%) |
| --- | --- | --- |
| `mix 8 sources, 1024-frame stereo block` | 12.87 µs | 12.77 to 12.97 µs |
| `mix_8_sources_taps_off` | 12.97 µs | 12.78 to 13.17 µs |
| `mix_8_sources_8_taps` | 12.89 µs | 12.71 to 13.06 µs |

`mix_8_sources_taps_off` is the mixing pass with audio tracks off, where each input checks for a
tap and finds none. `mix_8_sources_8_taps` stores every input's block into its tap in the same
pass that mixes it and copies the program into its own tap. The three medians are within 0.1 µs
of each other, inside the run-to-run spread.

Measured on 2026-11-18 on Linux with `cargo bench -p studiocraft-engine --bench mixer_taps` and
`cargo bench -p studiocraft-audio-tracks --bench collect`, in one run on one machine:

| Bench | Median | Range (95%) |
| --- | --- | --- |
| `mix_state_8_inputs_taps_off` | 120.19 µs | 118.36 to 122.11 µs |
| `mix_state_8_inputs_9_taps` | 137.04 µs | 134.37 to 139.65 µs |
| `mix_state_63_inputs_64_taps` | 617.29 µs | 612.12 to 623.00 µs |
| `collect_9_tracks_one_block` | 14.47 µs | 13.84 to 15.41 µs |

The `mix_state` benches run the mixer thread's whole tick (`MixState::tick`) for one 1024-frame
block: input levels, mixing, the loudness meter and the taps. In the tapped cases a second thread
drains every tap while the tick runs, as the collect thread does. Taps on 8 inputs and the program
add 16.9 µs to the tick. 64 tracks, the most a take records at once, take 617 µs of the 21.3 ms
between blocks. `collect_9_tracks_one_block` is the collect thread taking one block from each of 9
taps and placing it in its chunk, once every 21.3 ms.

`studiocraft-cli bench audio-tracks` feeds 9 tracks at the mixer's pace for 60 seconds while the
disk takes nothing for 40 seconds from second 5, with the default 96 MiB write buffer. A run on
2026-11-18 gave the same values as the run on 2026-11-17:

| Measure | Value |
| --- | --- |
| Seconds the buffer holds for 9 tracks | 29.1 s |
| Audio lost per track during the 40 s stall | 13.2 s |
| Every file the full 60 s long | yes |
| Feeder late | 0 ms |

Measured on 2026-11-19 on Linux with `cargo bench -p studiocraft-audio-tracks --bench session`, in
one run on one machine, for a 24-hour take of 9 tracks in 1 GiB parts (225 files) with 1,000
markers and 100 dropouts:

| Bench | Median | Range (95%) |
| --- | --- | --- |
| `session_build_9_tracks_24h_1000_markers` | 233.88 µs | 232.42 to 235.52 µs |
| `session_write_9_tracks_24h_1000_markers` | 10.08 ms | 9.84 to 10.34 ms |

The disk thread builds and writes the session once when a take starts and once after its files
are complete, so neither runs on the per-block path. The write includes serializing the session
and replacing the file with `craft-atomic`.

The lost audio is replaced with silence in place, so each file still has the take's full length.
A longer stall loses more; a larger buffer (up to 1024 MiB) holds more.

### Built-in H.264 encoder

This bench was measured on 2026-11-17 on Linux (AMD EPYC 9R14, 128 hardware threads), in a
release build. `cargo run --release -p studiocraft-encode --example encode_bench -- 3 <threads>` codes 3 seconds
of moving pictures per case after one second of warm-up, in quality mode at level 23. The frames
cycle through one second of different pictures. A recording runs the encoder on the computer's
hardware threads minus two, so 6 threads stands for an 8-thread computer and 14 threads for a
16-thread one. The other cores of this machine were idle. The table gives the share of real time:
the time spent coding over the length of the video. A recording keeps up only while the share is
under 100%. A recording that stays above 100% fails after the video falls 10 seconds behind the
sound.

| Size | Frame rate (fps) | Preset | 6 threads | 14 threads | 126 threads |
| --- | --- | --- | --- | --- | --- |
| 1280 × 720 | 30 | Speed | 42% | 25% | 30% |
| 1280 × 720 | 30 | Balanced | 72% | 40% | 45% |
| 1280 × 720 | 30 | Quality | 133% | 70% | 74% |
| 1280 × 720 | 60 | Speed | 92% | 50% | 56% |
| 1280 × 720 | 60 | Balanced | 158% | 77% | 87% |
| 1280 × 720 | 60 | Quality | 265% | 132% | 142% |
| 1920 × 1080 | 30 | Speed | 92% | 45% | 49% |
| 1920 × 1080 | 30 | Balanced | 164% | 74% | 74% |
| 1920 × 1080 | 30 | Quality | 294% | 130% | 107% |
| 1920 × 1080 | 60 | Speed | 174% | 86% | 88% |
| 1920 × 1080 | 60 | Balanced | 320% | 154% | 133% |
| 1920 × 1080 | 60 | Quality | 571% | 256% | 223% |

With 6 threads, 720p at 30 fps keeps up on Speed and Balanced. 1080p at 30 fps and 720p at 60 fps
keep up on Speed only, at 92% of real time. 1080p at 60 fps does not keep up on any preset. With
14 threads or more, 1080p at 60 fps keeps up on Speed only, and 1080p at 30 fps keeps up on Speed
and Balanced. Going from 14 to 126 threads changes the share by between 10 points slower and
33 points faster in the encoder table. The encoder splits a picture into at most one slice per four
rows of macroblocks (17 slices at 1080p, 12 at 720p). Speed is the encoder's default preset.

### VA-API encoder

Measured on 2026-11-17 on the same machine, which has no VA-API device. `cargo bench -p
craft-hwenc-native` measures the work the VA-API encoder does on the CPU for each frame:

| Bench | Median | Range (95%) |
| --- | --- | --- |
| `copy nv12 1080p` | 80.3 µs | 79.8 to 80.8 µs |
| `copy nv12 4k` | 1.22 ms | 1.17 to 1.26 ms |
| `packet 1080p p picture` | 7.73 µs | 7.65 to 7.81 µs |

The copy moves a frame into an H.264 surface's planes and fills the rows past 1080 with the last
row, and the packet bench turns a 12.5 kB coded
picture into a length-prefixed packet. At 1080p and 60 fps the two take about 0.5% of the frame
time. The copy bench writes into heap memory; a mapped VA surface can be uncached memory, where
writes are slower, and the bench does not measure that. On a machine with a VA-API H.264 encoder the bench also measures `vaapi h264 encode 1080p`,
a whole frame from `encode`.

### FLV tags

This bench was measured on 2026-11-18 on Linux (AMD EPYC 9R14), in a release build.
`cargo bench -p craft-flv` writes and reads one second of a 6000 kb/s, 60 fps H.264 stream with
160 kb/s AAC: 60 video tags of about
12 500 bytes and 47 audio tags of 426 bytes, 770 KB in all, into buffers that keep their capacity.

The destination benches send the same second to 4 outputs:

- "Annex B per destination" gives every output the Annex B access units, so each output scans them
  for start codes twice, once to measure the tag and once to write it.
- "Converted once" converts each access unit with `annexb_to_nalus` once, checks each body once
  with `Body::check`, and writes the one result to every output with `Writer::write_checked`, or
  with `Checked::write_to` for RTMP message bodies.
- "With zero bytes" uses pseudo-random slice data with emulation prevention applied, so about one
  byte in 256 is 0, as in entropy-coded slices. The other Annex B input has no zero bytes in its
  slices.

| Bench | Median | Range (95%) |
| --- | --- | --- |
| `flv/write 1 s, Annex B input` | 191.0 µs | 190.4 to 191.6 µs |
| `flv/write 1 s, Annex B input with zero bytes` | 198.2 µs | 197.3 to 199.3 µs |
| `flv/write 1 s, length-prefixed input` | 15.78 µs | 15.71 to 15.85 µs |
| `flv/write 1 s to 4 destinations, Annex B per destination` | 820.5 µs | 815.0 to 826.0 µs |
| `flv/write 1 s to 4 destinations, converted once` | 170.5 µs | 169.5 to 171.5 µs |
| `flv/RTMP bodies 1 s to 4 destinations, converted once` | 161.1 µs | 160.4 to 162.0 µs |
| `flv/write 1 s through a BufWriter, length-prefixed input` | 15.56 µs | 15.52 to 15.61 µs |
| `flv/read and parse 1 s` | 15.53 µs | 15.49 to 15.57 µs |

With each access unit converted once and the result shared, sending the stream to 4 destinations
costs 170.5 µs of one core per second of stream, or 2.8 µs per video frame. Converting it for each
destination costs 820.5 µs. Runs on this host vary by up to 15% from one run to the next.

### Packet fan-out

Measured on 2026-11-18 on Linux (AMD EPYC 9R14), release build. `cargo bench -p craft-packet`
times the producer's thread for one second of a 60 fps stream, 60 video packets of 12 500 bytes and
47 audio packets of 426 bytes (107 packets), with new timestamps each second. A counting allocator
checks the timed parts, and the bench exits with an error if one allocates.

- `fanout`: publishing to 1, 4, 8 and 16 subscribers. One subscriber never reads, so its 16-packet
  ring stays full and it drops packets up to each keyframe. The other rings hold 330 packets and
  3.5 MB and are drained between seconds on the bench thread, outside the timing.
- `parked`: publishing to receivers that each run on their own thread and park in `recv_timeout`
  between packets, as a network sender that keeps up does. Before each packet the bench waits,
  outside the timing, until every receiver is parked, so the timing includes every wake.
- `share`: sharing each packet from the pool (video with `share`, audio with `share_copy`) and
  publishing it to one subscriber, without and with a stalled subscriber whose ring holds 10 000
  packets.

| Bench | Median | Range (95%) |
| --- | --- | --- |
| `fanout/publish 1 s to 1 subscriber that never reads` | 1.234 µs | 1.229 to 1.240 µs |
| `fanout/publish 1 s to 4 subscribers, one never reads` | 6.464 µs | 6.443 to 6.488 µs |
| `fanout/publish 1 s to 8 subscribers, one never reads` | 14.47 µs | 14.30 to 14.65 µs |
| `fanout/publish 1 s to 16 subscribers, one never reads` | 31.32 µs | 30.93 to 31.76 µs |
| `parked/publish 1 s to 1 parked receiver` | 523.1 µs | 487.2 to 554.3 µs |
| `parked/publish 1 s to 4 parked receivers` | 983.0 µs | 939.1 µs to 1.038 ms |
| `parked/publish 1 s to 8 parked receivers` | 1.952 ms | 1.885 to 2.021 ms |
| `parked/publish 1 s to 16 parked receivers` | 4.404 ms | 4.164 to 4.656 ms |
| `parked/publish 1 s to 8 parked receivers, woken every 3 packets` | 615.8 µs | 596.0 to 636.8 µs |
| `parked/publish 1 s to 16 parked receivers, woken every 3 packets` | 1.135 ms | 1.110 to 1.161 ms |
| `share/share and publish 1 s, stalled ring holds 0 packets` | 14.87 µs | 14.76 to 15.01 µs |
| `share/share and publish 1 s, stalled ring holds 10000 packets` | 15.06 µs | 15.06 to 15.07 µs |

Publishing a packet to 8 subscribers that do not need waking costs 135 ns, about 17 ns per
subscriber, and allocates nothing. The packet's bytes are not copied. Waking a parked receiver
costs the producer far more: 4.9 µs for one receiver, and about 2.3 µs per receiver when 8 are
woken for each packet, 0.20% of one core for one second of stream. With `wake_every` at 3, which
`RingConfig::for_stream` sets for this stream, 8 receivers cost 0.062% of one core and 16 cost
0.11%. Sharing a packet from the pool costs about the same with a stalled ring of 10 000 packets
as without one. The `parked` timings depend on the load on the host's other cores: another run on
the same host measured 5.630 ms for 8 parked receivers, 2.9 times the figure above.

### RTMP chunking

Measured on 2026-11-18 on Linux (AMD EPYC 9R14), release build. `cargo bench -p craft-rtmp`
chunks one second of a 6000 kb/s, 60 fps H.264 stream with 160 kb/s AAC with `ChunkWriter`: 60
video messages of 12 505 bytes and 47 audio messages of 428 bytes, 770 KB in all, each written as
its codec bytes and its data.

- `sink`: into `io::sink()` at the 4096-byte chunk size the publisher sets and at the default 128
  bytes, which times building the chunk headers and slices.
- `vec`: into a `Vec` that keeps its capacity, which adds copying every byte.
- `tcp`: into a loopback TCP socket that another thread drains, which adds the system calls.

| Bench | Median | Range (95%) |
| --- | --- | --- |
| `sink/chunk 1 s at 4096 bytes` | 4.509 µs | 4.439 to 4.582 µs |
| `sink/chunk 1 s at 128 bytes` | 23.38 µs | 22.99 to 23.71 µs |
| `vec/chunk 1 s at 4096 bytes into a Vec` | 17.80 µs | 17.54 to 18.08 µs |
| `tcp/send 1 s at 4096 bytes over loopback TCP` | 1.253 ms | 1.194 to 1.304 ms |

At the 4096-byte chunk size, chunking one second of stream costs 4.5 µs of one core, about 42 ns
per message, and allocates nothing. Sending it over loopback TCP costs 1.25 ms, 0.13% of one core,
almost all of it in the system calls. An earlier run on the same host measured 1.505 ms for the
TCP bench.

### Face tracking

Measured on 2026-11-24 on Linux (AMD EPYC 9R14), release build. `cargo bench -p craft-face-track
--bench tracker` with `CRAFT_FACE_LANDMARKER` set to the Face Landmarker bundle runs the models on
a 1280 × 720 picture. The detector reads the whole picture and the mesh a 420-pixel crop of it. A
followed picture is one crop and mesh run and the head pose fit. The tracker also works out the
expression weights, smooths them and fills its output for each followed picture; the bench does
not measure that part.

| Bench | Median | Range (95%) |
| --- | --- | --- |
| `detector_1_threads` | 5.194 ms | 5.161 to 5.229 ms |
| `mesh_1_threads` | 22.04 ms | 21.89 to 22.19 ms |
| `followed_picture_1_threads` | 22.56 ms | 22.45 to 22.68 ms |
| `detector_2_threads` | 4.476 ms | 4.449 to 4.503 ms |
| `mesh_2_threads` | 23.08 ms | 22.50 to 23.68 ms |
| `followed_picture_2_threads` | 21.52 ms | 21.22 to 21.82 ms |

At 30 fps a picture arrives every 33.3 ms. The crop, mesh and pose fit of a followed picture take
21.5 ms on 2 threads, which leaves 11.8 ms for the expression weights, the smoothing and the
output. A picture in which the detector
checks a weakly held face takes about 26 ms. A search, which runs while no face is followed, takes
the detector and two mesh runs, about 49 ms. The second thread saves 1 ms on a followed picture.
`tests/no_alloc.rs` counts
no allocation in the models, the pose fit or a search once their buffers are warm, on 1 thread and
on 2.

## Measuring

- Run the criterion benches with `cargo bench -p <crate>`.
- Run the timing run with `cargo run --release -p studiocraft-cli -- bench <seconds>`.
- Run the audio tracks stall run with `cargo run --release -p studiocraft-cli -- bench audio-tracks`.
- Run the encoder bench with `cargo run --release -p studiocraft-encode --example encode_bench -- <seconds> [<threads>]`.
- In the app, View › Stats shows the frames drawn, the frames drawn late and the last and average
  frame times while you work.

Record new baselines here when a change moves them by more than 10%.
