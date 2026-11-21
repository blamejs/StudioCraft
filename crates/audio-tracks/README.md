# studiocraft-audio-tracks

Writes each recorded source, and the program mix, to its own WAV file (48 kHz, stereo, 32-bit
float) while a recording or the stream runs. The files go to
`<recording folder>/<take name> Audio/Audio Files/`, with `00 Program.wav`, one file per source and
`take.journal`.

The mixer stores each tapped input's block in a tap (`studiocraft_media::tap`). The collect thread
moves the blocks into 1 MiB chunks, and the disk thread writes them, rewrites the headers every
second, and syncs the files and the journal every 5 seconds. See the table in `src/lib.rs` and
`docs/architecture.md` for the threads and queue sizes, and `docs/performance.md` for the measured
costs.

- `recover::repair_all(folder)` repairs takes that ended without the journal's last line. The app
  runs it on its recording folder at launch, on a background thread, and
  `studiocraft-cli audio-tracks repair <folder>` runs it from the command line. The disk thread
  holds a lock on `take.journal` for the whole take, and repair skips a take whose journal is
  locked, so it does not modify a take that another thread or process is still writing.
- `takes::list(folder)` lists the takes in a recording folder, newest first: each take's name,
  start time, length, track count, size, dropouts, markers and missing files, and whether it
  finished, needs repair or is still being written, read from its journal and the lengths of its
  files. It reads at most 500 entries of the folder, looks only at
  `<entry>/Audio Files/take.journal` in each, does not follow links, and counts a take whose
  journal cannot be read instead of stopping.
- `cargo bench -p studiocraft-audio-tracks` measures the collect thread's work per block.
