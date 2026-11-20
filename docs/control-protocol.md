# Control protocol

`studiocraft --control <port>` accepts control requests on `127.0.0.1:<port>`. Each request and
each reply is one line of JSON:

```json
{"id": 1, "token": "<token>", "method": "run", "params": {"command": "scene.add", "params": {}}}
{"id": 1, "ok": true, "result": {"scene": 7}}
{"id": 2, "ok": false, "error": "unknown command `scene.fly`"}
```

Every request must carry the session token from the `STUDIOCRAFT_CONTROL_TOKEN` environment
variable the app was started with (64 hexadecimal characters). The app does not start when
`--control` is given without a valid token.

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
| `ui.inspect` | none | The window's layout and open dialogs (see below). |
| `ui.menu.list` | none | The menu bar as the window shows it, as a tree. |
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
`source.sync_offset` takes `{"source", "ms"}` and delays the source's audio by `ms`
milliseconds, a whole number from 0 to 5000; a source without audio is an error.
`source.monitor` takes `{"source", "mode"}`, where `mode` is `off`, `monitor_only` or
`monitor_and_output`; any other value, or a source without audio, is an error. `monitor_only`
plays the source on the monitoring device and leaves it out of recordings and streams.
`document.inspect` reports `monitoring` with the profile's `device`, whether it is `playing`, and
the device's `rate` and `channels` while it plays.
`source.set_record_track` takes `{"source", "on"}`, where `on` is `true` or `false`, and sets
whether audio tracks write the source to a WAV file of its own; a source without audio is an
error, and Edit › Undo takes it back. Setting the value a source already has from its monitoring
setting changes nothing and adds no undo step. Until it is set, a source whose monitoring setting
sends it to the program mix (`off` or `monitor_and_output`) is written and a monitor-only source is
not. A take writes at most 64 tracks at once, the program mix included; sources after that, in the
collection's order, are not written. Counting sources that left or were replaced during it, one
take makes at most 256 tracks, each split into parts by the file size limit; a source after that is
not written. `audio_tracks.status` takes no params and returns the audio tracks settings, the
output the running take follows (`anchor`, `recording` or `stream`), whether a repair runs
(`repairing`), what the last repair did (`last_repair`), and the last take's state, as
`document.inspect`'s `audio_tracks` entry does. `audio_tracks.repair` takes `{"folder"}` and
repairs the takes in it that ended without finishing their files, on a background thread; it
returns `{"started": true, "folder"}` at once. When the repair ends, `last_repair` holds
`{"repaired": [{"folder", "files": [{"file", "seconds"}]}], "failed": [{"folder", "error"}]}`,
where `failed` lists each take that could not be repaired, such as one whose journal does not read.
An empty `folder`, one that does not exist and a fifth repair while four run are errors. A take that is still being written, or that another repair holds, is left unchanged.
Each repaired take's entry also has `session`, the session file the repair wrote, or `null` for a
take that did not make its own folder.
`audio_tracks.open_last` takes no params and opens the last take's session file in the audio
editor on a background thread, or the take's folder when the editor cannot be found or started; it
returns `{"started": true, "session"}` at once, and a notice reports what happened. The last session
is the last one a take wrote when it ended or a repair wrote, and `audio_tracks.status` returns it
as `last_session`. No session yet and a fifth opening while four run are errors.

`dock.layout_save` takes `{"name"}` and saves every dock setting under that name, replacing a
layout whose name differs only in case; it returns `{"saved", "replaced"}`. Without `name` it
opens the Save Dock Layout prompt and returns `{"asking": true}`. Saving a 33rd layout is an
error. `dock.layout_apply` and `dock.layout_remove` take `{"name"}`, compared without regard to
case, and return `{"applied"}` or `{"removed"}`; a name no layout has is an error.

Each entry of `engine.commands` has `id`, `label`, `params` (a short description of the
parameters), `undo` (whether Edit › Undo takes the command back), `scope` (`engine` or `window`),
`enabled` (whether the command can run now, as its menu item shows) and `shortcut` (the first
shortcut, such as `Cmd+Z`, or `null`).

### Parameters and limits

Every request's `params` is an object or left out. A parameter of the wrong type, or a value out
of range, is an error; the app does not fall back to a default for it. Defaults apply only to
parameters that are left out.

- Coordinates are window points, with (0, 0) at the window's top left. A point outside the window
  is an error. `ui.inspect` reports the window's size.
- `button` is `primary` (the default), `secondary` or `middle`. `count` is 1 to 3 presses
  (default 1).
- `modifiers` is a list of `cmd`, `ctrl`, `alt` and `shift`. `cmd` is Command on macOS and Ctrl
  on Windows and Linux.
- `steps` is 1 to 200 moves (default 12). `ui.drag` drags with the primary button.
- `key` is a shortcut as Settings › Hotkeys writes it: `Cmd+Z`, `Shift+F11`, `Delete`. A key
  pressed twice is written twice: `A A`.
- `text` is at most 10,000 characters and goes to the field that has the keyboard focus.
- `max_side` is the longest side of the returned picture in pixels; 0 (the default) is full size.
  Pictures are never larger than 4,096 pixels on a side.
- `path` names a menu and the labels inside it, separated by `>`. Three dots match an ellipsis
  when no label in that menu is written exactly as given.
  An unknown path, a submenu and a disabled item are errors.

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
| `docks` | Each shown dock in the strip (`scenes`, `sources`, `mixer`) from left to right, with its `rect`: `{"x", "y", "width", "height"}`; its `header` rect; its `title` rect, where a right-click opens the dock's menu; `title_truncated`, whether the title was shortened to fit; and `actions`, each header action with its `name` (`add`, `remove`, `up`, `down`, `properties`) and `rect`, from left to right. |
| `floating_docks` | Each shown dock in a window of its own, with the same fields as `docks`; its `rect` is the window's. |
| `dock_layouts` | The names of the saved dock layouts, in the order they were first saved. |
| `dock_strip` | The strip that holds the docks, or `null` when every dock is hidden: its `rect` and `height`, `saved_height` (the height set by dragging its top edge, or `null`) and `saved_widths` (`scenes`, `sources` and `mixer`, each a width set by dragging a divider, or `null`) and `order`, every dock's name from left to right, hidden ones included. |
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

Two engine commands only read: `app.info` returns the app's name, id, version, license and
`contributors` (the number of people the About window lists from `contributors/`), and
`app.links` returns the community links (`discord`, `website`, `github`, `issues`). With
`{"link": "github"}` it returns `{"url": "..."}` for that one link; an unknown key is an error.
Neither command opens a browser or changes the scene collection. The window's `help.discord`,
`help.website`, `help.github` and `help.issues` commands open those links in the browser.

The window's `app.check_updates` command (Help › Check for Updates…) asks the release list for a
newer version and shows the answer in the window; it replies `{"checking": true}`. A release list
that cannot be reached is reported as an error, never as "up to date". Sessions started with
`--no-prefs`, `--screenshot` or `--self-test` never check, and the command fails there with
"update checks are off in this session".

The engine command `plugin.list` also only reads. It returns every registered plug-in, grouped as
`sources`, `filters`, `transitions`, `encoders` and `outputs`. Each entry has `id`, `name`,
`version` and `schema` (the settings fields with their types, limits and defaults), and sources add
`video` and `audio`, filters `target`, transitions `instant`, and encoders `kind`, `codec` (`h264`,
`hevc` or `aac`) and `builtin`. `builtin` is `false` for an encoder that runs inside the ffmpeg program,
and only such an encoder has `ffmpeg_codec`.

The engine command `source.schema` also only reads. With `{"kind": "camera", "settings":
{"device": "..."}}` it returns `{"schema": ...}`, the source's settings fields with the lists the
system offers now: for a camera, the connected cameras and the formats of the camera that
`settings.device` names. `settings` may be left out. `source.add` and `source.settings` check new
values against the schema for the settings they are given, so a camera format must be one the
chosen camera offers. A camera's `device` is its name and the system's id joined by U+001F, and its
`format` is written as `1280x720@30:MJPG` (width, height, frames per second and a four-character
code: `MJPG`, `YUYV`, `NV12`, `GREY`, `RGB3` or `BGR3`), or `""` for the camera's default.

```sh
studiocraft-cli send 9876 engine.execute '{"command": "app.links", "params": {"link": "issues"}}'
```

## Command line

```sh
export STUDIOCRAFT_CONTROL_TOKEN=$(openssl rand -hex 32)
studiocraft --control 9876 &
studiocraft-cli send 9876 document.inspect
studiocraft-cli send 9876 engine.execute '{"command": "scene.add", "params": {"name": "Intro"}}'
```

`send` takes the method names in the table above. A command line it can't read (a missing port or
method, a port outside 1 to 65535, parameters that are not JSON) exits with code 2; a request that
fails exits with code 1.

## MCP

The MCP server sends its tool calls to the app through this channel. [mcp.md](mcp.md) describes
how to register it and lists its tools and resources.
