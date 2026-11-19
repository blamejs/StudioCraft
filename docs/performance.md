# Performance

## Budgets

- At 60 fps the compositor has 16.7 ms per frame. Drawing the program scene must stay well under
  that, so encoders and the window have time too.
- One audio block is 1024 frames at 48 kHz, which is 21.3 ms. Mixing every source into one block
  must stay under 1 ms.
- The frame loop and the mixer loop do not allocate. Frames and audio blocks come from pools.

## Baselines

Measured on 2026-11-15 on Linux, release builds.

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

The lost audio is replaced with silence in place, so each file still has the take's full length.
A longer stall loses more; a larger buffer (up to 1024 MiB) holds more.

### Built-in H.264 encoder

Measured on 2026-11-17 on Linux (AMD EPYC 9R14, 128 hardware threads), release build.
`cargo run --release -p studiocraft-encode --example encode_bench -- 3 <threads>` codes 3 seconds
of moving pictures per case after one second of warm-up, in quality mode at level 23. The frames
cycle through one second of different pictures. A recording runs the encoder on the computer's
hardware threads minus two, so 6 threads stands for an 8-thread computer and 14 threads for a
16-thread one; the other cores of this machine were idle. The table gives the share of real time:
the time spent coding over the length of the video. A recording keeps up only while it is under
100%; one that stays above it fails after the video falls 10 seconds behind the sound.

| Size | Frame rate | Preset | 6 threads | 14 threads | 126 threads |
| --- | --- | --- | --- | --- | --- |
| 1280×720 | 30 | Speed | 42% | 25% | 30% |
| 1280×720 | 30 | Balanced | 72% | 40% | 45% |
| 1280×720 | 30 | Quality | 133% | 70% | 74% |
| 1280×720 | 60 | Speed | 92% | 50% | 56% |
| 1280×720 | 60 | Balanced | 158% | 77% | 87% |
| 1280×720 | 60 | Quality | 265% | 132% | 142% |
| 1920×1080 | 30 | Speed | 92% | 45% | 49% |
| 1920×1080 | 30 | Balanced | 164% | 74% | 74% |
| 1920×1080 | 30 | Quality | 294% | 130% | 107% |
| 1920×1080 | 60 | Speed | 174% | 86% | 88% |
| 1920×1080 | 60 | Balanced | 320% | 154% | 133% |
| 1920×1080 | 60 | Quality | 571% | 256% | 223% |

With 6 threads, 720p at 30 fps keeps up on Speed and Balanced, and 1080p at 30 fps and 720p at
60 fps keep up on Speed only, near the limit. 1080p at 60 fps does not keep up on any preset. With
14 threads or more, 1080p at 60 fps keeps up on Speed only, and 1080p at 30 fps on Speed and
Balanced. More than 14 threads gives little: the encoder splits a picture into at most one slice
per four rows of macroblocks (17 slices at 1080p, 12 at 720p). Speed is the encoder's default
preset.

### FLV tags

Measured on 2026-11-18 on Linux (AMD EPYC 9R14), release build. `cargo bench -p craft-flv` writes
and reads one second of a 6000 kb/s, 60 fps H.264 stream with 160 kb/s AAC: 60 video tags of about
12 500 bytes and 47 audio tags of 426 bytes, 770 KB in all, into buffers that keep their capacity.

The destination benches send the same second to 4 outputs. "Annex B per destination" gives every
output the Annex B access units, so each output scans them for start codes twice, once to measure
the tag and once to write it. "Converted once" converts each access unit with `annexb_to_nalus`
once, checks each body once with `Body::check`, and writes the one result to every output with
`Writer::write_checked`, or with `Checked::write_to` for RTMP message bodies. "With zero bytes" uses
pseudo-random slice data with emulation prevention applied, so about one byte in 256 is 0, as in
entropy-coded slices; the other Annex B input has no zero bytes in its slices.

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

## Measuring

- Run the criterion benches with `cargo bench -p <crate>`.
- Run the timing run with `cargo run --release -p studiocraft-cli -- bench <seconds>`.
- Run the audio tracks stall run with `cargo run --release -p studiocraft-cli -- bench audio-tracks`.
- Run the encoder bench with `cargo run --release -p studiocraft-encode --example encode_bench -- <seconds> [<threads>]`.
- In the app, View › Stats shows the same counters while you work.

Record new baselines here when a change moves them by more than 10%.
