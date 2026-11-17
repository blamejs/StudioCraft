# StudioCraft

StudioCraft records and live-streams scenes. A scene is a stack of sources: displays, windows,
pictures, text, colors, media files and audio devices. You arrange the sources on
a canvas, switch between scenes with transitions, mix the audio, and record to a file or stream to
an RTMP server.

StudioCraft is written in Rust and draws its window with egui and wgpu. It runs on Windows, macOS
and Linux.

## Features

- **Scenes and sources.** Add, rename, duplicate, reorder and remove scenes and sources. Drag,
  resize and rotate sources in the preview with snapping to the canvas edges and center. Lock a
  source so it can't be moved, or lock the whole preview.
- **Built-in sources.** Color, image, text, media file, display capture, window capture, audio
  input and audio output (desktop audio). Display, window and device capture work on Windows and
  macOS.
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
  audible source.
- **Recording and streaming.** MKV, MP4 or MOV recordings at four quality levels, and RTMP streams
  at a constant bitrate. With the H.264 (built in) encoder, StudioCraft records MKV files at the
  Compact, High and Pristine levels without `ffmpeg`, using its own H.264 and AAC encoders and a
  Speed, Balanced or Quality preset. MP4 and MOV recordings, Lossless recordings and streams run in a
  separately installed `ffmpeg` with the software x264 encoder or a hardware encoder (NVIDIA NVENC,
  AMD AMF, Intel Quick Sync, Windows Media Foundation). x264 is the default encoder for recordings
  and streams. Settings › Output and Settings › Stream show the chosen encoder's settings, such as
  the x264 and NVENC presets. Tools › Available Encoders lists H.264 (built in) and the encoders the
  installed `ffmpeg` offers, and says when `ffmpeg` was not found.
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
fontconfig and ALSA.

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
