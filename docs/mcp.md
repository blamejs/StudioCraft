# MCP server

`studiocraft-cli mcp` is a Model Context Protocol (MCP) server on standard input and output. An MCP
client registers it as a program and drives StudioCraft through its tools, resources and prompts.

The `studiocraft-mcp` crate holds the server. Every tool runs StudioCraft commands by id, the same
commands the menus, shortcuts and the control channel run.

The server runs on one of two backends:

- **Headless.** `studiocraft-cli mcp` runs a studio in its own process, with the built-in plug-ins,
  a new scene collection and default settings. It composites on the system's graphics device when
  one is available.
- **App.** `studiocraft-cli mcp --connect <port>` sends every call to the app started with
  `studiocraft --control <port>`, as control requests ([control-protocol.md](control-protocol.md)).

[Headless and app backends](#headless-and-app-backends) lists what differs between the two.

## Install and register

Build the command-line tool and put it on the `PATH`:

```sh
cargo install --path apps/studiocraft-cli
```

### Headless

Register the server with an MCP client as a program that takes `mcp`:

```json
{"mcpServers": {"studiocraft": {"command": "studiocraft-cli", "args": ["mcp"]}}}
```

Add `--read-root` and `--write-root` to confine its file access ([Confining file
access](#confining-file-access)), `--collection <file>` to open a scene collection before the first
message, and `--allow-streaming` to let it start a stream ([Streaming](#streaming)):

```json
{"mcpServers": {"studiocraft": {"command": "studiocraft-cli",
  "args": ["mcp", "--read-root", "/work/show", "--write-root", "/work/show/out", "--collection", "/work/show/show.json"]}}}
```

When the `--collection` file can't be opened, the server exits with code 1 and says why.

### The running app

Start the app with a control port and a session token, as
[Connecting](control-protocol.md#connecting) shows for POSIX shells and PowerShell. Then register
`studiocraft-cli mcp --connect <port>` with the same token in `STUDIOCRAFT_CONTROL_TOKEN`:

```json
{"mcpServers": {"studiocraft": {"command": "studiocraft-cli", "args": ["mcp", "--connect", "9876"],
  "env": {"STUDIOCRAFT_CONTROL_TOKEN": "<the token the app was started with>"}}}}
```

`mcp <port>` is read as `mcp --connect <port>`. The port may be written as `9876` or
`127.0.0.1:9876`; only addresses on this computer are accepted. The server starts without
connecting; when no app listens on the port, each tool call fails with "StudioCraft is not
listening at 127.0.0.1:<port>".

## Options

The options are read before the server starts. These are errors, and `studiocraft-cli` exits with
code 2 without starting the server:

- An unknown option or a missing value.
- `--connect` given twice, or a port given twice.
- A port of 0 or above 65535, or an address not on this computer.
- `--collection` given twice.
- `--read-root`, `--write-root`, `--allow-streaming` or `--collection` together with a port.

## Headless and app backends

| | Headless (`mcp`) | App (`mcp --connect <port>`) |
| --- | --- | --- |
| Studio | The server uses its own studio, with a new scene collection and default settings. | The server uses the running app's studio, with its open scene collection, profile and settings. |
| Window tools (`ui_*`, `app_quit`) | `tools/list` leaves them out. A call fails and says how to start the app for it. | `tools/list` lists them, and calls go to the window. |
| Window commands (`view.stats`, `collection.import`, `app.settings`, `app.exit`) | The server refuses them with an error that says how to start the app for them. | The window runs them. |
| `collection_open`, `collection_save` | The tools read and write the file in the server's process. | The tools run the window's `collection.import` and `collection.export`. |
| Streaming | A stream starts only with `--allow-streaming`. | A stream starts as the app's Start Streaming does. |
| File roots | `--read-root` and `--write-root` apply. | The server does not accept them. |
| `render_preview` | The tool waits up to 2 seconds for a running transition to end and for the shown scene's sources to show their first picture, then draws. It fails without a graphics device. | The tool waits up to 5 seconds for the app's compositor. |

On the app backend, connecting waits at most 800 ms, and a reply may be up to 256 MiB long. When
the connection drops after a request was sent, a request that only reads is sent once more on a
new connection. A request that changes something is not repeated, and the error says that it may
or may not have run.

### Streaming

A stream publishes the program to the Real-Time Messaging Protocol (RTMP) server in
Settings › Stream. The headless backend starts a stream only when the server was started with
`--allow-streaming`. Without it, `stream_start`, `stream.start` and a `stream.toggle` that would
start a stream fail and nothing starts. The app backend starts a stream as its Start Streaming menu
item does.

## Confining file access

```sh
studiocraft-cli mcp --read-root /work/show --write-root /work/show/out
studiocraft-cli mcp --read-root /work/show --read-root /work/media --write-root /work/show/out
```

`--automation-read-root` and `--automation-write-root` are accepted as other names for the two
flags. The two flags keep the headless backend's file access inside the folders given.

### Reads and writes

- Every file a call reads must lie inside a read root: `collection_open`, the files a scene
  collection's sources, filters and transition name when it is opened, and the path settings of
  `source.add`, `source.settings`, `filter.settings` and `transition.settings` (for example the
  `file` of an `image` or `media` source), whichever tool runs them.
- Every file a call writes must lie inside a write root: `collection_save`, and the recording
  folder (Settings › Output, or the Videos folder when it is empty) for `record_start`,
  `record.start` and a `record.toggle` that would start a recording, and for `replay_save` and
  `replay.save`. With audio tracks on for the stream, the recording folder is also checked for
  `stream_start`, `stream.start` and a `stream.toggle` that would start the stream.
  `captions.download` writes a speech model into the models folder, so that folder, the model's
  folder and its files must lie inside a write root. `captions.start`, and a `captions.toggle` that
  would start captions, read a model from that folder, so they must lie inside a read root. The
  caption status reports `downloaded` as null for a model outside the read roots. `audio_tracks_repair` and `audio_tracks.repair` rewrite files, so their `folder` must lie inside
  a write root. With or without roots, the studio repairs only the recording folder and the
  folders inside it. `audio_tracks_open_last` and `audio_tracks.open_last` need the last session
  file inside a read root.
- `--collection <file>` is opened through the same checks, so the file and the files it names
  must lie inside a read root.

### How paths are checked

- Each flag may be given more than once; a path inside any root of its kind is allowed. Each root
  must be an existing folder. A kind of access with no root of its own is not granted.
- A path is made absolute against the server's working directory and its links are followed. For a
  file that does not exist yet, its nearest existing folder's links are followed and the rest must
  be plain names, so `..` below a missing folder and a link that leads nowhere are refused.
- The result is compared with each root folder name by folder name: `/work2/x` is not inside
  `/work`. On Windows case is ignored, and device names (`CON`, `NUL`, `COM1`), a `:` after the
  drive, a file or folder name that ends in a dot or a space, and a path that starts with `\\?\` or
  `\\.\` are refused.
- A call runs with the path that was checked: the path settings a command stores or a collection
  opens with, and the `folder` a repair reads, are the absolute paths with their links followed.
  Before a command that writes to the recording folder runs, the recording folder setting is set
  to that folder's checked path, and the setting keeps it afterwards. A
  path that resolves to a name that is not valid Unicode is refused, and so is one whose stored
  text, checked again, resolves to a different path.
- A refused call fails before anything runs. Without either flag nothing is checked.
- The roots apply to the headless backend only. Combining them, `--allow-streaming` or
  `--collection` with a port is an error at startup (exit code 2).

## Protocol

The server reads one JSON-RPC message per line, up to 4 MiB, and answers protocol versions
`2025-03-26` and `2024-11-05`. At initialization it reports the `tools`, `resources`, `prompts` and
`completions` capabilities, and its instructions describe the studio, the backend in use and the
community links.

Units are the same in every tool, resource and prompt:

- Canvas coordinates are pixels, x to the right and y down, with the origin at the top-left. The
  default canvas is 1920 × 1080.
- Rotation is in degrees.
- Volume gain is 0 to 1.
- Durations and audio sync offsets are in milliseconds.
- The window tools take window points, with (0, 0) at the window's top left.

## Tools

Every tool has a title and the `readOnlyHint`, `destructiveHint`, `idempotentHint` and
`openWorldHint` annotations. An unknown tool or an argument a tool does not take is a JSON-RPC
error -32602. A command that fails is a tool result with `isError: true` and the error message.

| Tool | Arguments | What it does |
| --- | --- | --- |
| `command_list` | `filter?`, `enabled_only?` | Lists every command with `id`, `label`, `params`, `undo`, `scope`, `enabled` and `shortcut`. `filter` keeps ids and labels that contain the text, ignoring case; `enabled_only` keeps commands that can run now. |
| `command_run` | `id`, `params?` | Runs one command and returns its result. |
| `command_batch` | `steps: [{id, params?}]`, `stop_on_error?` | Runs the steps in order and reports `completed`, `failed`, `skipped` and each step's result or error. It stops at the first failed step unless `stop_on_error` is `false` (default `true`). A batch of more than 1,000 steps, or one with a malformed step, runs nothing. |
| `doc_inspect` | `detail?: summary \| full` | Returns the scene collection, studio mode, program and preview scenes, outputs, statistics and canvas. `summary` (the default) leaves out source, filter and transition settings. |
| `render_preview` | `view?: program \| preview`, `max_side?` | Returns a PNG image of the program (the default), or of the preview in studio mode. `max_side` is 0 to 4096 pixels (default 960; 0 is full size). On the headless backend it first waits up to 2 seconds for a running transition to end and for sources just added to show their first picture. Without a graphics device it fails with "nothing is drawn: the studio runs without a graphics device". |
| `undo`, `redo` | none | Runs `edit.undo` or `edit.redo`. |
| `scene_add` | `name?` | Runs `scene.add`. The name defaults to the next free "Scene N". The tool returns `scene`, the new scene's id. |
| `scene_select` | `scene` | Runs `scene.select`, by id or name. In studio mode it sets the preview scene. |
| `source_add` | `kind`, `scene?`, `name?`, `settings?`, `transform?` | Runs `source.add`, in the scene being edited unless `scene` names one. The tool returns `source`, `item` and `scene`. |
| `item_transform` | `item`, `scene?`, `transform` | Runs `item.transform`. `item` is the item's id or the name of the source it shows. |
| `source_audio` | `source`, `sync_offset_ms?`, `monitor?`, `record_track?` | Runs `source.sync_offset` when `sync_offset_ms` is given (0 to 5000), `source.monitor` when `monitor` is given (`off`, `monitor_only` or `monitor_and_output`) and `source.set_record_track` when `record_track` is given (`true` or `false`), then returns the source's `audio` settings. A value out of range is an error and changes nothing. With no value it only reads the settings. `record_track` is `null` until set; a source whose `monitor` sends it to the program mix (`off` or `monitor_and_output`) is then written to its own track and a monitor-only source is not. |
| `transition_set` | `kind`, `ms?` | Runs `transition.set`, then `transition.duration` when `ms` is given. |
| `studio_transition` | `kind?`, `ms?` | Runs `studio.transition`: in studio mode, it moves the preview scene to the program. `kind` and `ms` apply to this switch only. |
| `collection_open` | `path` | Replaces the scene collection with the one in the file. The undo history starts empty. |
| `collection_save` | `path` | Writes the scene collection to the file, replacing any file at that path. |
| `app_links` | none | Returns the community links (`app.links`). |
| `source_schema` | `kind`, `settings?` | Runs `source.schema` and returns the source plug-in's settings fields with the choices the system offers now. For `camera`, the `device` field lists the connected cameras and the `format` field the formats of the camera that `settings.device` names. |
| `app_audio` | `action: list \| status \| lanes` | With `list`, runs `app_audio.list`, which answers at once: `supported`, `listing`, `error` and `apps`, the running applications that have audio sorted by name, each with `app` (the value an `app_audio` source's `app` setting takes), `name`, `path`, `pids`, `target_pid`, `playing`, `peak` (`null` without a current reading) and `within` (the listed application whose capture already records the process tree under `target_pid`). Before the first scan `listing` is true and `apps` is empty. With `status`, runs `app_audio.status` and returns `support`, `discovery_stalled`, the number of `captures` with `max_captures` (32), the summed `dropped_blocks` and `pool_exhausted` counters, and each capture under `running`. On a system without per-application capture, `list` returns no applications and `error` says why. With `lanes`, runs `app_audio.lanes` and returns `desktop_heard` (the program hears a Desktop audio source that is not muted, which also records a routed application that plays on the device it captures) and the Application Audio dock's lanes, microphones first, each with `kind`, `name`, `app`, `source`, `routed`, `routed_heard` (a global audio source of the lane reaches the program), `heard`, `gain`, `muted`, `peak`, `running`, `playing` and `within` ([`app_audio.lanes`](control-protocol.md#sourceschema)). |
| `app_audio_route` | `app`, `routed` | Runs `app_audio.route`: with `routed: true` the program hears the application in every scene, together with the microphone, through a global audio source, and the tool returns that source's id; with `routed: false` that source leaves the global audio sources. Only an application in the current list, or one that already has a source, can be routed. Routing is refused while the program hears another application, routed or shown in the program scene, whose capture records this one's processes, or whose processes this one's capture would record. One undo step. |
| `record_start` | none | Runs `record.start`, then returns the recording's state from `document.inspect`. Nothing happens when a recording already runs. |
| `record_stop` | none | Runs `record.stop`, then returns the recording's state. |
| `stream_start` | none | Runs `stream.start`, then returns the stream's state. The tool is marked `openWorldHint: true`. The headless backend refuses it without `--allow-streaming`. |
| `stream_stop` | none | Runs `stream.stop`, then returns the stream's state. The tool is marked `openWorldHint: true`. |
| `replay_start` | none | Runs `replay.start`, then returns the replay buffer's state. Nothing happens when it already runs. |
| `replay_stop` | none | Runs `replay.stop`, then returns the replay buffer's state. Replays being saved are still written. |
| `replay_save` | none | Runs `replay.save`, which writes what the replay buffer holds to a new MKV file in the recording folder in the background, then returns the replay buffer's state. It is an error when the replay buffer is not running or holds nothing yet. |
| `audio_tracks_status` | none | Runs `audio_tracks.status` and returns the settings (`enabled`, `when`, `include_program`, `buffer_mib`, `segment_gib`, `after_stop` and `editor_path`), `last_session` (the session file `audio_tracks_open_last` opens, or `null`), `active` and `finishing` (takes still writing), `repairing` (true while a repair runs) and `last_repair` (what the last repair did, as below), while a take runs the `anchor` it follows (`recording` or `stream`) and the `take_buffer_mib` and `take_segment_gib` it started with (a changed write buffer or file size applies from the next take), and for the last take its `state` (`recording`, `finishing`, `done` or `stopped`), `folder` (once the take's thread has made it), `tracks`, `seconds`, `bytes`, `dropped_seconds`, `low_space`, `track_limit` (true once a source was left without a file because the take made its 256 tracks) and `source_limit` (true while a take runs and more sources are set to record their own track than the 64 tracks recorded at once, the program included; the later sources are not recorded), with `stopped_at` and `reason` when it stopped early. |
| `audio_tracks_repair` | `folder` | Runs `audio_tracks.repair`, which repairs the takes in `folder` (a take's `Audio Files` folder, its `<name> Audio` folder, or a folder holding such folders) that ended without finishing their files, on a background thread, and returns `started` and `folder` at once. When it ends, `audio_tracks_status` shows `last_repair` with `repaired`, each take's `folder` with its `files` and their `seconds`, and `failed`, each take that could not be repaired with its `folder` and `error`. `folder` must be the recording folder or a folder inside it, with links followed; a folder outside it, or one that does not exist, is an error. At most 4 repairs run at once. Each repaired take also has its `session` file, or `null`. |
| `audio_tracks_open_last` | none | Runs `audio_tracks.open_last`, which opens the last take's session file (`<name> Audio/<name>.scraft`) in the audio editor (the editor path setting, or `soundcraft` on the `PATH`), or the take's folder when the editor cannot be found or started, on a background thread, and returns `started` and `session` at once. It is an error when no take has ended with a session since the app started, when 4 openings already run, and when the opening's thread cannot start. Each call starts the editor or file manager again. |
| `audio_tracks_marker` | `name?` | Runs `audio_tracks.marker`, which marks the running take at this moment; the take's session file lists the marker at that sample. `name` is 1 to 256 characters with no control characters (default "Marker N", the take's Nth marker). Returns the marker's `name` and `seconds` from the take's first sample. It is an error when no take runs, when the take is ending, and when 1,024 markers are waiting to be written. |
| `output_status` | none | Returns the `recording`, `streaming` and `replay` entries of `document.inspect`: `active`, `status`, `target` (the file or server) and `seconds`. The `replay` entry also has `saving` (replays still being written) and, while it runs, `bytes` (the bytes it holds). |

`transform` is an object with `position` `{x, y}` (the top-left corner in canvas pixels), `scale`
`{x, y}` (1 is the source's own size), `rotation` in degrees and `crop` `{left, top, right,
bottom}` in source pixels. Fields left out take their defaults.

### Window tools

These tools are listed only when the server sends calls to the app (`studiocraft-cli mcp --connect <port>`).
On the headless backend `tools/list` leaves them out, and a call to one is a tool result with
`isError: true` that says to start `studiocraft --control PORT` with `STUDIOCRAFT_CONTROL_TOKEN`
set and run `studiocraft-cli mcp --connect PORT`. Arguments are checked before anything is sent: a value of
the wrong type or out of range is error -32602 (a string `x`, for example, is not read as 0).
`ui_inspect`, `ui_screenshot` and `ui_menu_list` are marked `readOnlyHint: true`.

The earlier `studio_state`, `studio_commands` and `studio_run` tools have been removed. Use
`doc_inspect`, `command_list` and `command_run` instead.

| Tool | Arguments | What it does |
| --- | --- | --- |
| `ui_inspect` | none | Runs `ui.inspect` and returns the window's size, dialogs, docks, preview rectangle, menus, selection and status messages. |
| `ui_screenshot` | `max_side?` | Runs `ui.screenshot` and returns a PNG image. `max_side` is 0 to 4096 pixels (default 1600; 0 is full size). A reply that is not valid base64 or not a PNG is an error. |
| `ui_click` | `x`, `y`, `button?`, `count?`, `modifiers?` | Runs `ui.click`. `x` and `y` are window points of 0 or more; `button` is `primary` (the default), `secondary` or `middle`; `count` is 1 to 3 (default 1); `modifiers` is a list of `cmd`, `ctrl`, `alt` and `shift`. |
| `ui_drag` | `from: {x, y}`, `to: {x, y}`, `steps?`, `modifiers?` | Runs `ui.drag` with the primary button. `steps` is 1 to 200 (default 12). |
| `ui_key` | `key` | Runs `ui.key`, for example `Cmd+Z`. A key pressed twice is written twice: `A A`. |
| `ui_type` | `text` | Runs `ui.type`, which types up to 10,000 characters into the focused field. |
| `ui_menu_list` | none | Runs `ui.menu.list` and returns the menu bar as a tree. |
| `ui_menu_invoke` | `path` | Runs `ui.menu.invoke`, for example `Scene Collection > New...`. Three dots match an ellipsis when no label in that menu is written exactly as given ([`path`](control-protocol.md#parameters-and-limits)). An unknown path, a submenu or a disabled item is an error. |
| `ui_dialog` | `answer: ok \| cancel`, `text?` | Runs `ui.dialog`, which answers the open name prompt or the exit confirmation as its buttons do. `ok` presses OK, or Stop and Exit; `cancel` presses Cancel. `text` replaces the name in a name prompt first. A name the prompt refuses is an error and the prompt stays open. |
| `app_quit` | none | Runs `app.quit`, which closes the window as File › Exit does. While a recording, stream or replay buffer runs, the window asks first and the result's `confirming` is `true`. |

## Resources

| URI | Contents |
| --- | --- |
| `studiocraft://document` | `document.inspect` with `detail: full`, as JSON. |
| `studiocraft://commands` | `engine.commands`, as JSON. |

[Resource templates](#resource-templates) read one scene, source, command or plug-in at a time.

### Resource templates

`resources/templates/list` lists four templates. Each reads one thing as JSON:

| URI template | Contents |
| --- | --- |
| `studiocraft://scene/{id}` | The scene with this id or name: its items, each with `source_name`, and `program` and `preview`, which say whether it is the program scene or, in studio mode, the preview scene. |
| `studiocraft://source/{id}` | The source with this id or name, with its settings and filters, `scenes` (the names of the scenes that place it) and `global_audio`. |
| `studiocraft://command/{id}` | The command with this id, as `engine.commands` lists it. |
| `studiocraft://plugin/{kind}` | The source, filter or transition plug-in with this id: `type`, `name`, `version` and `schema` (its settings fields with their defaults), from the `plugin.list` command. |

A numeric id is looked up as an id first and then as a name. Names may be percent-encoded
(`studiocraft://scene/Be%20right%20back`). `resources/read` with any other URI, or with a value
that names nothing, is error -32002.

## Prompts

`prompts/list` lists three prompts, and `prompts/get` returns one user message with the arguments
filled in. A missing or empty required argument, an argument the prompt does not take, a value
that is not a string and an unknown prompt are error -32602.

| Prompt | Arguments | What it asks for |
| --- | --- | --- |
| `build_scene` | `description` | A new scene built from the description with `scene_add`, `source_add` and `item_transform`, checked with `render_preview`. |
| `lower_third` | `text`, `color?` (`#rrggbbaa`, default `#ffffffff`), `scene?` | A `text` source placed in the lower band of the canvas. |
| `check_collection` | none | A report of sources whose files are missing, empty scenes, scenes whose items are all hidden, and sources no scene places. Nothing is changed. |

## Completions

`completion/complete` suggests values for a prompt argument (`ref/prompt`) or a resource template's
variable (`ref/resource`):

| Target | Suggested values |
| --- | --- |
| `studiocraft://command/{id}` | Command ids. |
| `studiocraft://scene/{id}` and the `scene` argument of `lower_third` | Scene names. |
| `studiocraft://source/{id}` | Source names. |
| `studiocraft://plugin/{kind}` | Plug-in ids. |
| `color` | A short list of colors. |

A value is suggested when it starts with the typed text, ignoring case. A reply holds at most 100
values; `total` counts every match and `hasMore` is `true` when some were left out. An unknown
prompt, argument or template gives no values.

## Examples

Add a scene, place a color source in it, turn the source, and look at the result. Each line is one
request:

```json
{"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "scene_add", "arguments": {"name": "Intro"}}}
{"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "source_add", "arguments": {"kind": "color", "scene": "Intro", "name": "Backdrop", "settings": {"color": "#202830ff", "width": 640, "height": 360}}}}
{"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "item_transform", "arguments": {"item": "Backdrop", "scene": "Intro", "transform": {"position": {"x": 640, "y": 360}, "rotation": 15}}}}
{"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "render_preview", "arguments": {"max_side": 480}}}
```

Run several commands as one call and keep going past a failed step:

```json
{"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {"name": "command_batch", "arguments": {"stop_on_error": false, "steps": [
  {"id": "scene.add", "params": {"name": "Break"}},
  {"id": "transition.set", "params": {"kind": "fade"}},
  {"id": "transition.duration", "params": {"ms": 500}}]}}}
```

Read one scene by name, and ask for the scene names that start with `In`:

```json
{"jsonrpc": "2.0", "id": 6, "method": "resources/read", "params": {"uri": "studiocraft://scene/Intro"}}
{"jsonrpc": "2.0", "id": 7, "method": "completion/complete", "params": {"ref": {"type": "ref/resource", "uri": "studiocraft://scene/{id}"}, "argument": {"name": "id", "value": "In"}}}
```

Record for a while against the running app, then check the file:

```json
{"jsonrpc": "2.0", "id": 8, "method": "tools/call", "params": {"name": "record_start", "arguments": {}}}
{"jsonrpc": "2.0", "id": 9, "method": "tools/call", "params": {"name": "record_stop", "arguments": {}}}
{"jsonrpc": "2.0", "id": 10, "method": "tools/call", "params": {"name": "output_status", "arguments": {}}}
```
