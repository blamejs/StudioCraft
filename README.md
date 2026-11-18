# StudioCraft

StudioCraft records and live-streams scenes. A scene is a stack of sources: displays, windows,
cameras, pictures, text, colors, media files and audio devices. You arrange the sources on
a canvas, switch between scenes with transitions, mix the audio, and record to a file or stream to
an RTMP server.

StudioCraft is written in Rust and draws its window with egui and wgpu. It runs on Windows, macOS
and Linux.

## Features

- **Scenes and sources.** Add, rename, duplicate, reorder and remove scenes and sources. Drag,
  resize and rotate sources in the preview with snapping to the canvas edges and center. Lock a
  source so it can't be moved, or lock the whole preview.
- **Built-in sources.** Color, image, text, media file, display capture, window capture, camera,
  audio input and audio output (desktop audio). Display and window capture work on Windows, macOS
  and Linux. The camera source captures webcams and capture cards through Media Foundation on
  Windows, AVFoundation on macOS and V4L2 on Linux. Its Properties window lists the cameras and
  the chosen camera's formats (picture size, frame rate and pixel encoding: MJPEG, YUYV, NV12,
  gray, RGB or BGR; on macOS, YUYV only). The window reads these lists in the background and
  shows "Reading the lists…" meanwhile. While the window is open, a camera plugged in or out
  updates the lists within about 3 seconds. With Format at Default, the camera opens at its largest picture up to
  1920 × 1080 at 30 fps or more. A saved camera that is not plugged in shows as "(not
  connected)". A camera that is missing, in use by another program, unplugged or sending nothing
  for 5 seconds is reported in the Properties window and opened again every 2 seconds. One camera
  shows in one camera source at a time. On macOS
  the system asks for camera access the first time a camera is listed. Audio input and output capture work on Windows and Linux, and audio input capture on
  macOS. On Linux, audio capture goes through the PipeWire sound server and lists its devices by
  their descriptions. Display and window capture on Linux go through the desktop's screen-cast
  portal (`xdg-desktop-portal` with a backend for your desktop): the display or window is chosen in
  the desktop's own picker when the source starts. After the picker is canceled or sharing is
  stopped, changing any of the source's properties opens the picker again.
- **Filters.** Color correction, chroma key, color key, crop and pad, image mask, scroll, and
  sharpen for video. Color correction sets gamma, contrast, brightness, saturation, opacity, and the white
  balance by temperature and tint. Color key makes the pixels near a key color transparent by their
  RGB distance from it, with Similarity and Smoothness, and adjusts the rest by gamma, contrast,
  brightness and opacity. Crop and pad takes pixels off each edge of a source (a positive value) or
  adds transparent pixels (a negative value), or shows one area of it, and the source takes the new
  size in every scene. Image mask uses a picture file as an alpha mask (from its brightness or its
  alpha) or blends it with the source (multiply, add or subtract), stretched over the source or
  centered at its own size, then applies a color and an opacity. Scroll moves a source's picture
  at a horizontal and a vertical speed in pixels per second, repeating it (Loop) or moving it out
  once, in an area whose width and height can be limited. Sharpen makes edges crisper by a
  Sharpness from 0 to 1.
  Gain, noise gate, compressor, limiter and a three-band equalizer for audio.
- **Transitions.** Cut, fade, fade through a color, and swipe, with a duration from 0 to 20 seconds.
  Fade, fade through a color and swipe have an Easing setting: Linear, Ease in, Ease out, or Ease
  in and out.
- **Studio mode.** Edit a preview scene beside the program scene, then send it to the program with
  the chosen transition, or with a quick cut or fade.
- **Audio mixer.** Peak and RMS meters with peak hold, a volume fader in dB, and mute for every
  audible source. A source's Properties window sets its sync offset, which delays its audio by 0
  to 5000 ms to line it up with its picture, and its monitoring: off, monitor only (you hear it,
  and recordings and streams leave it out) or monitor and output. Monitored sources play on the
  device chosen in Settings › Audio. Device output works on Windows and macOS.
- **Recording and streaming.** MKV, MP4 or MOV recordings at four quality levels, and RTMP streams
  at a constant bitrate. With the H.264 (built in) encoder, StudioCraft records MKV files at the
  Compact, High and Pristine levels without `ffmpeg`, using its own H.264 and AAC encoders and a
  Speed, Balanced or Quality preset. MP4 and MOV recordings, Lossless recordings and streams run in a
  separately installed `ffmpeg` with the software x264 encoder or a hardware encoder (NVIDIA NVENC,
  AMD AMF, Intel Quick Sync, Windows Media Foundation, VideoToolbox, VA-API). The Hardware encoder
  runs the one set in Settings › Video › Hardware acceleration; Automatic tests each with one frame
  and falls back to x264. NVIDIA NVENC H.264 and HEVC (built in) record MKV files through the
  NVIDIA driver without `ffmpeg` on Windows and 64-bit Linux, and the Hardware encoder uses them for
  MKV recordings. Windows Media Foundation H.264 and HEVC (built in) do the same on Windows with the
  hardware encoders Media Foundation lists, Apple VideoToolbox H.264 and HEVC (built in) do the
  same on macOS with the VideoToolbox hardware encoders, and VA-API H.264 and HEVC (built in) do the
  same on 64-bit Linux through libva. x264 is the default encoder for recordings
  and streams. Settings › Output and Settings › Stream show the chosen encoder's settings, such as
  the x264 and NVENC presets. Tools › Available Encoders lists the built-in encoders and the encoders the
  installed `ffmpeg` offers, and says when `ffmpeg` was not found.
- **Replay buffer.** File › Replay Buffer keeps the last seconds of the program in memory with the
  built-in H.264 and AAC encoders, and File › Save Replay writes them to an MKV file named
  `Replay <date> <time>.mkv` in the recording folder while the buffer keeps running. Settings ›
  Output sets the length (5 to 3600 seconds, 20 by default) and the memory limit (32 to 16384 MB,
  512 by default). Start or Stop Replay Buffer and Save Replay can be given shortcuts in Settings ›
  Hotkeys, and the status bar shows REPLAY with the time it has run.
- **Profiles and scene collections.** Keep several sets of stream and output settings, and several
  sets of scenes, and switch between them from the menu bar.
- **Shortcuts.** Change the shortcuts in Settings › Hotkeys.
- **Statistics.** View › Stats shows the frame count, late frames, frame times, encoded and
  skipped frames, the encoder queue, and the size of the running recording or stream.
- **Scopes.** View › Scopes shows a waveform, an RGB parade, a histogram, or the YUV or HLS
  vectorscope of the program output. The `scopes.kind` command picks the scope.
- **Scene collection files.** Scene Collection › Import… and Export… read and write a scene
  collection as a file.
- **Language.** Settings › General › Language picks the interface language, or follows the system
  language.
- **Help.** Help › Community opens the Discord server, the website, the source code and the issue
  tracker. Help › Check for Updates… looks for a newer release, and Settings › General › Check for
  updates at start does the same when the app starts. Help › About StudioCraft lists the
  contributors and shows the license texts.

## Agents and automation

StudioCraft runs every action as a command with a stable id, and three tools run those commands
from outside the window:

- **Command line.** `studiocraft-cli send <port> <method> [<params>]` sends one control request
  to the running app and prints the reply. `studiocraft-cli check <file>` reports problems in scene
  collection files.
- **Control channel.** `studiocraft --control <port>` accepts JSON requests on
  `127.0.0.1:<port>`. Each request carries the token from `STUDIOCRAFT_CONTROL_TOKEN`. See
  [docs/control-protocol.md](docs/control-protocol.md).
- **MCP server.** `studiocraft-cli mcp` runs its own studio without a window, and
  `studiocraft-cli mcp --connect <port>` drives the app started with `--control <port>`, including
  its window, menus and screenshots. See [docs/mcp.md](docs/mcp.md).

```sh
export STUDIOCRAFT_CONTROL_TOKEN=$(openssl rand -hex 32)
studiocraft --control 9876 &
studiocraft-cli send 9876 engine.execute '{"command": "scene.add", "params": {"name": "Intro"}}'
studiocraft-cli mcp --connect 9876
```

## Building

You need a Rust toolchain (edition 2024) and, on Linux, the development packages for X11, Wayland,
fontconfig, ALSA and PipeWire (`libpipewire-0.3-dev` on Debian and Ubuntu), and libclang.

```sh
cargo run --release
```

Every build includes Inter and JetBrains Mono from `assets/fonts`, which draw Latin, Greek and
Cyrillic text. To build in the faces for other scripts (Arabic, Chinese, Japanese, Korean, Thai and
more), point `CRAFT_FONTS_DIR` at a checkout of
[craft-fonts](https://github.com/storytold/craft-fonts):

```sh
CRAFT_FONTS_DIR=/path/to/craft-fonts cargo run --release
```

StudioCraft's own translation catalogs are in `crates/ui-egui/i18n/locales/<locale>/studiocraft.tsv`,
in craft-i18n's catalog format, and every build includes them. `en/studiocraft.tsv` lists the
English source text of every interface string; a test fails when the window looks up a string that
has no row there. Every locale with a `studiocraft.tsv` is listed in Settings › General › Language.

To add the catalogs the Craft apps share, point `CRAFT_I18N_DIR` at a checkout of
[craft-i18n](https://github.com/storytold/craft-i18n) (Apache-2.0). Its `common.tsv` files are added, and so is
any `studiocraft.tsv` it has; where both catalogs have a row for the same text, the row in this
repository is used. Both paths must be absolute.

```sh
CRAFT_I18N_DIR=/path/to/craft-i18n CRAFT_FONTS_DIR=/path/to/craft-fonts cargo run --release
```

Streaming, and recording with an encoder other than H.264 (built in), need `ffmpeg` on the
`PATH`, or its location in Settings › Output. Recording uses x264 until H.264 (built in) is chosen
in Settings › Output.

## Running

```text
studiocraft [--config-dir <dir> | --no-prefs] [--control <port>]
studiocraft --screenshot <file.png>
studiocraft --self-test <file.json>
studiocraft --version
```

- `--config-dir <dir>` keeps settings, scene collections and profiles in `<dir>`.
- `--no-prefs` keeps everything in memory; nothing is read or written.
- `--control <port>` accepts control requests on `127.0.0.1:<port>`.
- `--screenshot <file>` draws about 120 frames, saves the window as a PNG and exits.
- `--self-test <file>` runs a short scripted session and writes its steps as JSON.

The command-line tool `studiocraft-cli` sends control requests, runs the MCP server, checks scene
collection files and times the compositor without a window:

```sh
cargo run --release -p studiocraft-cli -- help
```

## Checks

```sh
cargo xtask ci
```

This runs `cargo fmt --check`, clippy with warnings as errors, the tests, the crate layer check,
the dependency license check, the asset list check, the product name check and the WebAssembly
build of the model crates.

## Documentation

- [docs/architecture.md](docs/architecture.md): crates, layers, threads and extension points.
- [docs/performance.md](docs/performance.md): timing baselines.
- [docs/control-protocol.md](docs/control-protocol.md): the control channel.
- [docs/mcp.md](docs/mcp.md): registering the MCP server, and its tools, resources and prompts.
- [MECHANICS.md](MECHANICS.md): the timing and tuning values the app uses.
- [ROADMAP.md](ROADMAP.md): what comes next.

## License

StudioCraft is licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
