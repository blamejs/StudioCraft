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
| L1 | `studiocraft-plugin` | The `Source`, `Filter`, `Transition`, `Encoder` and `Output` traits and the `Registry` they are registered in. |
| L2 | `studiocraft-engine` | Commands with stable ids, undo and redo, source threads, the audio mixer thread and the frame scheduler. |
| L2 | `studiocraft-render` | The wgpu compositor: scenes, filters, transitions, and frame conversion for encoders. |
| L2 | `studiocraft-platform` | Display, window and audio capture, locating ffmpeg, and opening folders and links. |
| L3 | `studiocraft-sources` | The built-in sources and filters. |
| L3 | `studiocraft-outputs` | Recording and streaming through an ffmpeg process, and the video encoders. |
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

## Threads and data flow

- **Source threads.** Each source runs on its own thread (`engine::host`). It publishes video
  frames into a `VideoOut` slot and audio blocks into a bounded lock-free queue.
- **Mixer thread.** The mixer (`engine::mixer`) pulls one 1024-frame block (48 kHz stereo `f32`)
  from every audible source, applies the audio filters, gain, mute and scene-change fades, and
  measures peak and RMS levels. It pushes the mix to every output's bounded queue.
- **Compositor thread.** The scheduler (`engine::scheduler`) wakes on the master clock's frame ticks.
  The compositor (`render`) uploads changed source frames, draws the program scene (and the preview
  scene in studio mode) into a ring of 3 GPU textures, and converts the program to NV12 for the
  encoders. The window registers the ring's texture views with egui, so frames stay on the GPU.
- **Output threads.** Each recording or stream runs an ffmpeg process with one writer thread for
  video and one for audio.
- **Window thread.** The window reads `Studio` state, runs commands, and shows statistics.

One `MasterClock` (`media::clock`) drives the frame scheduler, the mixer and every timestamp.
Frames and audio blocks come from pools, and the steady-state frame loop does not allocate.

## Extension points

A new source, filter, transition, encoder or output is a type that implements the matching trait
in `studiocraft-plugin`. The type is registered in a `Registry` (see
`studiocraft_sources::register` and `studiocraft_outputs::register`). The window builds its
property forms from each plugin's `SettingsSchema`, so a new plugin does not need window code.

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
window's height when that is less, and never under 120 points. Scenes and Sources are 300 and 340
points wide by default and the Audio Mixer takes the rest; no column is narrower than 200 points
while the window has room. The strip's top edge and the 1-point dividers between docks are
`craft-ui` `ResizeHandle`s with 8-point hit bands. The header actions take a press or a drag
that starts on them, so the top edge resizes only from outside the actions. A dragged height (up to 45% of the window) and
dragged widths are saved in `config.toml` with the docks' visibility, and Docks › Reset Docks
clears them.

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
- **Audio and video capture on Linux.** Display, window and device capture use `xcap` and `cpal` on
  Windows and macOS only. On Linux these sources report that capture is unavailable.
- **Encoding.** Recording and streaming need an ffmpeg program installed separately. StudioCraft
  runs it as a process and does not link it.

## Dependency licenses

`cargo xtask licenses` accepts permissive software licenses (MIT, Apache-2.0, BSD, Zlib, ISC and
similar), the OFL-1.1 and Ubuntu font licenses of egui's built-in fonts, and the IJG license.
One package is accepted by name: `option-ext` (MPL-2.0), which `dirs-sys` uses through
`craft-settings`. The list is `EXCEPTIONS` in `xtask/src/licenses.rs`.
