# Architecture

StudioCraft is a Cargo workspace. The desktop app is the root package (`src/`). The libraries are
in `crates/`, the command-line tool is in `apps/studiocraft-cli`, and the workspace tooling is in
`xtask/`.

## Layers

A crate may depend only on workspace crates in a lower layer. A dev-dependency may also reach a
crate in the same layer. `cargo xtask layers` checks this against the table in
`xtask/src/layers.rs`.

| Layer | Crate | What it holds |
| --- | --- | --- |
| L0 | `studiocraft-model` | Scenes, sources, scene items, transforms, profiles, settings schemas, and the versioned file format with its migrations. |
| L0 | `studiocraft-media` | The master clock, frame pacing, video frame and audio block types, buffer pools, bounded lock-free queues, mixing and level meters. |
| L0 | `craft-tsmux` | The MPEG-2 transport stream writer: PAT and PMT, PES packets, PCR and continuity counters, written to any `io::Write`. It depends on no StudioCraft crate. |
| L0 | `craft-color-transform` | RGB spaces and Bradford adaptation, white balance by temperature and tint, OkLab and OkLCh, monotone tone curves and lookup tables, and PQ tone mapping. It depends on no StudioCraft crate. |
| L0 | `craft-scopes` | Video scope maths: the waveform, parade, histogram and vectorscopes of a borrowed RGBA8 or NV12 frame, their numeric summaries and trace images. It depends on no StudioCraft crate. |
| L0 | `craft-audio-dsp` | Audio DSP shared with the other Craft apps: the `AudioEffect` trait and its effects, the BS.1770 / EBU R128 loudness meter, panning, test signals and resampling. It depends on no StudioCraft crate. |
| L0 | `craft-ease` | Easing curves, keyed tracks, constant-speed Bezier motion paths and damped springs. It depends on no StudioCraft crate. |
| L1 | `studiocraft-plugin` | The `Source`, `Filter`, `Transition`, `Encoder` and `Output` traits and the `Registry` they are registered in. |
| L2 | `studiocraft-engine` | Commands with stable ids, undo and redo, source threads, the audio mixer thread and the frame scheduler. |
| L2 | `studiocraft-render` | The wgpu compositor: scenes, filters, transitions, and frame conversion for encoders. |
| L2 | `studiocraft-platform` | Display, window, camera and audio capture, locating ffmpeg, and opening folders and links. |
| L2 | `studiocraft-encode` | The built-in encoders: H.264 video from NV12 frames and AAC audio from the mixer's blocks. |
| L3 | `studiocraft-sources` | The built-in sources and filters. |
| L3 | `studiocraft-outputs` | Recording and streaming through an ffmpeg process, Matroska recording through the built-in encoders, and the video encoders. |
| L4 | `studiocraft-studio` | The running studio: sources, mixer, compositor, transitions and outputs behind one command interface. |
| L5 | `studiocraft-ui-egui` | The window: menus, preview, docks, properties, filters and settings. |
| L5 | `studiocraft-mcp` | The MCP server: its tools, resources, resource templates, prompts and completions, a studio in the same process without a window (with its file roots and streaming gate), and the control channel client that sends calls to the app. |
| app | `studiocraft` | The desktop binary, its control channel and the screenshot and self-test harness. |
| app | `studiocraft-cli` | `send` for one control request, `mcp` for the MCP server on standard input and output (headless, or `--connect <port>` to the app), `check` for scene collection files and `bench` for the timing run. |
| tool | `xtask` | `cargo xtask` checks. |

These crates have no operating-system or GPU dependency, and `cargo xtask wasm` checks that they
build for `wasm32-unknown-unknown`:

- `studiocraft-model`
- `studiocraft-plugin`
- `craft-tsmux`
- `craft-color-transform`
- `craft-scopes`
- `craft-audio-dsp`
- `craft-ease`

## Threads and data flow

- **Source threads.** Each source runs on its own thread (`engine::host`). It publishes video
  frames into a `VideoOut` slot and audio blocks into a bounded lock-free queue.
- **Audio capture threads.** An audio input or output source receives samples on the device's
  thread: cpal's on Windows and macOS, and on Linux a thread that `platform::audio` starts for each
  capture to run a PipeWire stream loop. The Linux stream asks for 48 kHz stereo `f32`, converts
  each buffer into a buffer allocated before the stream starts, and stops when its handle is
  dropped. Listing Linux devices reads the PipeWire registry and the `default` metadata object and
  waits at most 2 s for the server. A failed or ended stream and a lost server connection are
  reported to the source, which reopens the device; the rest of the app keeps running.
- **Screen capture threads.** On Linux a display or window capture (`platform::portal`) starts a
  thread that asks the desktop's screen-cast portal for one monitor or window, so the desktop shows
  its own picker while the source thread keeps running. The portal hands over a PipeWire stream,
  which a second thread reads. It copies each picture, row by row without the row padding, into a
  spare frame and publishes it through `platform::screen_frames::FrameSlot`: two lock-free queues
  hold the newest frame and up to 2 spares, so a picture the source has not taken yet is replaced
  by the newer one and neither thread waits for the other. The stream takes 4-byte RGB formats up
  to 8192 px a side and follows a resize with the next buffer. A canceled picker, or a session the
  desktop closes when sharing is stopped from its own controls, clears the picture, and the source
  does not open the picker again until its settings change. A desktop without the portal, a portal
  that returns no stream, a failed stream and a lost PipeWire connection are reported and tried
  again after 2 s. A capture dropped while the picker is open closes the portal session, which ends
  the request. Once the stream runs, the portal thread waits for the capture to be dropped, the
  session to close or the stream to end, then closes a pipe the stream loop watches and closes the
  portal session.
- **Camera capture threads.** A camera source (`sources::camera`) opens a
  `platform::camera::CameraCapture`, which starts a thread of its own. That thread lists the
  cameras, picks the saved one (the same name and system id, then the same id unless it is a
  Linux device node, then the same name),
  reads its formats and opens it through the `nokhwa` crate: Media Foundation on Windows,
  AVFoundation on macOS and V4L2 on Linux. On Linux only the V4L2 nodes that can capture video
  are listed. On Linux the 8-bit gray (`GREY`) formats are listed and streamed through the `v4l`
  crate (`platform::camera_gray`), since `nokhwa` asks for gray under another code; a camera
  that sends only gray lists only those. A gray stream waits at most 500 ms for each frame, so
  its capture thread sees a stop while the camera sends nothing. Once such a wait runs out, and
  for the first frame, each read waits at most 5 s and a read that runs out fails the capture
  (the source opens the camera again). The camera handle stays on that
  thread. Each frame is converted into a spare frame
  of the same `FrameSlot` the screen captures use: YUYV and NV12 to RGBA with the BT.601
  studio-range formulas, gray and RGB to RGBA, BGR to BGRA, and MJPEG decoded with `zune-jpeg` at
  the size in its own header (an MJPEG frame without Huffman tables uses the standard ones). An
  uncompressed frame longer than its tight size is read with the row stride V4L2 reports on
  Linux, and elsewhere with the stride its length gives; on Windows RGB24 frames arrive last row
  first; on macOS each frame's own size and encoding are used, the library's frame queue is
  emptied at every frame, and only YUYV formats are offered; the thread copies such frames tight and top
  row first into a reused buffer. On Windows every thread that uses Media Foundation sets up COM
  (`comedy`): each capture thread for its stream's life, and a short-lived thread for each camera
  list or format read. A frame that cannot be converted goes back to the spares; 30
  in a row fail the capture. A panic inside the camera library is caught and reported as an
  error. A missing or busy camera, a format the camera does not offer, a failed stream, or no new
  frame for 5 s (the first frame included) clears the picture and is reported to the source,
  which opens the camera again after 2 s while the rest of the app keeps running. Dropping the
  capture sets a stop flag; the thread sees it after its current frame, stops the stream and
  ends. A camera is reserved for one capture thread at a time: a second camera source showing the
  same camera reports "the camera is in use by another camera source". A thread can wait on a
  camera that sends nothing; until it ends, a new capture of that camera fails with "the camera is
  still closing" and the source tries again. The source's settings schema depends on its settings
  (`Plugin::schema_for`): the Format list holds the formats of the chosen camera. On macOS the
  formats are read from the device (`nokhwa-bindings-macos`) without changing its active format.
  A camera that a capture of the app has open is not opened again to read its formats; it lists
  the formats read when the capture opened it. A camera that another program holds open lists the
  formats it offered when it was last read, for up to 32 cameras. The engine checks a choice
  again when a choice before it changed, so a format kept from another camera is refused. The
  Properties window (`ui-egui::forms`) reads every schema on its own thread and shows "Reading
  the lists…" until it arrives. While a choice's new lists are read, the fields after it are
  locked, and the change is sent once they arrive, with a later choice they do not offer set back
  to its default. The window reads one schema at a time: a read asked for while one runs starts
  when it ends, and the earlier reply is dropped. Closing the window while a change waits sends
  the change with the choices after it at their defaults. When the source's settings change
  elsewhere while a change waits, the change made elsewhere is kept. Every 3 seconds the window
  asks the source on another thread for `Plugin::lists_stamp`, a value that changes when its
  devices change (the camera source hashes its list of cameras, which does not open them), and
  reads the lists again when the value changed. Each read of the lists also takes the value,
  just before it reads them. A source that returns `None` is not asked again.
  A camera source whose camera is unplugged keeps opening it every 2 seconds and resumes when it
  returns.
- **Mixer thread.** The mixer (`engine::mixer`) pulls one 1024-frame block (48 kHz stereo `f32`)
  from every audible source, delays it by the source's sync offset, applies the audio filters,
  gain, mute and scene-change fades, and measures peak and RMS levels. Each input's delay line
  holds 5 s of audio (the longest offset) and is allocated with the input list on the sending
  thread; a new input list takes over the delay lines of the sources it keeps, so their delayed
  audio still plays. It pushes the mix to every output's bounded queue. The clipped mix
  also feeds a `craft-audio-dsp` loudness meter (momentary, short-term, integrated, loudness range
  and true peak), whose readings the mixer publishes after every block without locks. Sources set
  to monitor only are measured but left out of that mix. Every monitored source, after its delay,
  filters, gain, mute and fade, is added to a second, preallocated monitoring mix, which the mixer
  pushes to the monitoring queue.
- **Monitoring thread.** While a source is monitored, `studio::monitor` opens the profile's
  monitoring device through `platform::audio::AudioSink` (cpal on Windows and macOS). The device's
  callback pulls the monitoring mix from a queue of 4 blocks, waits for 2 blocks before it plays,
  converts 48 kHz to the device's rate by linear interpolation and maps the two channels onto the
  device's, without allocating or locking. A device that fails to open or fails while it plays is
  reported in a notice and closed; the mixer and the outputs keep running, and the same device is
  not opened again until the setting or the monitored sources change.
- **Compositor thread.** The scheduler (`engine::scheduler`) wakes on the master clock's frame ticks.
  The compositor (`render`) uploads changed source frames, draws the program scene (and the preview
  scene in studio mode) into a ring of 3 GPU textures, and converts the program to NV12 for the
  encoders. The window registers the ring's texture views with egui, so frames stay on the GPU.
  While View › Scopes is open, the compositor also converts the program to a second NV12 copy of
  at most 480 × 270 and sends it to the window's scopes queue. The copy is drawn with linear
  filtering and its chroma is at half width and half height, so a hard edge in the program shows on
  the scopes as intermediate levels.
- **Scopes thread.** While View › Scopes is open, the window hands the newest program copy to the
  scopes thread (`ui-egui/src/scopes.rs`) when the thread is idle. The thread computes and paints
  the chosen scope with `craft-scopes` into buffers it keeps and makes the texture image. The window
  moves the image into its texture. The window and the thread share bounded lock-free queues and
  atomic flags, and the thread parks while it has nothing to do.
- **Output threads.** A stream, or a recording with an ffmpeg encoder, runs an ffmpeg process with
  one writer thread for video and one for audio (`outputs/src/writer.rs`). A recording with H.264
  (built in) runs a video encode thread, an audio encode thread and a file thread that writes the
  Matroska file (`outputs/src/native.rs`). The encode threads hand packets to the file thread
  through a queue that holds at most 64 MiB and 3 s of media. In both, the video and audio threads
  share their timeline through one 128-bit atomic word. The replay buffer (`outputs/src/replay.rs`)
  runs the same three threads, and its third thread keeps the packets in a ring bounded by length
  and bytes instead of writing a file. A save hands the ring thread a one-slot queue through a
  bounded lock-free queue, and the ring thread puts a copy of its packet list, which shares the
  packet bytes, in that slot. A `save replay` thread per save writes the clip to its MKV file, and
  `Studio::tick` reports each finished save.
- **Window thread.** The window reads `Studio` state, runs commands, and shows statistics.

One `MasterClock` (`media::clock`) drives the frame scheduler, the mixer and every timestamp.
Frames and audio blocks come from pools, and the steady-state frame loop does not allocate.

## Extension points

A new source, filter, transition, encoder or output is a type that implements the matching trait
in `studiocraft-plugin`. The type is registered in a `Registry` (see
`studiocraft_sources::register` and `studiocraft_outputs::register`). The window builds its
property forms from each plugin's `SettingsSchema`, so a new plugin does not need window code.

A video filter is one GPU pass (`VideoEffect`). Its `Reframe` sets which part of the input the
pass draws: the whole input, the input with pixels taken off or added at each edge, or one area of
it, up to 8192 pixels a side. The compositor draws each pass into the top-left corner of a pair of
filter textures as large as the largest pass, and the scene item takes the size of the last pass.
`Studio::source_size` applies the same passes, so the window's selection box matches the picture.
A pass reads its input through `input_at`, which returns transparent pixels outside the input, or
`input_pixel`, which reads at a pixel position clamped to the input's edges. A pass can also carry
an `EffectImage`, a picture the compositor uploads once and binds in group 2 for `image_at`; the
compositor releases it when no pass in the plan uses it. A pass with an `Animation` has its
parameters stepped before each frame with the seconds since the first frame that drew it and the
size of its input; the compositor forgets the start time when no pass in the plan uses the
animation.

An encoder names the codec it writes (`Encoder::codec`: H.264, HEVC or AAC). An encoder that runs
inside the ffmpeg program returns its `FfmpegEncoder` form from `Encoder::ffmpeg`, which gives
ffmpeg the codec and its arguments. An encoder without that form is built in: `Encoder::open`
starts its `VideoSession`, which takes NV12 frames and returns packets. A built-in encoder runs only
in an output that runs built-in encoders. The recording output runs built-in and ffmpeg encoders,
and with a built-in encoder it writes MKV files only. The replay buffer output (plug-in id
`replay`) runs the built-in encoders only, and its `OutputInstance::replay` returns a
`ReplayClip` that writes the buffer's packets to a file. The stream output runs ffmpeg encoders only.
`studiocraft-encode` holds the built-in H.264 encoder (`studiocraft_encode::H264`, plug-in id
`h264`) and the AAC encoder (`studiocraft_encode::AacSession`) that built-in recordings use for
sound.
`Encoder::availability` reports whether an output can run an encoder, and the output start check,
the Settings encoder lists and Tools › Available Encoders use it.

Each saved source and filter records its plugin's `kind_version`. When a plugin's version is
higher, `Plugin::migrate` updates the saved settings when the collection loads.

## Commands and undo

Every change to a scene collection is a `Command` (`engine::command`) with a stable id such as
`scene.add` or `item.transform`. Menus, shortcuts, the control channel and the MCP server all run
commands by id. Undoable commands record the collection before the change, and the history keeps
200 steps. Commands with the same coalescing key (for example `item.transform:<id>` during a drag)
within 750 ms of each other make one undo step.

The window adds its own commands, such as `view.stats` or `profile.new`, in
`ui-egui/src/commands.rs`. `craft-keymap` binds shortcuts to all of them.

## Look

`ui-egui/src/kit/tokens.rs` holds the dark and light palettes, the compact and comfortable size
sets and the type scale. `ui-egui/src/theme.rs` applies them to egui's dark and light styles at
start, gives the menu bar its style (menus at least 160 points wide), and paints the focus ring
around the focused widget after Tab, an arrow key or F6. The window follows the system's light or
dark setting. A unit test checks every text pair of both palettes for 4.5:1 contrast and every
state mark and the focus ring for 3:1. The modules under `ui-egui/src/kit/` depend only on egui,
`craft-ui` and `craft-menu`: `segmented.rs` is the segmented control, `onair.rs` the Record and
Go Live buttons with their timers, and `panel_header.rs` the dock header.

`ui-egui/src/output_bar.rs` draws the 32-point menu row: the menu titles on the left, and on the
right the Edit and Studio switch, the transition picker with its duration, Record and Go Live. When
the row is too narrow, the transition picker moves behind an icon button first (`craft-ui`'s
`fit_toolbar` makes that choice), then Record and Go Live leave out their labels, and last the menu
titles move into the menu bar's overflow menu. Docks › Output Controls and Docks › Transition
Controls show and hide the two groups.

`ui-egui/src/docks.rs` draws Scenes, Sources and Audio Mixer in the strip under the preview. Each
dock has a 24-point header from `kit/panel_header.rs`, with the title on the left and the dock's
actions on the right, and no footer. The header keeps its actions when the dock is narrow and
shortens the title instead. `ui-egui/src/strip.rs` sizes the strip: 200 points tall, or 30% of the
window's height when that is less, and never under 120 points. Scenes, Sources and Audio Mixer are
300, 340 and 400 points wide by default, and the last shown dock takes the rest; no column is
narrower than 200 points while the window has room. A right-click on a dock's title offers Move
Left and Move Right (`dock.move`), which pass over hidden docks. The strip's top edge and the 1-point dividers between docks are
`craft-ui` `ResizeHandle`s with 8-point hit bands. The header actions take a press or a drag
that starts on them, so the top edge resizes only from outside the actions. A dragged height (up to 45% of the window) and
dragged widths are saved in `config.toml` with the docks' visibility and order, and Docks › Reset
Docks clears them and puts the docks back in their first order.

`ui-egui/src/fonts.rs` builds the font stack with `craft-ui`'s `UiFonts`: Inter Regular for the
proportional family, Inter SemiBold for the `semibold` family (dock, section and dialog titles) and
JetBrains Mono for the monospace family, all embedded from `assets/fonts`. The craft-fonts faces
from `CRAFT_FONTS_DIR` follow for the scripts those faces lack. When the interface language is
written in a script they lack (Arabic, Thai), that script's face goes first in the proportional
families. egui's built-in faces come last. `StudioApp::install_fonts` installs the stack at start,
and the window installs it again on the first frame after the interface language changes.

## Saved files

Scene collections and profiles are JSON files with a `format` name and a `version`
(`model::format`). Reading a file at an older version runs the migrations in order. A file from a
newer version is refused with an error. The window keeps scene collections in `scenes/` and
profiles in `profiles/` under the settings folder, and writes them with `craft-atomic`.

## Shared crates

| Need | Crate |
| --- | --- |
| Menu bar model, layout and drawing | `craft-menu` |
| Shortcuts, rebinding and dispatch | `craft-keymap` |
| Settings file with sections, versions and debounced saving | `craft-settings` |
| Open, save and folder dialogs | `craft-dialogs` |
| Atomic file writes | `craft-atomic` |
| Close-request handling | `craft-quit` |
| Icon buttons and combo boxes, the interface font stack | `craft-ui` |
| Embedded interface fonts from `CRAFT_FONTS_DIR` | `craft-fonts-build` |
| Interface translations from `crates/ui-egui/i18n/locales` and `CRAFT_I18N_DIR`, the Language setting | `craft-i18n` |
| Font lookup for text sources | `craft-font-catalog` |
| Picture decoding for image sources | `craft-image-codecs` |
| Frame-rate and tick arithmetic | `craft-time` |
| Control channel framing and session tokens | `craft-control-protocol` |
| JSON-RPC framing for the MCP server | `craft-jsonrpc` |
| MCP sessions and tool results | `craft-mcp` |
| Update checks, the update prompt and finishing an installed update | `craft-update` |
| Who draws the window frame, and the menu strip as the title bar | `craft-window` |

The window draws its own title bar only where `craft-window` leaves the frame to the app (GNOME on
Wayland, or `STUDIOCRAFT_TITLEBAR=app`) and Settings › General › "Use system title bar" is off. The
menu strip is then the title bar: its empty part moves the window, a double-click maximizes it, and
the window buttons sit where the desktop puts them. `src/titlebar.rs` makes the decision and
`crates/ui-egui/src/title_bar.rs` draws the strip. Release file names and package ids are in
`docs/releasing.md`.

## Needs without a shared crate yet

- **Native macOS menu bar.** The menu is drawn in the window on every system.
- **Capture on Linux.** Display, window and audio device capture on Linux use the `ashpd` and
  `pipewire` crates directly in `studiocraft-platform`. Windows and macOS use `xcap` and `cpal`.
- **Cameras.** Camera capture on every system uses the `nokhwa` crate directly in
  `studiocraft-platform`, with `v4l` on Linux and `zune-jpeg` for MJPEG frames.
- **Encoding.** Streams, MP4 and MOV recordings, Lossless recordings, and recordings with x264 or a
  hardware encoder need an ffmpeg program installed separately. StudioCraft runs it as a process
  and does not link it. MKV recordings with H.264 (built in) use the `craft-video` H.264 encoder
  and the `craft-audio` AAC encoder and do not need ffmpeg.

## Dependency licenses

`cargo xtask licenses` accepts permissive software licenses (MIT, Apache-2.0, BSD, Zlib, ISC and
similar), the OFL-1.1 and Ubuntu font licenses of egui's built-in fonts, and the IJG license.
Two packages are accepted by name: `option-ext` (MPL-2.0), which `dirs-sys` uses through
`craft-settings`, and `libfuzzer-sys` ((MIT OR Apache-2.0) AND NCSA), which `Cargo.lock` lists
for `rav1e`. `rav1e` is an optional dependency of `craft-video` that no StudioCraft crate enables,
so neither package is built. The list is `EXCEPTIONS` in `xtask/src/licenses.rs`.
