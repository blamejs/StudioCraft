# Features

This page describes everything StudioCraft does, one section per area. It is for people who use
the app and want the details behind a menu item or a setting. The tuned values, such as timings,
sizes and bitrates, are listed in [MECHANICS.md](../MECHANICS.md).

## Sources

A scene is a stack of sources. You add, rename, duplicate, reorder and remove scenes and sources.
In the preview you drag, resize and rotate sources, and they snap to the canvas edges and center.
You can lock a source so it can't be moved, or lock the whole preview. Each scene item can be
shown or hidden, and every change can be undone.

### Built-in sources

| Source | What it captures | Systems |
| --- | --- | --- |
| Color | A solid color. | All |
| Image | A picture file. | All |
| Text | Text in a chosen font. | All |
| Media file | A media file. | All |
| Display capture | A whole display. | Windows, macOS and Linux |
| Window capture | One window. | Windows, macOS and Linux |
| Camera | A webcam or capture card. | Windows (Media Foundation), macOS (AVFoundation) and Linux (V4L2) |
| Audio input capture | A microphone or other input device. | Windows, macOS and Linux |
| Audio output capture | The sound the computer plays (desktop audio). | Windows and Linux |

### Display and window capture on Linux

On Linux, display and window capture go through the desktop's screen-cast portal
(`xdg-desktop-portal` with a backend for your desktop) and PipeWire. The desktop's own picker
asks for the display or window when the source starts. After the picker is canceled or sharing is
stopped, changing any of the source's properties opens the picker again.

### Audio capture on Linux

On Linux, audio input and output capture go through the PipeWire sound server. The device lists
show PipeWire's devices by their descriptions, with the default device first.

### Camera

The camera source's Properties window lists the cameras and the chosen camera's formats. A format
is a picture size, a frame rate and a pixel encoding: MJPEG, YUYV, NV12, gray, RGB or BGR. On
macOS only YUYV formats are offered. The window reads these lists in the background and shows
"Reading the lists…" until they arrive.

The camera source behaves as follows:

| Situation | What happens |
| --- | --- |
| Format is Default | The camera opens at its largest picture up to 1920 × 1080 at 30 fps or more. |
| A camera is plugged in or out while the Properties window is open | The lists update within about 3 seconds. |
| A saved camera is not plugged in | The camera shows as "(not connected)". |
| The camera is missing, in use by another program, unplugged, or sends nothing for 5 seconds | The Properties window reports it, and the source opens the camera again every 2 seconds. |
| A second camera source shows the same camera | One camera shows in one camera source at a time. The second source reports that the camera is in use by another camera source. |
| A camera is listed for the first time on macOS | The system asks for camera access. |

[docs/architecture.md](architecture.md#camera-capture) describes how the capture thread opens,
reads and reopens a camera.

## Filters

Each source can carry a list of filters. The video filters are color correction, chroma key,
color key, crop and pad, image mask, scroll and sharpen. The audio filters are gain, noise gate,
compressor, limiter and a three-band equalizer.

| Filter | What it does |
| --- | --- |
| Color correction | Sets gamma, contrast, brightness, saturation and opacity, and the white balance by temperature and tint. |
| Chroma key | Makes the pixels near a key color transparent by their chroma distance from it, with key color spill reduction. |
| Color key | Makes the pixels near a key color transparent by their RGB distance from it, with Similarity and Smoothness, and adjusts the rest by gamma, contrast, brightness and opacity. |
| Crop and pad | Takes pixels off each edge of a source (a positive value), adds transparent pixels (a negative value), or shows one area of it. The source takes the new size in every scene. |
| Image mask | Uses a picture file as an alpha mask (from its brightness or its alpha) or blends it with the source (multiply, add or subtract). The picture is stretched over the source or centered at its own size, and the filter then applies a color and an opacity. |
| Scroll | Moves a source's picture at a horizontal and a vertical speed in pixels per second. The picture repeats (Loop) or moves out once, in an area whose width and height can be limited. |
| Sharpen | Makes edges crisper by a Sharpness from 0 to 1. |
| Gain, noise gate, compressor, limiter and equalizer | Change the level and tone of a source's audio. |

[MECHANICS.md](../MECHANICS.md#video) lists each filter's default values and limits.

## Transitions

The transitions are cut, fade, fade through a color, and swipe, with a duration from 0 to 20
seconds. Fade, fade through a color and swipe have an Easing setting: Linear, Ease in, Ease out,
or Ease in and out.

### Studio mode

In studio mode you edit a preview scene beside the program scene. You then send the preview to
the program with the chosen transition, or with a quick cut or fade.

## Audio

The Audio Mixer dock shows peak and RMS meters with peak hold, a volume fader in dB, and a mute
button for every audible source. The mixer also measures the final mix's loudness (momentary,
short-term, integrated, loudness range and true peak). The window does not show these readings
yet.

A source's Properties window sets two more audio options:

- **Sync offset** delays the source's audio by 0 to 5000 ms to line it up with its picture.
- **Monitoring** is off, monitor only, or monitor and output. With monitor only, you hear the
  source, and recordings and streams leave it out.

Monitored sources play on the device chosen in Settings › Audio. Monitoring works on Windows and
macOS.

## Recording

Recordings are MKV, MP4 or MOV files at four quality levels: Compact, High, Pristine and Lossless.
The Lossless level encodes the video frames without compression loss and writes very large files.
The software encoder runs x264 with `-qp 0`, and NVENC through `ffmpeg` runs with `-tune lossless`. StudioCraft writes recordings to your Videos folder
unless Settings › Output names another folder, and names each file `YYYY-MM-DD HH-MM-SS.<ext>`.
File › Show Recordings opens the folder.

### Encoders

x264 is the default encoder for recordings and streams. Settings › Output and Settings › Stream
show the chosen encoder's settings, such as the x264 and NVENC presets.

| Encoder | Records | Needs `ffmpeg` | Systems |
| --- | --- | --- | --- |
| Software (x264) | MKV, MP4 and MOV at every level, and streams | Yes | All |
| H.264 (built in) | MKV at Compact, High and Pristine, with a Speed, Balanced or Quality preset | No | All |
| Hardware (set in Settings › Video) | MKV, MP4 and MOV, and streams, with the hardware encoder set in Settings › Video › Hardware acceleration | Yes, except for the built-in encoders below | All |
| NVIDIA NVENC H.264 and HEVC (built in) | MKV, through the NVIDIA driver | No | Windows and 64-bit Linux |
| Windows Media Foundation H.264 and HEVC (built in) | MKV, with the hardware encoders Media Foundation lists | No | Windows |
| Apple VideoToolbox H.264 and HEVC (built in) | MKV, with the VideoToolbox hardware encoders | No | macOS |

H.264 (built in) uses StudioCraft's own H.264 and AAC encoders. The Hardware encoder uses the
built-in hardware encoders for MKV recordings. [Hardware acceleration](#hardware-acceleration)
describes how it picks one.

### When `ffmpeg` is needed

MP4 and MOV recordings, recordings at the Lossless quality level, streams, and recordings with x264 or a hardware
encoder outside the built-in ones run in a separately installed `ffmpeg`. StudioCraft looks for
`ffmpeg` on the `PATH`, or at the location set in Settings › Output. Recording uses x264 until
H.264 (built in) is chosen in Settings › Output. No release package includes `ffmpeg`, and the
Flatpak cannot run an `ffmpeg` installed on the system.

Tools › Available Encoders lists the built-in encoders and the encoders the installed `ffmpeg`
offers, and says when `ffmpeg` was not found.

### Replay buffer

File › Replay Buffer keeps the last seconds of the program in memory with the built-in H.264 and
AAC encoders. File › Save Replay writes them to an MKV file named `Replay <date> <time>.mkv` in
the recording folder, and the buffer keeps running. Settings › Output sets the length (5 to 3600
seconds, 20 by default) and the memory limit (32 to 16384 MB, 512 by default). Start or Stop
Replay Buffer and Save Replay can be given shortcuts in Settings › Hotkeys. While the buffer runs,
the status bar shows REPLAY with the time it has run.

## Streaming

StudioCraft streams to a Real-Time Messaging Protocol (RTMP) server at a constant bitrate. Set the
server and the stream key in Settings › Stream, then click Go Live. Streams run in `ffmpeg` with
x264 or a hardware encoder ([When `ffmpeg` is needed](#when-ffmpeg-is-needed)). Settings › Stream
also sets the video bitrate, the audio bitrate, the video encoder and the keyframe interval. The
defaults are listed in [MECHANICS.md](../MECHANICS.md#recording-and-streaming).

## Hardware acceleration

Settings › Video › Hardware acceleration holds two settings.

**Graphics adapter** picks the adapter the window draws with: Automatic, Power saving, Software,
or a listed adapter. The choice takes effect from the next start. The window shows a notice when
the chosen adapter is missing or the app fell back to software. After a device loss the app starts
again on a new device, and after 3 losses in 10 minutes it draws in software.

**Hardware encoder** picks the encoder that recordings and streams use when their encoder is
Hardware. Automatic tests each candidate with a one-frame encode, in an order set by the graphics
adapter's vendor, and uses x264 when none works. The candidates run in `ffmpeg`: NVIDIA NVENC, AMD
AMF, Intel Quick Sync, Windows Media Foundation, VideoToolbox and VA-API. NVENC, AMF and Quick
Sync offer HEVC and AV1 as well as H.264, and VideoToolbox and VA-API are listed.

For an MKV recording, the Hardware encoder tests the built-in hardware encoders in StudioCraft's
own process, as it tests NVENC, and records with the built-in encoder:

- NVIDIA NVENC H.264 and HEVC (built in) run through the NVIDIA driver on Windows and 64-bit
  Linux.
- Windows Media Foundation H.264 and HEVC (built in) run the hardware encoders Media Foundation
  lists, on Windows.
- Apple VideoToolbox H.264 and HEVC (built in) run the VideoToolbox hardware encoders, on macOS.

The compositor converts the program to the encoders' frame format on the GPU.

## Interface

### Theme

StudioCraft has a compact dark and light theme and follows the system's light or dark setting.
Controls and menu rows are 24 points tall, body text is 12 points, and menus are sized to their
content and at least 160 points wide. A focus ring marks the focused widget after Tab, an arrow
key or F6. Every text pair in both palettes has a contrast of at least 4.5:1, and every state mark
and the focus ring have at least 3:1.

### Menu row and output controls

The menu row is 32 points tall. It holds the menu titles on the left, and on the right an Edit and
Studio switch, the transition picker with its duration, and Record and Go Live with their timers.
When the row is too narrow, the transition picker moves behind an icon button first, then Record
and Go Live leave out their labels, and last the menu titles move into the menu bar's overflow
menu. Docks › Output Controls and Docks › Transition Controls show and hide the two groups.

### Docks

Scenes, Sources and Audio Mixer sit in a strip under the preview. Each dock has a 24-point header
with the title on the left and the dock's actions on the right, and no footer. When the dock is
narrow, the header keeps its actions and shortens the title. 1-point dividers separate the docks.

The strip is 200 points tall, or 30% of the window's height when that is less, and never under 120
points. Scenes, Sources and Audio Mixer start 300, 340 and 400 points wide
([MECHANICS.md](../MECHANICS.md#editing)), and the last shown dock takes the rest. No dock is narrower than 200 points while the window has room.

You resize the strip by dragging its top edge outside the header actions, and resize the docks by
dragging a divider. The edge and the dividers have 8-point hit bands, and a dragged strip can be up
to 45% of the window's height. A right-click on a dock's title offers Move Left and Move Right,
which pass over hidden docks, and Undock.

An undocked dock is drawn in a window of its own inside the main window. You drag it by the dock's
title and resize it from its right and bottom edges. Its starting and smallest sizes are in
[MECHANICS.md](../MECHANICS.md#editing). A smaller main window draws it further in without
changing its saved place or size. Its menu offers Dock in the Strip.

StudioCraft keeps the strip's height, the docks' widths, visibility and order, which docks are
undocked, and their windows' places and sizes until Docks › Reset Docks. Reset Docks puts every
dock back in the strip in its first order.

### Dock layouts

Docks › Save Dock Layout… asks for a name and keeps every dock setting under it. Docks › Layouts
puts a layout back, and Docks › Remove Layout deletes one. Layout names follow the rules for
profile names and are compared without regard to case, so saving under a name that is taken
replaces that layout. At most 32 layouts are kept, and a settings file with more keeps the first
32.

### Fonts

The interface uses Inter for text, Inter SemiBold for titles and JetBrains Mono for readouts. When
`CRAFT_FONTS_DIR` is set at build time, the craft-fonts faces follow for the scripts Inter lacks,
and egui's built-in faces come last. When the interface language is written in a script Inter
lacks (Arabic, Thai), that script's face goes first. The menus' check and radio marks draw with or
without `CRAFT_FONTS_DIR`.

### Title bar

StudioCraft draws its own title bar only where the desktop leaves the window frame to the app
(GNOME on Wayland, or `STUDIOCRAFT_TITLEBAR=app`) and Settings › General › "Use system title bar"
is off. The menu strip is then the title bar. You move the window by its empty part, a
double-click maximizes it, and the window buttons sit where the desktop puts them.

### Languages

Settings › General › Language picks the interface language, or follows the system language. These
languages are built in: English, Simplified Chinese, Traditional Chinese, Arabic, Czech, Dutch,
French, German, Hebrew, Italian, Japanese, Korean, Polish, Brazilian Portuguese, Russian, Spanish,
Swedish, Turkish and Ukrainian.

In Arabic and Hebrew the window runs right to left. The menus, output controls, dock headers and
status bar start at the right, and egui's submenus, scroll bars and collapsing headers mirror.

### Profiles and scene collections

A profile is a set of stream and output settings, and a scene collection is a set of scenes. You
can keep several of each and switch between them from the menu bar. Scene Collection › Import…
and Export… read and write a scene collection as a file.

### Stats and scopes

View › Stats shows the frame count, late frames, frame times, encoded and skipped frames, the
encoder queue, and the size of the running recording or stream.

View › Scopes shows a waveform, an RGB parade, a histogram, or the YUV or HLS vectorscope of the
program output. The `scopes.kind` command picks the scope.

### Help and updates

Help › Community opens the Discord server, the website, the source code and the issue tracker.
Help › Check for Updates… looks for a newer release, and Settings › General › Check for updates at
start (on by default) does the same when the app starts. The app finishes an update it installed
itself. Help › About StudioCraft lists the contributors, with a choice of username, display name
or real name, and shows the license texts.

The app icon is the window icon, the picture in the update prompt, and the icon of the menu entry
the updater writes beside an AppImage.

## Hotkeys

Settings › Hotkeys lists every command that can take a shortcut and changes its shortcuts. Type
shortcuts such as Ctrl+1 or Shift+F5, and separate several with commas. Ctrl+1 to Ctrl+9 show the
first nine scenes. The list includes Start or Stop Recording, Start or Stop Streaming, Start or
Stop Replay Buffer, Save Replay, Transition to Program, Cut to Program and Studio Mode.

## Settings

File › Settings opens the Settings window. OK or Apply puts the changes in force, and Cancel
discards them.

| Tab | Settings |
| --- | --- |
| General | Language, Check for updates at start, and Use system title bar (from the next start). |
| Stream | Server, Stream key, Video bitrate, Audio bitrate, Video encoder and Keyframe interval. |
| Output | Recording folder, Format, Quality, Video encoder and its settings, the `ffmpeg` program, Replay buffer length and Replay buffer memory limit. |
| Audio | Monitoring device, the device monitored sources play on. |
| Video | Canvas size, Output size and Frames per second, and the Hardware acceleration section with Graphics adapter and Hardware encoder. Video settings can't change while recording, streaming or running the replay buffer. |
| Hotkeys | A shortcut for each command. |

## Control and automation

StudioCraft runs every action as a command with a stable id. Three tools run those commands from
outside the window:

- `studiocraft-cli send <port> <method> [<params>]` sends one control request to the running app
  and prints the reply. `studiocraft-cli check <file>` reports problems in scene collection files.
- `studiocraft --control <port>` accepts JSON requests on `127.0.0.1:<port>`. Each request carries
  the token from `STUDIOCRAFT_CONTROL_TOKEN`. [docs/control-protocol.md](control-protocol.md)
  describes the protocol, and its [Connecting](control-protocol.md#connecting) section shows how
  to make the token.
- The Model Context Protocol (MCP) server lets an MCP client drive StudioCraft.
  `studiocraft-cli mcp` runs its own studio without a window, and
  `studiocraft-cli mcp --connect <port>` drives the app started with `--control <port>`, including
  its window, menus and screenshots. Connected to the app, the server can inspect the window, take
  screenshots, click, drag, press keys, type, use menus, answer dialogs and quit.
  [docs/mcp.md](mcp.md) describes how to register it, and its tools, resources and prompts.

In a POSIX shell, these commands start the app with a control port, add a scene and connect the
MCP server:

```sh
export STUDIOCRAFT_CONTROL_TOKEN=$(openssl rand -hex 32)
studiocraft --control 9876 &
studiocraft-cli send 9876 engine.execute '{"command": "scene.add", "params": {"name": "Intro"}}'
studiocraft-cli mcp --connect 9876
```

### Command-line options

```text
studiocraft [--config-dir <dir> | --no-prefs] [--control <port>]
studiocraft --screenshot <file.png>
studiocraft --self-test <file.json>
studiocraft --version
```

| Option | What it does |
| --- | --- |
| `--config-dir <dir>` | Keeps settings, scene collections and profiles in `<dir>`. |
| `--no-prefs` | Keeps everything in memory, and reads and writes no settings. |
| `--control <port>` | Accepts control requests on `127.0.0.1:<port>`. |
| `--screenshot <file>` | Draws about 120 frames, saves the window as a PNG and exits. |
| `--self-test <file>` | Runs a short scripted session and writes its steps as JSON. |

The command-line tool `studiocraft-cli` sends control requests, runs the MCP server, checks scene
collection files and times the compositor without a window. To list its commands from a source
checkout, run:

```sh
cargo run --release -p studiocraft-cli -- help
```
