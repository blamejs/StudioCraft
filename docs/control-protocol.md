# Control protocol

Scripts, test tools and the Model Context Protocol (MCP) server drive a running StudioCraft through
its control channel: a local TCP port that takes one JSON request per line.

## Connecting

Install the app and the command-line tool on the `PATH` from the repository root:

```sh
cargo install --path .
cargo install --path apps/studiocraft-cli
```

`studiocraft --control <port>` accepts control requests on `127.0.0.1:<port>`. Every request must
carry the session token from the `STUDIOCRAFT_CONTROL_TOKEN` environment variable the app was
started with (64 hexadecimal characters). The app does not start when `--control` is given without
a valid token.

In a POSIX shell, make a token and start the app with it:

```sh
export STUDIOCRAFT_CONTROL_TOKEN=$(openssl rand -hex 32)
studiocraft --control 9876 &
```

In Windows PowerShell, the same steps are:

```powershell
$bytes = [byte[]]::new(32)
[Security.Cryptography.RandomNumberGenerator]::Create().GetBytes($bytes)
$env:STUDIOCRAFT_CONTROL_TOKEN = -join ($bytes | ForEach-Object { $_.ToString('x2') })
Start-Process studiocraft -ArgumentList '--control', '9876'
```

`studiocraft-cli send` and `studiocraft-cli mcp --connect` read the token from
`STUDIOCRAFT_CONTROL_TOKEN` as the app does, so run them where that variable holds the same token.
To copy the token into another shell or an MCP client's settings, print it with
`echo $STUDIOCRAFT_CONTROL_TOKEN` in a POSIX shell or `$env:STUDIOCRAFT_CONTROL_TOKEN` in
PowerShell.

## Requests and replies

Each request and each reply is one line of JSON:

```json
{"id": 1, "token": "<token>", "method": "run", "params": {"command": "scene.add", "params": {}}}
{"id": 1, "ok": true, "result": {"scene": 7}}
{"id": 2, "ok": false, "error": "unknown command `scene.fly`"}
```

The app serves at most 8 connections at once. A connection closes after 300 seconds without a
request. A request line may be up to 1 MiB long. A request waits up to 30 seconds for the window to
answer.

## Methods

| Method | Params | Result |
| --- | --- | --- |
| `engine.execute` | `{"command": "<id>", "params"?: {...}}` | The command's result. |
| `engine.commands` | `{"filter"?: "<text>"}` | Every command, or those whose id or label contains `filter` (case is ignored). |
| `document.inspect` | `{"detail"?: "summary" \| "full"}` | The scene collection, studio mode, recording, stream and replay buffer state (`recording`, `streaming`, `replay`), frame statistics and canvas. `summary` (the default) leaves out the `settings` of sources, filters and the transition. |
| `render.preview` | `{"view"?: "program" \| "preview", "max_side"?: 0-4096}` | `{"width", "height", "png_base64"}`: a PNG of the program, or of the preview in studio mode. |
| `ui.inspect` | none | The window's layout and open dialogs ([`ui.inspect`](#uiinspect)). |
| `ui.menu.list` | none | The menu bar as the window shows it, as a tree ([`ui.menu.list`](#uimenulist)). |
| `ui.menu.invoke` | `{"path": "Scene Collection > New…"}` | `{"command", "result"}`: the item's command and its result. |
| `ui.click` | `{"x", "y", "button"?, "count"?, "modifiers"?}` | `{"queued": true}` |
| `ui.drag` | `{"from": {"x", "y"}, "to": {"x", "y"}, "steps"?, "modifiers"?}` | `{"queued": true}` |
| `ui.key` | `{"key": "Cmd+Z"}` | `{"queued": true}` |
| `ui.type` | `{"text": "..."}` | `{"queued": true}` |
| `ui.dialog` | `{"answer": "ok" \| "cancel", "text"?: "..."}` | `{"dialog", "closed": true}`: `dialog` is `prompt` (with the accepted `name` after OK) or `confirm_exit` (with `exiting`). |
| `ui.screenshot` | `{"max_side"?: 0-4096}` | `{"width", "height", "png_base64"}`: a PNG of the whole window. |
| `app.quit` | none | `{"confirming": bool}`: whether the window asks first because a recording, stream or replay buffer runs. |

The older method names still work. `run` is `engine.execute` and `commands` is
`engine.commands`. `state` returns the full `document.inspect` result together with a `ui` object
holding the window's state (selected item, open windows, docks, notices, profiles and
collections).

`engine.execute` runs every engine command (for example `scene.add`, `source.add`,
`item.transform`, `record.toggle`, `studio.transition`) and every window command (for example
`view.stats`, `profile.new`, `selection.fit`). A failed command is an error with its message.
The message is in English. The window's status bar shows it as well, in the interface language
when the language's catalog has the message.
[Commands with their own parameters](#commands-with-their-own-parameters) describes `app.info`,
`app.links`, `app.check_updates`, `plugin.list`, `source.schema`, `source.sync_offset`,
`source.monitor`, `audio.loudness`, `source.set_record_track`, the `present.*` commands, `audio_tracks.status`, `audio_tracks.repair`,
`audio_tracks.open_last`, `audio_tracks.marker`, `record.marker`, the `captions.*` commands, the
`dock.layout_*` commands, `video.displays`, `video.canvas_from_display` and `canvas.relayout`.

Each entry of `engine.commands` has these fields:

| Field | Value |
| --- | --- |
| `id` | The command id. |
| `label` | The command's label. |
| `params` | A short description of the parameters. |
| `undo` | Whether Edit › Undo takes the command back. |
| `scope` | `engine` or `window`. |
| `enabled` | Whether the command can run now, as its menu item shows. |
| `shortcut` | The first shortcut, such as `Cmd+Z`, or `null`. |

### Parameters and limits

Every request's `params` is an object or left out. A parameter of the wrong type, or a value out
of range, is an error; the app does not fall back to a default for it. Defaults apply only to
parameters that are left out.

| Parameter | Allowed values | Default |
| --- | --- | --- |
| `x`, `y` | Window points, with (0, 0) at the window's top left. A point outside the window is an error. `ui.inspect` reports the window's size. | Required |
| `button` | `primary`, `secondary` or `middle`. | `primary` |
| `count` | 1 to 3 presses. | 1 |
| `modifiers` | A list of `cmd`, `ctrl`, `alt` and `shift`. `cmd` is Command on macOS and Ctrl on Windows and Linux. | No modifiers |
| `steps` | 1 to 200 moves. `ui.drag` drags with the primary button. | 12 |
| `key` | A shortcut as Settings › Hotkeys writes it: `Cmd+Z`, `Shift+F11`, `Delete`. A key pressed twice is written twice: `A A`. | Required |
| `text` | At most 10,000 characters. The text goes to the field that has the keyboard focus. | Required |
| `max_side` (pixels) | The longest side of the returned picture. Pictures are never larger than 4,096 pixels on a side. | 0 (full size) |
| `path` | A menu and the labels inside it, separated by `>`. Three dots match an ellipsis when no label in that menu is written exactly as given. An unknown path, a submenu and a disabled item are errors. | Required |

### Input, screenshots and dialogs

`ui.click`, `ui.drag`, `ui.key` and `ui.type` queue input and answer at once. The window takes
one batch of the queued input per frame: a click moves the pointer in one frame and presses in the
next, and a drag makes one move per frame. At most 10,000 frames of input can wait. `ui.inspect`
reports `input_waiting` while some is left.

`ui.screenshot` answers once the window has drawn its next frame and delivered the picture. It
fails when no picture arrives within the 30 second answer limit. At most 8 screenshots can wait
at once. `render.preview` waits up to 5 seconds for the compositor; without a graphics device it
fails with "nothing is drawn: the studio runs without a graphics device".

`app.quit` closes the window as File › Exit does. While a recording, stream or replay buffer runs,
the window asks for confirmation first, and the reply's `confirming` is `true`.

`ui.dialog` answers the open name prompt (New Profile, Rename Scene Collection and the others) or
the exit confirmation as its buttons do. `ok` presses OK in the name prompt and Stop and Exit in
the exit confirmation; `cancel` presses Cancel. In a name prompt, `text` replaces the name before
the button is pressed. A name the prompt refuses, such as one already taken, is an error and the
prompt stays open with the error shown. The exit confirmation takes no `text`. When neither
dialog is open the request is an error.

### `ui.inspect`

| Field | Value |
| --- | --- |
| `window` | `{"width", "height"}` in points. |
| `dialogs` | `settings`, `about`, `stats`, `encoders`, `confirm_exit` and `updates` (the update prompt or the result of Help › Check for Updates; each `true` or `false`), `properties` and `filters` (the ids of the sources whose windows are open) and `prompt` (`{"title", "text", "error"}` of the name prompt, or `null`). |
| `docks` | Each shown dock in the strip (`scenes`, `sources`, `mixer`, `app_audio`) from left to right, with its `rect`: `{"x", "y", "width", "height"}`; its `header` rect; its `title` rect, where a right-click opens the dock's menu; `title_truncated`, whether the title was shortened to fit; and `actions`, each header action with its `name` (`add`, `remove`, `up`, `down`, `properties`) and `rect`, from left to right. |
| `floating_docks` | Each shown dock in a window of its own, with the same fields as `docks`; its `rect` is the window's. |
| `dock_layouts` | The names of the saved dock layouts, in the order they were first saved. |
| `dock_strip` | The strip that holds the docks, or `null` when every dock is hidden: its `rect` and `height`, `saved_height` (the height set by dragging its top edge, or `null`) and `saved_widths` (`scenes`, `sources`, `mixer` and `app_audio`, each a width set by dragging a divider, or `null`) and `order`, every dock's name from left to right, hidden ones included. |
| `output_bar` | The menu row: its `rect`; `transitions`, where the transition picker is (`inline`, `overflow` behind an icon button, or `hidden` by Docks › Transition Controls); `compact`, whether Record and Go Live leave out their labels; and `parts`, each drawn part with its `name` (`menus`, `mode`, `transition`, `duration`, `transition_settings`, `transition_overflow`, `record`, `stream`) and `rect`, from left to right. |
| `preview` | The editable view's `rect` and `scale` (points per canvas pixel), or `null` before it is drawn. In studio mode this is the preview; otherwise it is the program. |
| `studio_mode` | Whether studio mode is on. |
| `menus` | The menu titles. |
| `selected_item` | The selected item's id, or `null`. |
| `notices` | The status bar messages, each `{"text", "error"}`. |
| `frames` | Frames drawn so far. |
| `input_waiting` | Whether queued input is still waiting for a frame. |

### `ui.menu.list`

Each menu and submenu has `label`, `path` and `children`. Each item has `label`, `path`, `id`
(the command it runs), `params` (when the item passes any), `enabled`, `checked` (`true` or
`false` for a check or choice item, `null` for a plain one) and `shortcut`.

## Commands with their own parameters

These are commands, not methods: run each one with `engine.execute`, as the example under
[`app.info` and `app.links`](#appinfo-and-applinks) shows.

### `app.info` and `app.links`

Both engine commands only read. Neither command opens a browser or changes the scene collection.

- `app.info` returns the app's name, id, version, license and `contributors` (the number of people
  the About window lists from `contributors/`).
- `app.links` returns the community links (`discord`, `website`, `github`, `issues`). With
  `{"link": "github"}` it returns `{"url": "..."}` for that one link; an unknown key is an error.

The window's `help.discord`, `help.website`, `help.github` and `help.issues` commands open those
links in the browser.

```sh
studiocraft-cli send 9876 engine.execute '{"command": "app.links", "params": {"link": "issues"}}'
```

### `app.check_updates`

The window's `app.check_updates` command (Help › Check for Updates…) asks the release list for a
newer version and shows the answer in the window; it replies `{"checking": true}`. A release list
that cannot be reached is reported as an error, never as "up to date". Sessions started with
`--no-prefs`, `--screenshot` or `--self-test` never check, and the command fails there with
"update checks are off in this session".

### `plugin.list`

The engine command `plugin.list` only reads. It returns every registered plug-in, grouped as
`sources`, `filters`, `transitions`, `encoders` and `outputs`. Each entry has `id`, `name`,
`version` and `schema` (the settings fields with their types, limits and defaults). Some groups add
fields of their own:

| Group | Added fields |
| --- | --- |
| `sources` | `video` and `audio`. |
| `filters` | `target`. |
| `transitions` | `instant`. |
| `encoders` | `kind`, `codec` (`h264`, `hevc` or `aac`) and `builtin`. `builtin` is `false` for an encoder that runs inside the ffmpeg program, and only such an encoder has `ffmpeg_codec`. |

### `source.schema`

The engine command `source.schema` only reads. With `{"kind": "camera", "settings":
{"device": "..."}}` it returns `{"schema": ...}`, the source's settings fields with the lists the
system offers now: for a camera, the connected cameras and the formats of the camera that
`settings.device` names. `settings` may be left out.

`source.add` and `source.settings` check new values against the schema for the settings they are
given, so a camera format must be one the chosen camera offers. The camera settings are written
this way:

| Setting | Form |
| --- | --- |
| `device` | The camera's name and the system's id joined by U+001F. |
| `format` | `1280x720@30:MJPG` (width, height, frames per second and a four-character code: `MJPG`, `YUYV`, `NV12`, `GREY`, `RGB3` or `BGR3`), or `""` for the camera's default. |

The engine commands `app_audio.list` and `app_audio.status` also only read, and take no
parameters. `app_audio.list` returns `{"supported", "listing", "error", "apps"}` at once and does
not wait for the system. Before the discovery thread's first scan, `listing` is `true` and `apps`
is empty; ask again. Each application has `app`, `name`, `path`, `pids`, `target_pid`, `playing`,
`peak` and `within`. Applications are sorted by name. `playing` is true while one of the
application's audio sessions is active, which includes a paused or silent stream. `peak` gives the
level, 0 to 1, of the process tree a capture of the application records. `within` is the `app`
value of another listed application whose capture already records the process tree under
`target_pid`, or `null`. Other instances of the application, in trees of their own, are not part of
that capture.
Each `app_audio.list` keeps the application list current for the next 60 s and has the meters read
for the next 2 s. `peak` is `null` on a first call, for a reading older than 250 ms, while the
system's meter gives no reading, and for an
application past the 64th, which has no meter row; it holds the newest level on a call made within
2 s of the one before. When the discovery thread is stuck in a system call, `error` says so and the
list is the last one read.

An Application audio capture source's `app` setting takes the `app` value: the program file's name
in lower case, `|`, and the program's path in lower case. This is version 1 of the setting. A value
finds the listed application at its path, in any letter case; when none is there, it finds the one
application with that file name, so a program that moved after an update is still found. A value of
the file name alone finds an application only while one listed application has that file name. An
empty or missing `app` chooses no application. An update with any other value that is not in this
form is refused with an error, and the source keeps its current choice. A saved collection with such
a value still loads: the source stays silent and reports the error until another application is
chosen. `app_audio.status` returns `support` (`supported`, `reason` and `permission`),
`discovery_stalled`, `captures`, `max_captures`, the summed `dropped_blocks`, `pool_exhausted` and
`discontinuities` counters, and `running` with each capture's `app`, `pid`, `state`, `packets`,
`discontinuities`, `dropped_blocks` and `pool_exhausted`. `discontinuities` counts the packets the
system marked as following lost audio. `state` is `starting` while the system activates the capture, `started` once the
stream runs and before audio arrives, then `running`, `failed` or `ended`. A capture whose owner let
go of it while it was stuck in a system call is not listed and does not count against
`max_captures`; while 8 such captures are still stuck, no new capture starts. When the system
cannot capture one application, `supported` is `false` and `error`
and `reason` say why.

`app_audio.lanes` also only reads and takes no parameters. It returns the lanes of the Application
Audio dock as `{"supported", "reason", "listing", "error", "lanes", "desktop_heard"}`.
`desktop_heard` is true when the program hears an `audio_output` source that is not muted; that
capture also records what a routed application plays on the device it captures. `lanes` holds a lane for each `audio_input`
source, then a lane for each listed application, then a lane for each routed application that is
not running. When the program hears more than one source of an application, each source after
the first gets a lane of its own, named after the source, with `app` set to `null`. Each lane has
`kind` (`microphone` or `application`), `name`, `app` (`null` for a microphone), `source` (the id
of the source that carries it, or `null`), `routed`, `routed_heard`, `heard`, `gain`, `muted`,
`peak`, `running`, `playing` and `within`. `routed` is true when the source is a global audio
source. `routed_heard` is true when a global audio source of the lane reaches the program, that
is, its monitoring is not `monitor_only`. `heard` is true when the source is a global audio source
or is shown in the program scene, and its monitoring is not `monitor_only`.

`app_audio.route` takes `app` (a value from `app_audio.lanes` or `app_audio.list`) and `routed`.
With `routed: true` it returns `{"source": id}`: the global audio source that already captures the
application, or a source of the collection that captures it, added to the global audio sources, or
a new Application audio capture source named after the application. A new source is made only for
an application in the current list. A routed source set to `monitor_only` is set to
`monitor_and_output`. Routing an application is an error while the program hears another
listed application, through a global audio source or the program scene, and one of the two names
the other in `within`, directly or through the applications between them, since one capture
records the other's processes. With `routed: false` it takes every source that captures the
application out of the global audio sources, removes the ones no scene shows, and returns
`{"sources": [ids]}`; a scene that shows one of them still plays it. A source saved for a program
in another folder, which captures the listed program only because it has the same file name, is
taken out of the global audio sources and kept. Sources match an application the way a capture
source finds it, so a source saved before the program moved to a new folder is that program's
source. While no listed application matches the value and sources of the collection name programs
at more than one path with its file name, the value matches only sources saved with exactly that
value, so a file name alone matches only sources saved by file name. These values are errors: one
that is not in the `app` form, and an application that is not listed and has no source. Both
directions are one undo step, and a call that changes nothing records none.

```sh
studiocraft-cli send 9876 engine.execute '{"command": "app.links", "params": {"link": "issues"}}'
studiocraft-cli send 9876 engine.execute '{"command": "app_audio.list"}'
studiocraft-cli send 9876 engine.execute '{"command": "app_audio.route", "params": {"app": "player.exe|c:\\apps\\player.exe", "routed": true}}'
```

### `source.sync_offset` and `source.monitor`

`source.sync_offset` takes `{"source", "ms"}` and delays the source's audio by `ms` milliseconds, a
whole number from 0 to 5000; a source without audio is an error.

`source.monitor` takes `{"source", "mode"}`, where `mode` is `off`, `monitor_only` or
`monitor_and_output`; any other value, or a source without audio, is an error. `monitor_only`
plays the source on the monitoring device and leaves it out of recordings and streams.
`document.inspect` reports `monitoring` with the profile's `device`, whether it is `playing`, and
the device's `rate` and `channels` while it plays.

### `audio.loudness`

`audio.loudness` takes no parameters and returns the final mix's loudness as the mixer measures it
(ITU-R BS.1770-4, EBU R 128) since the mixer started: `momentary_lufs` (the last 400 ms),
`short_term_lufs` (the last 3 s), `integrated_lufs`, `range_lu` and `true_peak_dbtp`. A reading is
`null` while it is minus infinity: for silence, before 400 ms or 3 s of program have played, and
for the integrated value until a block passes the gates. `range_lu` is 0 with too little signal.
`generation` counts the mixer's published readings; a reply with the same `generation` as an
earlier one holds the same readings. `target` is the profile's loudness band, `{"low_lufs",
"high_lufs"}`. Its default, −18 to −14 LUFS, is StudioCraft's own value. A profile file whose band
has a bound outside −60 to 0 LUFS, or a low bound that is not below the high bound, is refused, and
such a profile is not saved.

### `source.set_record_track`, `audio_tracks.status` and `audio_tracks.repair`

`source.set_record_track` takes `{"source", "on"}`, where `on` is `true` or `false`, and sets
whether audio tracks write the source to a WAV file of its own; a source without audio is an
error, and Edit › Undo takes it back. Setting the value a source already has from its monitoring
setting changes nothing and adds no undo step. Until it is set, a source whose monitoring setting
sends it to the program mix (`off` or `monitor_and_output`) is written and a monitor-only source is
not. A take writes at most 64 tracks at once, the program mix included; sources after that, in the
collection's order, are not written. Counting sources that left or were replaced during it, one
take makes at most 256 tracks, each split into parts by the file size limit; a source after that is
not written.

`audio_tracks.status` takes no params and returns the audio tracks settings, the output the running
take follows (`anchor`, `recording` or `stream`), whether a repair runs (`repairing`), what the last
repair did (`last_repair`), and the last take's state, as `document.inspect`'s `audio_tracks` entry
does.

`audio_tracks.repair` takes `{"folder"}` and repairs the takes in it that ended without finishing
their files, on a background thread; it returns `{"started": true, "folder"}` at once. When the
repair ends, `last_repair` holds
`{"repaired": [{"folder", "files": [{"file", "seconds"}]}], "failed": [{"folder", "error"}]}`,
where `failed` lists each take that could not be repaired, such as one whose journal does not read.
`folder` must be the recording folder or a folder inside it, compared with the links of both
followed, and the repair reads the folder at that resolved path. An empty `folder`, one that does
not exist, one outside the recording folder, a recording folder that does not exist and a fifth
repair while four run are errors. A take that is still being written, or that another repair
holds, is left unchanged. Each repaired take's entry also has `session`, the session file the
repair wrote, or `null` for a take that did not make its own folder.

### `present.window`, `present.next` and `present.previous`

These commands change which window a [Staged windows](features.md#staged-windows) source
(`staged_windows`) presents. `present.window` takes `{"index", "source"?}`, where `index` is a slot
from 1 to 6, and presents the window in that slot. `present.next` and `present.previous` take
`{"source"?}` and present the next or the previous slot that holds a window, wrapping around.
`source` is a source's id or name. Without it, the command changes every Staged windows source in
the collection that has a window in the slot it moves to. Each returns
`{"presented": [{"source", "slot"}]}` with the sources it changed; a source that already presents
that slot is left out. The presented slot is the source's `presented` setting, so it is saved with
the collection. The commands add no undo step, and neither does a `source.settings` call that
changes only `presented`. Edit › Undo leaves the presented slot as it is. A `source` that is not a Staged windows
source, a collection without one, an `index` outside 1 to 6, and a slot with no window in any
target are errors.

### `audio_tracks.open_last` and `audio_tracks.marker`

`audio_tracks.open_last` takes no params and opens the last take's session file in the audio
editor on a background thread, or the take's folder when the editor cannot be found or started; it
returns `{"started": true, "session"}` at once, and a notice reports what happened. The last session
belongs to the take that stopped last or the repair that started last, of those that wrote one; a
take or repair that finishes after a newer one does not replace it. `audio_tracks.status` returns
it as `last_session`. No session yet and a fifth opening while four run are errors.

`audio_tracks.marker` takes `{"name"}` and marks the running take at this moment; the take's
session lists the marker at that sample. `name` is 1 to 256 characters, not only spaces, with no
control characters; without it the marker is "Marker N", the take's Nth marker. It returns
`{"name", "seconds"}`, where `seconds` counts from the take's first sample. No running take, a take
that is ending, and 1,024 markers waiting to be written are errors.

### `record.marker`

`record.marker` takes `{"name"}` and marks the running recording at this moment. `name` follows
the rules of `audio_tracks.marker`; without it the marker is "Marker N", the recording's Nth
marker. It returns `{"name", "seconds"}`, where `seconds` counts from the recording's first frame
and leaves out the time the recording has skipped so far while the computer was suspended. No running recording and a recording that already holds 10,000 markers are errors. When the
recording stops, `<recording name>.otio` is written beside it with each marker at its frame
([Recording markers](features.md#recording-markers)).

### `captions.*`

`captions.start` takes `{"model"?, "language"?}` and starts live captions of the program audio. A
value left out comes from the first Live Captions source in the collection, and then defaults to
`whisper-base` and `auto` (the language is detected). `model` is an id from `captions.status`;
`language` is one of the 99 ISO 639 codes the models recognize, such as `en`, or `auto`. It returns
`{"model", "language"}`. An unknown model, a model that is not downloaded, another language code, an
empty value and a start while captions run or are still stopping are errors. `captions.stop` stops them; the phrase being spoken
is finished first. `captions.toggle` starts or stops them and returns what `captions.start`
returns, or `{"running": false}`.

`captions.status` takes no params and returns what `document.inspect`'s `captions` entry holds:
`state` (`loading`, `running`, `stopping`, `stopped` or `failed: <reason>`, which stays until the
next start), `model` and `language`
of the running captions, `available` (false in a build without speech recognition), `models_dir`,
`models` (each with `id`, `name`, `bytes`, `downloaded`, `license` and `source`), `download` (the
`model` being downloaded with `done` and `total` bytes, or null), `cues` (finished phrases kept for
caption files), `gaps` (program blocks that arrived out of step) and `pending_files` (recordings
waiting for their caption files).

`captions.download` takes `{"model"?}` and downloads that model into `models_dir` on a background
thread. A model left out comes from the first Live Captions source, and then defaults to
`whisper-base`. It returns `{"model", "downloading": true, "bytes"}` at once. A file already there
with its pinned size and SHA-256 sum is kept; every other file is downloaded and checked against
them, and a file that fails the check is deleted. A status message reports
the end of the download. `captions.download_cancel` stops it. An unknown model and a second
download while one runs are errors.

A recording that ran while captions ran gets `<recording name>.srt`, `.vtt` and `.txt` beside it,
timed from the recording's start. A recording in which no phrase was recognized gets no caption
files. A file of that name that already exists is not replaced. The
files are written once the phrases up to the recording's end are finished, when captions stop, or
30 s after the recording ends. The app checks this on every frame; the headless MCP server checks
it on each request and when it exits.

### `pause.*`

`pause.start` takes `{"media"?, "countdown_s"?, "at_zero"?}` and shows the pause screen in the
program with the scene collection's transition. Each value given replaces its profile setting for
this pause only: `media` is a picture or video file (`""` for none), `countdown_s` is 0 to 43,200
seconds (0 for no countdown) and `at_zero` is `keep` or `resume`. A `null` value, a file type that
is not a picture or video, a path longer than 4,096 bytes or holding a NUL character, and a value
out of range are errors. While paused, the values given apply at once without a new transition,
and a given `countdown_s` starts the countdown again. It returns what `pause.status` returns.

`pause.stop` takes no params and shows the program scene again with the scene collection's
transition. It is an error when the program is not paused. `pause.toggle` pauses with the profile's
settings, or resumes, and returns what `pause.status` returns.

`pause.extend` takes `{"seconds"}`, 1 to 43,200, and adds that time to the countdown, up to 43,200 s
left. A countdown that has reached zero, or a pause without one, counts down from `seconds`. It is
an error when the program is not paused. It returns what `pause.status` returns.

`pause.status` takes no params and returns what `document.inspect`'s `pause` entry holds:
`{"paused": false}`, or `paused: true` with `since_ms` (the time paused), `countdown` (`null`
without one, else `total_s`, `remaining_ms`, `at_zero` and `reached`), `media`, `media_error`
(why the picture or video is not showing, or `null`) and `text_error` (why the countdown text is not
showing, or `null`).

None of these commands changes the scene collection or adds an undo step.

### `dock.layout_save`, `dock.layout_apply` and `dock.layout_remove`

`dock.layout_save` takes `{"name"}` and saves every dock setting under that name, replacing a
layout whose name differs only in case; it returns `{"saved", "replaced"}`. Without `name` it
opens the Save Dock Layout prompt and returns `{"asking": true}`. Saving a 33rd layout is an
error.

`dock.layout_apply` and `dock.layout_remove` take `{"name"}`, compared without regard to case, and
return `{"applied"}` or `{"removed"}`; a name no layout has is an error.

### `view.streaming` and the `stream.destination` commands

`view.streaming` opens the Streaming window, or closes it when it is open, and returns `{"on"}`.

`stream.destinations` takes no parameters and returns `{"destinations", "running", "max"}`.
`max` is 16. Each destination is `{"id", "name", "enabled", "service", "server", "key", "status"}`,
where `service` is the id of the streaming service it is set up for or empty for a custom server,
`key` is `profile`, `held` or `none` and `server` shows a user name and password before the host, a
key at the end of the address, and any query, as dots. A destination whose key is held adds `held_name` and `held`, whether a key is held
under that name now. `status` is `idle` while no stream runs, `not_in_stream` for a destination
added after the stream started, and otherwise `connecting`, `live`, `retrying`, `stopped` or
`failed`. While the stream runs a destination also has `bytes`, `dropped` and `reconnects`; a
`retrying` one has `attempt` and `error`, and a `failed` one has `error`.

`stream.destination_add` takes `{"service"?, "name"?, "server"?, "enabled"?, "key"?, "held_name"?}`
and returns `{"id"}`. `service` is the id of one of the seven streaming services, or empty for a
custom server; the error for an unknown `service` lists the ids. A service that documents one ingest
server for every account fills in that server when `server` is left out. The first destination of a
profile gets the id `main` and the profile's key; later ones get `destination-2`, `destination-3`
and so on, and no key of their own unless `key` says otherwise. `enabled` is `true` when it is left
out.

`stream.destination_set` takes `{"id", "service"?, "name"?, "server"?, "enabled"?, "key"?,
"held_name"?}` and changes only the fields given; it returns `{"id", "changed"}`, and a call that
changes nothing does not save the profile. A new `service` replaces a server that is empty or that
is one of the services' documented servers: with that service's server, or, for a service that
shows each account its own server, with no server, and the destination is turned off. Any other
server is kept, also on a change to a custom server. A `server` or `enabled` given in the same call is used instead, and a destination
given a server that is not empty keeps its on or off setting. A destination keeps a
`service` this version does not know until a call changes it. With `key` set to `held`,
`held_name` names the session key; without it the destination keeps its held name, or uses its id.

A command does not send the profile's stream key, or a key held for the session, to a server the
user did not send it to. Only the destination `main` takes `key` `profile` from a command. A
command's `held_name` must be the name the destination already uses, so a command cannot give a
destination the session key of another one. A command that would leave a destination with the
profile's key or a held key and a server other than the one it had with that key is refused: give
`key` `none` with the new server. The Streaming window's own Add Destination and Save make these
changes.

`stream.destination_remove` takes `{"id"}` and returns `{"removed"}`. A destination that is sending
or trying to is not removed until it is stopped.

`stream.destination_start` and `stream.destination_stop` take `{"id"}` and start or stop sending to
that destination of the running stream; the others go on. They return `{"id", "on"}`.

These are errors: a missing or unknown `id`, a 17th destination, a `name` over 128 characters, a
`server` over 4096 bytes, a `name` or `server` with a control character, a `server` with a user
name or password before the host (`rtmp://user:pass@host/app`), a `key` other than
`profile`, `held` or `none`, `held_name` without `key` `held` or outside 1 to 64 bytes, a `service`
that is not empty and is not a streaming service id, a parameter
of the wrong type, a start or stop while no stream runs or while the running stream was started with
another profile, and a start of a destination whose held key is not set. While a stream of another
profile runs, each destination's `status` is `not_in_stream`. Each change is saved in the profile, and an open Settings window shows the new
destinations. Keys held for the session are set in the Streaming window only and are not command
parameters.

### `video.displays` and `video.canvas_from_display`

`video.displays` takes no parameters and returns `{"displays", "window"}`. `displays` lists each
display the system reports as `{"id", "name", "width", "height", "primary"}`, with sizes in pixels;
on Linux the list holds Wayland's outputs, where no display is `primary`, or RandR's monitors
under X11. The app reads the Linux list in the background when it starts and again after each
call, and a call returns the list read before it, so a display added or removed shows from the
second call after the change. `window` is `{"width", "height"}` of the display the window is on,
or `null` when the windowing system does not report it.

`video.canvas_from_display` takes `{"display"?}` and sets the canvas size to that display's size
in pixels. Without `display` it uses the display the window is on. Each side is rounded down to an
even number. An output size of 100%, 75%, 66% (two thirds) or 50% of the old canvas becomes the
same share of the new canvas, rounded down to even sides; any other output size is not changed. The
scenes are laid out for the new size as `canvas.relayout` describes. The profile is saved, also
when the canvas already had that size. An open Settings window shows the new canvas and output
sizes. It returns `{"width", "height", "output": {"width", "height"}, "changed", "follows"}`. These
are errors: a `display` that is not a string (`null` included), an id that `video.displays` does not
list, a display with a side under 16 or over 8192 pixels, an unknown size for the window's display,
a change while the recording, the stream or the replay buffer runs, and a profile that can't be
saved. After a failed save the old canvas size, output size and layout come back, and the undo
history holds no step for the layout change.

Afterward the canvas follows the display: while the same profile is in use and its canvas size has
not been set another way, the canvas takes the display's new size when its resolution or scaling
changes. The window lists the displays every 2 seconds, and at once when the display the window is
on reports a new size. A display that is not connected is followed again when it is; when its id
has changed, it is found by its name if no other listed display has that name. A new size that is
outside the canvas's range is not taken. While the recording, the stream or the replay buffer runs
the canvas keeps its size, and it takes the display's size after they stop. When the profile can't
be saved, the new size is taken back, its error is shown once, and each look tries the size again.
`follows` is the id of the followed display, or `null` when the canvas follows none: without
`display`, the display the window is on is followed only when it is the only listed display of its
size.

### `canvas.relayout`

`canvas.relayout` takes `{"from": {"width", "height"}, "to": {"width", "height"}, "sizes"?,
"items"?}` and places every item of every scene, locked and hidden items included, for a canvas
that changed from `from` to `to`. `sizes` is a list of `{"source", "width", "height"}` with the
size each source shows at. `items` is a list of item ids; when it is given, only those items are
placed. The app runs it, as one undo step, whenever the canvas size changes: from Settings ›
Video, from `video.canvas_from_display`, or by switching to a profile with another canvas size.

- An item with a fit mode is placed by that mode on the new canvas: `fit` shows the whole source
  inside the canvas, `fill` covers the canvas, and `stretch` makes the source the canvas's size.
  The crop is kept and the rotation becomes 0. Fit to Screen, Fill Screen and Stretch to Screen
  set the mode, and so does `item.transform` with `fit`. An `item.transform` without `fit` clears
  it.
- Any other item keeps its center at the same place in proportion to the canvas. Its size is
  multiplied by the square root of the product of the width and height factors, so changing the
  canvas back gives the old layout.
- An item whose source is not in `sizes` keeps its top-left corner at the same place in proportion
  instead, with the same size factor. When the app runs the command, the sources that show
  nothing at that moment are left out of `sizes`. An item of such a source that has a fit mode is
  placed by its mode once the source shows a picture, as its own undo step, while it still has
  its mode and the place the canvas change gave it.
- With `from` equal to `to`, only items with a fit mode whose source is in `sizes` can move.

These are errors: a side of `from` or `to` under 16 or over 8192 pixels, a `sizes` entry for a
source the collection does not have, a source listed twice, a source side under 1 or over 65536
pixels, an `items` entry for an item the collection does not have, and an item listed twice.

## Command line

`studiocraft-cli send <port> <method> [<params as JSON>]` sends one request to the app and prints
the reply. Start the app as [Connecting](#connecting) shows, then send requests from a shell that
has the same token:

```sh
studiocraft-cli send 9876 document.inspect
studiocraft-cli send 9876 engine.execute '{"command": "scene.add", "params": {"name": "Intro"}}'
```

`send` takes the method names listed in [Methods](#methods). A command line it can't read (a
missing port or method, a port outside 1 to 65535, parameters that are not JSON, more than one
parameters argument) exits with code 2. A request that fails, or a missing or invalid
`STUDIOCRAFT_CONTROL_TOKEN`, exits with code 1.

## MCP

The MCP server sends its tool calls to the app through this channel. [mcp.md](mcp.md) describes
how to register it and lists its tools and resources.
