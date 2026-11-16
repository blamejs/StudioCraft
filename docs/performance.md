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

## Measuring

- Run the criterion benches with `cargo bench -p <crate>`.
- Run the timing run with `cargo run --release -p studiocraft-cli -- bench <seconds>`.
- In the app, View › Stats shows the same counters while you work.

Record new baselines here when a change moves them by more than 10%.
