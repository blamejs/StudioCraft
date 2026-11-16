# MCP server

`studiocraft-cli mcp` is an MCP server on standard input and output. The `studiocraft-mcp` crate
holds it. Every tool runs StudioCraft commands by id, the same commands the menus, shortcuts and
the control channel run.

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

Start the app with a control port and a session token of 64 hexadecimal characters in
`STUDIOCRAFT_CONTROL_TOKEN`. The app does not start when `--control` is given without a valid
token.

```sh
# POSIX shells
export STUDIOCRAFT_CONTROL_TOKEN=$(openssl rand -hex 32)
studiocraft --control 9876 &
```

```powershell
# Windows (PowerShell)
$bytes = [byte[]]::new(32)
[Security.Cryptography.RandomNumberGenerator]::Create().GetBytes($bytes)
$env:STUDIOCRAFT_CONTROL_TOKEN = -join ($bytes | ForEach-Object { $_.ToString('x2') })
Start-Process studiocraft -ArgumentList '--control', '9876'
```

Then register `studiocraft-cli mcp --connect <port>` with the same token:

```json
{"mcpServers": {"studiocraft": {"command": "studiocraft-cli", "args": ["mcp", "--connect", "9876"],
  "env": {"STUDIOCRAFT_CONTROL_TOKEN": "<the token the app was started with>"}}}}
```

`mcp <port>` is read as `mcp --connect <port>`. The port may be written as `9876` or
`127.0.0.1:9876`; only addresses on this computer are accepted. The server starts without
connecting; when no app listens on the port, each tool call fails with "StudioCraft is not
listening at 127.0.0.1:<port>".

### Options

The options are read before the server starts. These are errors, and `studiocraft-cli` exits with
code 2 without starting the server:

- an unknown option or a missing value;
- `--connect` given twice, or a port given twice;
- a port of 0 or above 65535, or an address not on this computer;
- `--collection` given twice;
- `--read-root`, `--write-root`, `--allow-streaming` or `--collection` together with a port.

### Headless and app backends

| | Headless (`mcp`) | App (`mcp --connect <port>`) |
| --- | --- | --- |
| Studio | Its own, with a new scene collection and default settings. | The running app's, with its open scene collection, profile and settings. |
| Window tools (`ui_*`, `app_quit`) | Left out of `tools/list`. A call fails and says how to start the app for it. | Listed and sent to the window. |
| Window commands (`view.stats`, `collection.import`, `app.settings`, `app.exit`) | Refused with an error that says how to start the app for them. | Run by the window. |
| `collection_open`, `collection_save` | Read and write the file in the server's process. | Run the window's `collection.import` and `collection.export`. |
| Streaming | Only with `--allow-streaming`. | As the app's Start Streaming does. |
| File roots | `--read-root` and `--write-root`. | Not accepted. |
| `render_preview` | Waits up to 2 seconds for a running transition to end and for the shown scene's sources to show their first picture, then draws. Fails without a graphics device. | Waits up to 5 seconds for the app's compositor. |

On the app backend, connecting waits at most 800 ms, and a reply may be up to 256 MiB long. When
the connection drops after a request was sent, a request that only reads is sent once more on a
new connection. A request that changes something is not repeated, and the error says that it may
or may not have run.

### Streaming

A stream publishes the program to the RTMP server in Settings › Stream. The headless backend starts
a stream only when the server was started with `--allow-streaming`. Without it, `stream_start`,
`stream.start` and a `stream.toggle` that would start a stream fail and nothing starts. The app
backend starts a stream as its Start Streaming menu item does.

## Confining file access

```sh
studiocraft-cli mcp --read-root /work/show --write-root /work/show/out
studiocraft-cli mcp --read-root /work/show --read-root /work/media --write-root /work/show/out
```

`--automation-read-root` and `--automation-write-root` are accepted as other names for the two
flags. The two flags keep the headless backend's file access inside the folders given:

- Every file a call reads must lie inside a read root: `collection_open`, the files a scene
  collection's sources, filters and transition name when it is opened, and the path settings of
  `source.add`, `source.settings`, `filter.settings` and `transition.settings` (for example the
  `file` of an `image` or `media` source), whichever tool runs them.
- Every file a call writes must lie inside a write root: `collection_save`, and the recording
  folder (Settings › Output, or the Videos folder when it is empty) for `record_start`,
  `record.start` and a `record.toggle` that would start a recording.
- Each flag may be given more than once; a path inside any root of its kind is allowed. Each root
  must be an existing folder. A kind of access with no root of its own is not granted.
- A path is made absolute against the server's working directory and its links are followed. For a
  file that does not exist yet, its nearest existing folder's links are followed and the rest must
  be plain names, so `..` below a missing folder and a link that leads nowhere are refused. The
  result is compared with each root folder name by folder name: `/work2/x` is not inside `/work`.
  On Windows case is ignored, and device names (`CON`, `NUL`, `COM1`) and a `:` after the drive are
  refused.
- A refused call fails before anything runs. Without either flag nothing is checked.
- The roots apply to the headless backend only. Combining them, `--allow-streaming` or
  `--collection` with a port is an error at startup (exit code 2).
- `--collection <file>` is opened through the same checks, so the file and the files it names
  must lie inside a read root.

## Protocol

The server reads one JSON-RPC message per line, up to 4 MiB, and answers protocol versions
`2025-03-26` and `2024-11-05`. At initialization it reports the `tools`, `resources`, `prompts` and
`completions` capabilities, and its instructions describe the studio, the backend in use and the
community links.

Units are the same in every tool, resource and prompt:

- Canvas coordinates are pixels, x to the right and y down, with the origin at the top-left. The
  default canvas is 1920 x 1080.
- Rotation is in degrees.
- Volume gain is 0 to 1.
- Durations are in milliseconds.
- The window tools take window points, with (0, 0) at the window's top left.

### Prompts

`prompts/list` lists three prompts, and `prompts/get` returns one user message with the arguments
filled in. A missing or empty required argument, an argument the prompt does not take, a value
that is not a string and an unknown prompt are error -32602.

| Prompt | Arguments | What it asks for |
| --- | --- | --- |
| `build_scene` | `description` | A new scene built from the description with `scene_add`, `source_add` and `item_transform`, checked with `render_preview`. |
| `lower_third` | `text`, `color?` (`#rrggbbaa`, default `#ffffffff`), `scene?` | A `text` source placed in the lower band of the canvas. |
| `check_collection` | none | A report of sources whose files are missing, empty scenes, scenes whose items are all hidden, and sources no scene places. Nothing is changed. |

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

### Completions

`completion/complete` suggests values for a prompt argument (`ref/prompt`) or a resource template's
variable (`ref/resource`): command ids for `studiocraft://command/{id}`, scene names for
`studiocraft://scene/{id}` and the `scene` argument of `lower_third`, source names for
`studiocraft://source/{id}`, plug-in ids for `studiocraft://plugin/{kind}`, and a short list of
colors for `color`. A value is suggested when it starts with the typed text, ignoring case. A
reply holds at most 100 values; `total` counts every match and `hasMore` is `true` when some were
left out. An unknown prompt, argument or template gives no values.

## Tools

Every tool has a title and the `readOnlyHint`, `destructiveHint`, `idempotentHint` and
`openWorldHint` annotations. An unknown tool or an argument a tool does not take is a JSON-RPC
error -32602. A command that fails is a tool result with `isError: true` and the error message.

| Tool | Arguments | What it does |
| --- | --- | --- |
| `command_list` | `filter?`, `enabled_only?` | Every command with `id`, `label`, `params`, `undo`, `scope`, `enabled` and `shortcut`. `filter` keeps ids and labels that contain the text, ignoring case; `enabled_only` keeps commands that can run now. |
| `command_run` | `id`, `params?` | Runs one command and returns its result. |
| `command_batch` | `steps: [{id, params?}]`, `stop_on_error?` | Runs the steps in order and reports `completed`, `failed`, `skipped` and each step's result or error. It stops at the first failed step unless `stop_on_error` is `false` (default `true`). A batch of more than 1,000 steps, or one with a malformed step, runs nothing. |
| `doc_inspect` | `detail?: summary \| full` | The scene collection, studio mode, program and preview scenes, outputs, statistics and canvas. `summary` (the default) leaves out source, filter and transition settings. |
| `render_preview` | `view?: program \| preview`, `max_side?` | A PNG image of the program (the default), or of the preview in studio mode. `max_side` is 0 to 4096 pixels (default 960; 0 is full size). On the headless backend it first waits up to 2 seconds for a running transition to end and for sources just added to show their first picture. Without a graphics device it fails with "nothing is drawn: the studio runs without a graphics device". |
| `undo`, `redo` | none | `edit.undo` and `edit.redo`. |
| `scene_add` | `name?` | `scene.add`. The name defaults to the next free "Scene N". Returns `scene`, the new scene's id. |
| `scene_select` | `scene` | `scene.select`, by id or name. In studio mode it sets the preview scene. |
| `source_add` | `kind`, `scene?`, `name?`, `settings?`, `transform?` | `source.add`, in the scene being edited unless `scene` names one. Returns `source`, `item` and `scene`. |
| `item_transform` | `item`, `scene?`, `transform` | `item.transform`. `item` is the item's id or the name of the source it shows. |
| `transition_set` | `kind`, `ms?` | `transition.set`, then `transition.duration` when `ms` is given. |
| `studio_transition` | `kind?`, `ms?` | `studio.transition`: in studio mode, moves the preview scene to the program. `kind` and `ms` apply to this switch only. |
| `collection_open` | `path` | Replaces the scene collection with the one in the file. The undo history starts empty. |
| `collection_save` | `path` | Writes the scene collection to the file, replacing any file at that path. |
| `app_links` | none | The community links (`app.links`). |
| `record_start` | none | `record.start`, then the recording's state from `document.inspect`. Nothing happens when a recording already runs. |
| `record_stop` | none | `record.stop`, then the recording's state. |
| `stream_start` | none | `stream.start`, then the stream's state. Marked `openWorldHint: true`. The headless backend refuses it without `--allow-streaming`. |
| `stream_stop` | none | `stream.stop`, then the stream's state. Marked `openWorldHint: true`. |
| `output_status` | none | The `recording` and `streaming` entries of `document.inspect`: `active`, `status`, `target` (the file or server) and `seconds`. |

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

| Tool | Arguments | What it does |
| --- | --- | --- |
| `ui_inspect` | none | `ui.inspect`: the window's size, dialogs, docks, preview rectangle, menus, selection and status messages. |
| `ui_screenshot` | `max_side?` | `ui.screenshot`, returned as a PNG image. `max_side` is 0 to 4096 pixels (default 1600; 0 is full size). A reply that is not valid base64 or not a PNG is an error. |
| `ui_click` | `x`, `y`, `button?`, `count?`, `modifiers?` | `ui.click`. `x` and `y` are window points of 0 or more; `button` is `primary` (the default), `secondary` or `middle`; `count` is 1 to 3 (default 1); `modifiers` is a list of `cmd`, `ctrl`, `alt` and `shift`. |
| `ui_drag` | `from: {x, y}`, `to: {x, y}`, `steps?`, `modifiers?` | `ui.drag` with the primary button. `steps` is 1 to 200 (default 12). |
| `ui_key` | `key` | `ui.key`, for example `Cmd+Z`. A key pressed twice is written twice: `A A`. |
| `ui_type` | `text` | `ui.type`: up to 10,000 characters into the focused field. |
| `ui_menu_list` | none | `ui.menu.list`: the menu bar as a tree. |
| `ui_menu_invoke` | `path` | `ui.menu.invoke`, for example `Scene Collection > New...`. An unknown path, a submenu or a disabled item is an error. |
| `ui_dialog` | `answer: ok \| cancel`, `text?` | `ui.dialog`: answers the open name prompt or the exit confirmation as its buttons do. `ok` presses OK, or Stop and Exit; `cancel` presses Cancel. `text` replaces the name in a name prompt first. A name the prompt refuses is an error and the prompt stays open. |
| `app_quit` | none | `app.quit`: closes the window as File › Exit does. While a recording or stream runs, the window asks first and the result's `confirming` is `true`. |

The earlier `studio_state`, `studio_commands` and `studio_run` tools have been removed. Use
`doc_inspect`, `command_list` and `command_run` instead.

## Resources

| URI | Contents |
| --- | --- |
| `studiocraft://document` | `document.inspect` with `detail: full`, as JSON. |
| `studiocraft://commands` | `engine.commands`, as JSON. |

[Resource templates](#resource-templates) read one scene, source, command or plug-in at a time.

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
