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

## Measuring

- Run the criterion benches with `cargo bench -p <crate>`.
- Run the timing run with `cargo run --release -p studiocraft-cli -- bench <seconds>`.
- Run the encoder bench with `cargo run --release -p studiocraft-encode --example encode_bench -- <seconds> [<threads>]`.
- In the app, View › Stats shows the same counters while you work.

Record new baselines here when a change moves them by more than 10%.
