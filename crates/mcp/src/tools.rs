//! The MCP tools. Each one turns into control methods on the backend.

use base64::Engine as _;
use craft_jsonrpc::RpcError;
use craft_mcp::text_tool_result;
use serde_json::{Map, Value, json};

use crate::backend::{Backend, McpError};
use crate::remote::TOKEN_ENV;

/// Most steps one `command_batch` runs.
pub const MAX_BATCH_STEPS: usize = 1000;
/// `render_preview`'s longest side when `max_side` is left out.
const PREVIEW_SIDE: u64 = 960;
/// `ui_screenshot`'s longest side when `max_side` is left out.
const SCREENSHOT_SIDE: u64 = 1600;
/// Largest `max_side`.
const MAX_SIDE: u64 = 4096;
/// Most moves in one `ui_drag`.
const MAX_DRAG_STEPS: u64 = 200;
/// Most presses in one `ui_click`.
const MAX_CLICKS: u64 = 3;
/// Longest text one `ui_type` sends, in characters.
const MAX_TYPE_CHARS: usize = 10_000;
/// Mouse buttons `ui_click` presses.
const BUTTONS: [&str; 3] = ["primary", "secondary", "middle"];
/// The buttons `ui_dialog` presses.
const DIALOG_ANSWERS: [&str; 2] = ["ok", "cancel"];
/// Modifier keys held during a click or drag.
const MODIFIERS: [&str; 4] = ["cmd", "ctrl", "alt", "shift"];
/// The first bytes of every PNG file.
const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];

type Args = Map<String, Value>;

/// How a tool treats the studio, for the MCP client.
#[derive(Clone, Copy)]
struct Hints {
    read_only: bool,
    destructive: bool,
    idempotent: bool,
    /// The tool reaches past this computer (a stream goes out to a server).
    open_world: bool,
}

const READS: Hints = Hints { read_only: true, destructive: false, idempotent: true, open_world: false };
const ADDS: Hints = Hints { read_only: false, destructive: false, idempotent: false, open_world: false };
const SETS: Hints = Hints { read_only: false, destructive: false, idempotent: true, open_world: false };
const REPLACES: Hints = Hints { read_only: false, destructive: true, idempotent: true, open_world: false };
const RUNS: Hints = Hints { read_only: false, destructive: true, idempotent: false, open_world: false };
/// Starting or stopping the recording: a repeat changes nothing.
const RECORDS: Hints = Hints { read_only: false, destructive: false, idempotent: true, open_world: false };
/// Starting or stopping the stream, which goes out to the configured server.
const PUBLISHES: Hints = Hints { read_only: false, destructive: false, idempotent: true, open_world: true };

fn tool(name: &str, title: &str, description: &str, properties: Value, required: &[&str], hints: Hints) -> Value {
    json!({
        "name": name,
        "title": title,
        "description": description,
        "inputSchema": {"type": "object", "properties": properties, "required": required, "additionalProperties": false},
        "annotations": {
            "title": title,
            "readOnlyHint": hints.read_only,
            "destructiveHint": hints.destructive,
            "idempotentHint": hints.idempotent,
            "openWorldHint": hints.open_world,
        },
    })
}

fn scene_ref(what: &str) -> Value {
    json!({"type": ["integer", "string"], "description": format!("The {what} scene's id or name.")})
}

fn transform_schema() -> Value {
    json!({
        "type": "object",
        "description": "position {x, y} of the top-left corner in canvas pixels, scale {x, y} (1 is the source's own size), rotation in \
                        degrees, crop {left, top, right, bottom} in source pixels. Fields left out take their defaults.",
    })
}

fn point_schema(what: &str) -> Value {
    json!({
        "type": "object",
        "description": format!("The {what} point in window points, (0, 0) at the window's top left."),
        "properties": {"x": {"type": "number", "minimum": 0}, "y": {"type": "number", "minimum": 0}},
        "required": ["x", "y"],
        "additionalProperties": false,
    })
}

fn modifiers_schema() -> Value {
    json!({
        "type": "array",
        "items": {"type": "string", "enum": MODIFIERS},
        "description": "Keys held down: cmd (Command on macOS, Ctrl elsewhere), ctrl, alt and shift.",
    })
}

/// Every tool with its title, input schema and annotations. The `ui_*` tools and `app_quit` are
/// listed only when `has_ui` is true, since they need the app's window.
pub fn tool_definitions(has_ui: bool) -> Vec<Value> {
    let mut tools = studio_tool_definitions();
    if has_ui {
        tools.extend(ui_tool_definitions());
    }
    tools
}

/// The tools that work on every backend.
fn studio_tool_definitions() -> Vec<Value> {
    vec![
        tool(
            "command_list",
            "List commands",
            "Every command StudioCraft can run, with its id, label, parameters, whether undo takes it back, and whether it can run now.",
            json!({
                "filter": {"type": "string", "description": "Keep commands whose id or label contains this text (case is ignored)."},
                "enabled_only": {"type": "boolean", "description": "Keep only commands that can run now."},
            }),
            &[],
            READS,
        ),
        tool(
            "command_run",
            "Run a command",
            "Run one command by id, as a menu item or shortcut does: for example scene.add, source.add, item.transform, record.toggle or \
             studio.transition. Returns the command's result.",
            json!({
                "id": {"type": "string", "description": "The command id, from command_list."},
                "params": {"type": "object", "description": "The command's parameters."},
            }),
            &["id"],
            RUNS,
        ),
        tool(
            "command_batch",
            "Run several commands",
            "Run commands in order and report each step's result or error. Stops at the first failed step unless stop_on_error is false. \
             At most 1000 steps.",
            json!({
                "steps": {
                    "type": "array",
                    "maxItems": MAX_BATCH_STEPS,
                    "items": {
                        "type": "object",
                        "properties": {"id": {"type": "string"}, "params": {"type": "object"}},
                        "required": ["id"],
                        "additionalProperties": false,
                    },
                },
                "stop_on_error": {"type": "boolean", "description": "Stop at the first failed step (default true)."},
            }),
            &["steps"],
            RUNS,
        ),
        tool(
            "doc_inspect",
            "Inspect the scene collection",
            "The scene collection (scenes, sources, scene items, transition), studio mode, the program and preview scenes, recording and \
             stream state, frame statistics and the canvas size. `summary` (the default) leaves out source, filter and transition settings.",
            json!({"detail": {"type": "string", "enum": ["summary", "full"]}}),
            &[],
            READS,
        ),
        tool(
            "render_preview",
            "Render a preview",
            "A PNG picture of the program, or of the preview scene in studio mode.",
            json!({
                "view": {"type": "string", "enum": ["program", "preview"], "description": "Which view to draw (default program)."},
                "max_side": {"type": "integer", "minimum": 0, "maximum": MAX_SIDE, "description": "Longest side in pixels (default 960; 0 is full size)."},
            }),
            &[],
            READS,
        ),
        tool("undo", "Undo", "Take back the last change to the scene collection.", json!({}), &[], ADDS),
        tool("redo", "Redo", "Make the last undone change again.", json!({}), &[], ADDS),
        tool(
            "scene_add",
            "Add a scene",
            "Add a scene. Returns its id.",
            json!({"name": {"type": "string", "description": "The scene's name (default: the next free \"Scene N\")."}}),
            &[],
            ADDS,
        ),
        tool(
            "scene_select",
            "Show a scene",
            "Show a scene in the program (in studio mode, in the preview).",
            json!({"scene": scene_ref("shown")}),
            &["scene"],
            SETS,
        ),
        tool(
            "source_add",
            "Add a source",
            "Create a source of a plug-in kind (for example color, image, text, media, display_capture, audio_input) and place it in a scene. \
             Returns the source and item ids.",
            json!({
                "kind": {"type": "string", "description": "The source plug-in id."},
                "scene": scene_ref("target (default: the scene being edited)"),
                "name": {"type": "string"},
                "settings": {"type": "object", "description": "The plug-in's settings."},
                "transform": transform_schema(),
            }),
            &["kind"],
            ADDS,
        ),
        tool(
            "item_transform",
            "Transform a scene item",
            "Set a scene item's position, scale, rotation and crop.",
            json!({
                "item": {"type": ["integer", "string"], "description": "The item's id, or the name of the source it shows."},
                "scene": scene_ref("item's (default: the scene being edited)"),
                "transform": transform_schema(),
            }),
            &["item", "transform"],
            SETS,
        ),
        tool(
            "source_audio",
            "Set a source's audio",
            "Set how a source's audio is mixed: `sync_offset_ms` delays it to line it up with its picture. Returns the source's audio \
             settings. A value out of range is an error and changes nothing.",
            json!({
                "source": {"type": ["integer", "string"], "description": "The source's id or name."},
                "sync_offset_ms": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": studiocraft_model::collection::MAX_SYNC_OFFSET_MS,
                    "description": "How long the audio is delayed, in milliseconds.",
                },
            }),
            &["source"],
            SETS,
        ),
        tool(
            "transition_set",
            "Set the transition",
            "Set the scene transition (for example fade, cut, swipe) and, optionally, its length.",
            json!({
                "kind": {"type": "string", "description": "The transition plug-in id."},
                "ms": {"type": "integer", "minimum": 0, "description": "Length in milliseconds."},
            }),
            &["kind"],
            SETS,
        ),
        tool(
            "studio_transition",
            "Transition to program",
            "In studio mode, move the preview scene to the program with the transition, or with the given one.",
            json!({
                "kind": {"type": "string", "description": "A transition plug-in id for this switch only."},
                "ms": {"type": "integer", "minimum": 0, "description": "Length in milliseconds for this switch only."},
            }),
            &[],
            ADDS,
        ),
        tool(
            "collection_open",
            "Open a scene collection file",
            "Replace the scene collection with the one in a file. The undo history starts empty.",
            json!({"path": {"type": "string", "description": "The file's path."}}),
            &["path"],
            REPLACES,
        ),
        tool(
            "collection_save",
            "Save the scene collection to a file",
            "Write the scene collection to a file, replacing any file at that path.",
            json!({"path": {"type": "string", "description": "The file's path."}}),
            &["path"],
            REPLACES,
        ),
        tool("app_links", "Community links", "StudioCraft's community, website, source and issue links.", json!({}), &[], READS),
        tool(
            "record_start",
            "Start recording",
            "Start recording the program to a new file in the recording folder (Settings › Output), as Start Recording does. Returns the \
             recording's state. Nothing happens when a recording already runs.",
            json!({}),
            &[],
            RECORDS,
        ),
        tool(
            "record_stop",
            "Stop recording",
            "Stop the recording and finish its file, as Stop Recording does. Returns the recording's state.",
            json!({}),
            &[],
            RECORDS,
        ),
        tool(
            "stream_start",
            "Start streaming",
            "Start streaming the program to the RTMP server in Settings › Stream, as Start Streaming does. Returns the stream's state. The \
             in-process server starts a stream only when it was started with --allow-streaming.",
            json!({}),
            &[],
            PUBLISHES,
        ),
        tool("stream_stop", "Stop streaming", "Stop the stream, as Stop Streaming does. Returns the stream's state.", json!({}), &[], PUBLISHES),
        tool(
            "replay_start",
            "Start the replay buffer",
            "Start keeping the last seconds of the program in memory (length and memory limit in Settings › Output), as Start Replay \
             Buffer does. It runs the built-in encoders. Returns the replay buffer's state. Nothing happens when it already runs.",
            json!({}),
            &[],
            RECORDS,
        ),
        tool(
            "replay_stop",
            "Stop the replay buffer",
            "Stop the replay buffer and drop what it holds, as Stop Replay Buffer does. Replays being saved are still written. Returns the \
             replay buffer's state.",
            json!({}),
            &[],
            RECORDS,
        ),
        tool(
            "replay_save",
            "Save a replay",
            "Write what the replay buffer holds to a new MKV file in the recording folder, as Save Replay does; the buffer keeps running. \
             The file is written in the background, and output_status counts the saves still being written. An error when the replay \
             buffer is not running or holds nothing yet.",
            json!({}),
            &[],
            ADDS,
        ),
        tool(
            "output_status",
            "Recording, stream and replay buffer state",
            "Whether the recording, the stream and the replay buffer run, with each one's status, file or server, and seconds since it \
             started; for the replay buffer also the bytes it holds and the saves still being written.",
            json!({}),
            &[],
            READS,
        ),
    ]
}

/// The tools that need the app's window.
fn ui_tool_definitions() -> Vec<Value> {
    vec![
        tool(
            "ui_inspect",
            "Inspect the window",
            "The window's size, open dialogs, docks with their rectangles, the preview's rectangle and scale, studio mode, menu titles, \
             the selected item, status bar messages and whether queued input is still waiting.",
            json!({}),
            &[],
            READS,
        ),
        tool(
            "ui_screenshot",
            "Screenshot the window",
            "A PNG picture of the whole window, taken after its next frame.",
            json!({
                "max_side": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": MAX_SIDE,
                    "description": "Longest side in pixels (default 1600; 0 is full size).",
                },
            }),
            &[],
            READS,
        ),
        tool(
            "ui_click",
            "Click in the window",
            "Click at a point in the window. The click is queued and the window carries it out over its next frames.",
            json!({
                "x": {"type": "number", "minimum": 0, "description": "Window points from the left edge."},
                "y": {"type": "number", "minimum": 0, "description": "Window points from the top edge."},
                "button": {"type": "string", "enum": BUTTONS, "description": "The mouse button (default primary)."},
                "count": {"type": "integer", "minimum": 1, "maximum": MAX_CLICKS, "description": "Presses: 2 is a double click (default 1)."},
                "modifiers": modifiers_schema(),
            }),
            &["x", "y"],
            RUNS,
        ),
        tool(
            "ui_drag",
            "Drag in the window",
            "Drag with the primary button from one point to another, one move per frame.",
            json!({
                "from": point_schema("starting"),
                "to": point_schema("ending"),
                "steps": {"type": "integer", "minimum": 1, "maximum": MAX_DRAG_STEPS, "description": "Moves between the points (default 12)."},
                "modifiers": modifiers_schema(),
            }),
            &["from", "to"],
            RUNS,
        ),
        tool(
            "ui_key",
            "Press a shortcut",
            "Press a key or shortcut as Settings > Hotkeys writes it, for example Cmd+Z, Shift+F11 or Delete.",
            json!({"key": {"type": "string", "description": "The shortcut. A key pressed twice is written twice: `A A`."}}),
            &["key"],
            RUNS,
        ),
        tool(
            "ui_type",
            "Type text",
            "Type text into the field that has the keyboard focus. At most 10,000 characters.",
            json!({"text": {"type": "string", "description": "The text to type."}}),
            &["text"],
            RUNS,
        ),
        tool(
            "ui_menu_list",
            "List the menus",
            "The menu bar as the window shows it: each menu and item with its label, path, command id, whether it is enabled or checked, \
             and its shortcut.",
            json!({}),
            &[],
            READS,
        ),
        tool(
            "ui_menu_invoke",
            "Choose a menu item",
            "Choose a menu item by its path, for example `Scene Collection > New...`, and return the command it ran and the result. An \
             unknown path, a submenu or a disabled item is an error.",
            json!({"path": {"type": "string", "description": "Menu labels separated by `>`; three dots match an ellipsis."}}),
            &["path"],
            RUNS,
        ),
        tool(
            "ui_dialog",
            "Answer a dialog",
            "Answer the open name prompt (New Scene Collection, Rename Profile and the others) or the exit confirmation as its buttons \
             do. `ok` presses OK, or Stop and Exit; `cancel` presses Cancel. `text` replaces the name in a name prompt first. A name the \
             prompt refuses is an error and the prompt stays open. ui_inspect's `dialogs` shows which one is open.",
            json!({
                "answer": {"type": "string", "enum": DIALOG_ANSWERS, "description": "The button to press."},
                "text": {"type": "string", "description": "The name to enter in a name prompt."},
            }),
            &["answer"],
            RUNS,
        ),
        tool(
            "app_quit",
            "Quit the app",
            "Close the window as File > Exit does. While a recording, stream or replay buffer runs, the window asks first and the result's \
             `confirming` is true.",
            json!({}),
            &[],
            RUNS,
        ),
    ]
}

fn invalid(message: impl Into<String>) -> RpcError {
    RpcError::new(-32602, message)
}

/// Unknown tools and argument keys are protocol errors.
fn check_arguments(name: &str, args: &Args) -> Result<(), RpcError> {
    let defs = tool_definitions(true);
    let Some(def) = defs.iter().find(|t| t.get("name").and_then(Value::as_str) == Some(name)) else {
        return Err(invalid(format!("there is no tool `{name}`")));
    };
    let props = def.pointer("/inputSchema/properties").and_then(Value::as_object);
    match args.keys().find(|k| !props.is_some_and(|p| p.contains_key(*k))) {
        Some(key) => Err(invalid(format!("unknown argument `{key}` for `{name}`"))),
        None => Ok(()),
    }
}

fn opt_object(args: &Args, key: &str) -> Result<Value, McpError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(json!({})),
        Some(v @ Value::Object(_)) => Ok(v.clone()),
        Some(_) => Err(McpError::Invalid(format!("`{key}` must be an object"))),
    }
}

fn req_str<'a>(args: &'a Args, key: &str) -> Result<&'a str, McpError> {
    args.get(key).and_then(Value::as_str).ok_or_else(|| McpError::Invalid(format!("`{key}` must be a string")))
}

fn opt_bool(args: &Args, key: &str, default: bool) -> Result<bool, McpError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v.as_bool().ok_or_else(|| McpError::Invalid(format!("`{key}` must be true or false"))),
    }
}

/// The given keys of `args`, as a command's parameters.
fn pick(args: &Args, keys: &[&str]) -> Value {
    Value::Object(keys.iter().filter_map(|k| args.get(*k).filter(|v| !v.is_null()).map(|v| ((*k).to_owned(), v.clone()))).collect())
}

fn run(b: &mut dyn Backend, id: &str, params: Value) -> Result<Value, McpError> {
    b.call("engine.execute", json!({"command": id, "params": params}))
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
}

fn text(v: &Value) -> Value {
    text_tool_result(&pretty(v), false)
}

fn command_list(b: &mut dyn Backend, args: &Args) -> Result<Value, McpError> {
    let enabled_only = opt_bool(args, "enabled_only", false)?;
    let filter = match args.get("filter") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return Err(McpError::Invalid("`filter` must be a string".into())),
    };
    let mut list = b.call("engine.commands", json!({"filter": filter}))?;
    if enabled_only && let Some(a) = list.as_array_mut() {
        a.retain(|c| c.get("enabled").and_then(Value::as_bool) == Some(true));
    }
    Ok(text(&list))
}

fn command_batch(b: &mut dyn Backend, args: &Args) -> Result<Value, McpError> {
    let steps = args.get("steps").and_then(Value::as_array).ok_or_else(|| McpError::Invalid("`steps` must be a list".into()))?;
    if steps.len() > MAX_BATCH_STEPS {
        return Err(McpError::Invalid(format!("`steps` holds {} steps; a batch runs at most {MAX_BATCH_STEPS}", steps.len())));
    }
    let stop = opt_bool(args, "stop_on_error", true)?;
    // Every step is checked before any runs.
    let mut plan = Vec::with_capacity(steps.len());
    for (i, s) in steps.iter().enumerate() {
        let Some(step) = s.as_object() else { return Err(McpError::Invalid(format!("step {i} must be an object with `id`"))) };
        if let Some(key) = step.keys().find(|k| !matches!(k.as_str(), "id" | "params")) {
            return Err(McpError::Invalid(format!("step {i} has an unknown key `{key}`; a step has `id` and `params`")));
        }
        let id = step.get("id").and_then(Value::as_str).ok_or_else(|| McpError::Invalid(format!("step {i} needs a string `id`")))?;
        let params = opt_object(step, "params").map_err(|e| McpError::Invalid(format!("step {i}: {e}")))?;
        plan.push((id, params));
    }
    let (mut failed, mut report) = (0usize, Vec::with_capacity(plan.len()));
    for (i, (id, params)) in plan.into_iter().enumerate() {
        match run(b, id, params) {
            Ok(result) => report.push(json!({"index": i, "id": id, "ok": true, "result": result})),
            Err(e) => {
                failed += 1;
                report.push(json!({"index": i, "id": id, "ok": false, "error": e.to_string()}));
                if stop {
                    break;
                }
            }
        }
    }
    let ran = report.len();
    let summary = json!({
        "completed": ran - failed,
        "failed": failed,
        "skipped": steps.len() - ran,
        "steps": report,
    });
    Ok(text_tool_result(&pretty(&summary), failed > 0))
}

fn render_preview(b: &mut dyn Backend, args: &Args) -> Result<Value, McpError> {
    let view = match args.get("view") {
        None | Some(Value::Null) => "program",
        Some(Value::String(v)) if v == "program" || v == "preview" => v,
        Some(_) => return Err(McpError::Invalid("`view` is program or preview".into())),
    };
    let side = match args.get("max_side") {
        None | Some(Value::Null) => PREVIEW_SIDE,
        Some(v) => v
            .as_u64()
            .filter(|n| *n <= MAX_SIDE)
            .ok_or_else(|| McpError::Invalid(format!("`max_side` must be a whole number from 0 to {MAX_SIDE}")))?,
    };
    let picture = b.call("render.preview", json!({"view": view, "max_side": side}))?;
    image_result(&picture, view)
}

/// A `{width, height, png_base64}` reply as an image result. A picture that is not base64 or not
/// a PNG is an error.
fn image_result(picture: &Value, label: &str) -> Result<Value, McpError> {
    let data = picture.get("png_base64").and_then(Value::as_str).ok_or_else(|| McpError::App("the picture is missing from the reply".into()))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|e| McpError::App(format!("the picture in the reply is not valid base64: {e}")))?;
    if !bytes.starts_with(&PNG_SIGNATURE) {
        return Err(McpError::App("the picture in the reply is not a PNG".into()));
    }
    let (w, h) = (picture.get("width").and_then(Value::as_u64).unwrap_or(0), picture.get("height").and_then(Value::as_u64).unwrap_or(0));
    Ok(json!({
        "content": [
            {"type": "image", "data": data, "mimeType": "image/png"},
            {"type": "text", "text": format!("{label}, {w} x {h} pixels")},
        ],
        "isError": false,
    }))
}

/// A window coordinate: a number of 0 or more.
fn coordinate(v: Option<&Value>, what: &str) -> Result<Value, RpcError> {
    match v {
        Some(n @ Value::Number(num)) if num.as_f64().is_some_and(|f| f >= 0.0) => Ok(n.clone()),
        _ => Err(invalid(format!("`{what}` must be a number of window points, 0 or more"))),
    }
}

/// A `{x, y}` point argument.
fn point(args: &Args, key: &str) -> Result<Value, RpcError> {
    let Some(Value::Object(p)) = args.get(key) else {
        return Err(invalid(format!("`{key}` must be an object {{x, y}}")));
    };
    if let Some(k) = p.keys().find(|k| !matches!(k.as_str(), "x" | "y")) {
        return Err(invalid(format!("`{key}` has an unknown key `{k}`; a point has `x` and `y`")));
    }
    Ok(json!({"x": coordinate(p.get("x"), &format!("{key}.x"))?, "y": coordinate(p.get("y"), &format!("{key}.y"))?}))
}

/// An optional whole number from `min` to `max`.
fn opt_whole(args: &Args, key: &str, min: u64, max: u64) -> Result<Option<u64>, RpcError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .filter(|n| (min..=max).contains(n))
            .map(Some)
            .ok_or_else(|| invalid(format!("`{key}` must be a whole number from {min} to {max}"))),
    }
}

/// A string argument that is not empty.
fn filled_str<'a>(args: &'a Args, key: &str) -> Result<&'a str, RpcError> {
    match args.get(key) {
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(s),
        Some(Value::String(_)) => Err(invalid(format!("`{key}` can't be empty"))),
        _ => Err(invalid(format!("`{key}` must be a string"))),
    }
}

/// The optional `button`, `count` and `modifiers` of a click or drag, added to `params`.
fn pointer_options(args: &Args, params: &mut Args) -> Result<(), RpcError> {
    match args.get("button") {
        None | Some(Value::Null) => {}
        Some(Value::String(b)) if BUTTONS.contains(&b.as_str()) => {
            params.insert("button".into(), json!(b));
        }
        Some(_) => return Err(invalid(format!("`button` is one of {}", BUTTONS.join(", ")))),
    }
    if let Some(count) = opt_whole(args, "count", 1, MAX_CLICKS)? {
        params.insert("count".into(), json!(count));
    }
    match args.get("modifiers") {
        None | Some(Value::Null) => {}
        Some(Value::Array(list)) if list.iter().all(|m| m.as_str().is_some_and(|m| MODIFIERS.contains(&m))) => {
            params.insert("modifiers".into(), Value::Array(list.clone()));
        }
        Some(_) => return Err(invalid(format!("`modifiers` is a list of {}", MODIFIERS.join(", ")))),
    }
    Ok(())
}

/// For a tool that needs the window: its control method and checked parameters. `None` for every
/// other tool.
fn ui_request(name: &str, args: &Args) -> Result<Option<(&'static str, Value)>, RpcError> {
    let request = match name {
        "ui_inspect" => ("ui.inspect", json!({})),
        "ui_menu_list" => ("ui.menu.list", json!({})),
        "app_quit" => ("app.quit", json!({})),
        "ui_screenshot" => ("ui.screenshot", json!({"max_side": opt_whole(args, "max_side", 0, MAX_SIDE)?.unwrap_or(SCREENSHOT_SIDE)})),
        "ui_click" => {
            let mut p = Map::new();
            p.insert("x".into(), coordinate(args.get("x"), "x")?);
            p.insert("y".into(), coordinate(args.get("y"), "y")?);
            pointer_options(args, &mut p)?;
            ("ui.click", Value::Object(p))
        }
        "ui_drag" => {
            let mut p = Map::new();
            p.insert("from".into(), point(args, "from")?);
            p.insert("to".into(), point(args, "to")?);
            if let Some(steps) = opt_whole(args, "steps", 1, MAX_DRAG_STEPS)? {
                p.insert("steps".into(), json!(steps));
            }
            pointer_options(args, &mut p)?;
            ("ui.drag", Value::Object(p))
        }
        "ui_key" => ("ui.key", json!({"key": filled_str(args, "key")?})),
        "ui_type" => {
            let text = match args.get("text") {
                Some(Value::String(t)) => t,
                _ => return Err(invalid("`text` must be a string")),
            };
            if text.is_empty() {
                return Err(invalid("`text` can't be empty"));
            }
            if text.chars().count() > MAX_TYPE_CHARS {
                return Err(invalid(format!("`text` holds more than {MAX_TYPE_CHARS} characters")));
            }
            ("ui.type", json!({"text": text}))
        }
        "ui_menu_invoke" => ("ui.menu.invoke", json!({"path": filled_str(args, "path")?})),
        "ui_dialog" => {
            let answer = match args.get("answer") {
                Some(Value::String(a)) if DIALOG_ANSWERS.contains(&a.as_str()) => a,
                _ => return Err(invalid(format!("`answer` is one of {}", DIALOG_ANSWERS.join(", ")))),
            };
            let mut p = Map::new();
            p.insert("answer".into(), json!(answer));
            match args.get("text") {
                None | Some(Value::Null) => {}
                Some(Value::String(t)) => {
                    p.insert("text".into(), json!(t));
                }
                Some(_) => return Err(invalid("`text` must be a string")),
            }
            ("ui.dialog", Value::Object(p))
        }
        _ => return Ok(None),
    };
    Ok(Some(request))
}

/// What a window tool answers on a backend without a window.
fn needs_desktop(name: &str) -> String {
    format!("`{name}` needs the desktop app: start `studiocraft --control PORT` with {TOKEN_ENV} set and run `studiocraft-cli mcp --connect PORT`")
}

/// Run a window tool's control method and shape its reply.
fn ui_call(b: &mut dyn Backend, method: &str, params: Value) -> Result<Value, McpError> {
    let reply = b.call(method, params)?;
    if method == "ui.screenshot" { image_result(&reply, "window") } else { Ok(text(&reply)) }
}

fn transition_set(b: &mut dyn Backend, args: &Args) -> Result<Value, McpError> {
    let kind = req_str(args, "kind")?;
    // The duration is checked first, so a bad one changes nothing.
    let max = studiocraft_model::collection::MAX_TRANSITION_MS;
    let ms = match args.get("ms").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => match v.as_u64().and_then(|ms| u32::try_from(ms).ok()) {
            Some(ms) if ms <= max => Some(ms),
            _ => return Err(McpError::Invalid(format!("`ms` must be a whole number from 0 to {max}"))),
        },
    };
    let mut result = json!({"kind": run(b, "transition.set", json!({"kind": kind}))?});
    if let Some(ms) = ms {
        result["ms"] = run(b, "transition.duration", json!({"ms": ms}))?;
    }
    Ok(text(&result))
}

/// Run the audio commands for the given keys, then report the source's `audio` from
/// `document.inspect`.
fn source_audio(b: &mut dyn Backend, args: &Args) -> Result<Value, McpError> {
    let source = match args.get("source") {
        Some(v @ (Value::Number(_) | Value::String(_))) => v.clone(),
        _ => return Err(McpError::Invalid("`source` must be a source's id or name".into())),
    };
    let max = studiocraft_model::collection::MAX_SYNC_OFFSET_MS;
    let offset = match args.get("sync_offset_ms").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => match v.as_u64() {
            Some(ms) if ms <= u64::from(max) => Some(ms),
            _ => return Err(McpError::Invalid(format!("`sync_offset_ms` must be a whole number from 0 to {max}"))),
        },
    };
    if let Some(ms) = offset {
        run(b, "source.sync_offset", json!({"source": source, "ms": ms}))?;
    }
    let state = b.call("document.inspect", json!({"detail": "summary"}))?;
    let sources = state.pointer("/collection/sources").and_then(Value::as_array).cloned().unwrap_or_default();
    let found = sources.iter().find(|s| s.get("id") == Some(&source) || s.get("name") == Some(&source));
    match found {
        Some(s) => Ok(text(&json!({"source": s.get("id"), "name": s.get("name"), "audio": s.get("audio")}))),
        None => Err(McpError::Invalid(format!("there is no source {source}"))),
    }
}

/// The recording's, the stream's and the replay buffer's state from `document.inspect`.
fn outputs(b: &mut dyn Backend) -> Result<Value, McpError> {
    let state = b.call("document.inspect", json!({"detail": "summary"}))?;
    let pick = |key: &str| state.get(key).cloned().unwrap_or_else(|| json!({"active": false}));
    Ok(json!({"recording": pick("recording"), "streaming": pick("streaming"), "replay": pick("replay")}))
}

/// Run an output command, then report the output's state.
fn output_command(b: &mut dyn Backend, id: &str, key: &str) -> Result<Value, McpError> {
    run(b, id, json!({}))?;
    let state = outputs(b)?;
    let mut reply = Map::new();
    reply.insert(key.to_owned(), state.get(key).cloned().unwrap_or(Value::Null));
    Ok(text(&Value::Object(reply)))
}

fn dispatch(b: &mut dyn Backend, name: &str, args: &Args) -> Result<Value, McpError> {
    match name {
        "command_list" => command_list(b, args),
        "command_run" => Ok(text(&run(b, req_str(args, "id")?, opt_object(args, "params")?)?)),
        "command_batch" => command_batch(b, args),
        "doc_inspect" => Ok(text(&b.call("document.inspect", pick(args, &["detail"]))?)),
        "render_preview" => render_preview(b, args),
        "undo" => Ok(text(&run(b, "edit.undo", json!({}))?)),
        "redo" => Ok(text(&run(b, "edit.redo", json!({}))?)),
        "scene_add" => Ok(text(&run(b, "scene.add", pick(args, &["name"]))?)),
        "scene_select" => Ok(text(&run(b, "scene.select", pick(args, &["scene"]))?)),
        "source_add" => {
            req_str(args, "kind")?;
            Ok(text(&run(b, "source.add", pick(args, &["kind", "scene", "name", "settings", "transform"]))?))
        }
        "item_transform" => Ok(text(&run(b, "item.transform", pick(args, &["item", "scene", "transform"]))?)),
        "source_audio" => source_audio(b, args),
        "transition_set" => transition_set(b, args),
        "studio_transition" => Ok(text(&run(b, "studio.transition", pick(args, &["kind", "ms"]))?)),
        "collection_open" => Ok(text(&b.call("collection.open", json!({"path": req_str(args, "path")?}))?)),
        "collection_save" => Ok(text(&b.call("collection.save", json!({"path": req_str(args, "path")?}))?)),
        "app_links" => Ok(text(&run(b, "app.links", json!({}))?)),
        "record_start" => output_command(b, "record.start", "recording"),
        "record_stop" => output_command(b, "record.stop", "recording"),
        "stream_start" => output_command(b, "stream.start", "streaming"),
        "stream_stop" => output_command(b, "stream.stop", "streaming"),
        "replay_start" => output_command(b, "replay.start", "replay"),
        "replay_stop" => output_command(b, "replay.stop", "replay"),
        "replay_save" => output_command(b, "replay.save", "replay"),
        "output_status" => Ok(text(&outputs(b)?)),
        other => Err(McpError::Invalid(format!("there is no tool `{other}`"))),
    }
}

/// Answer a `tools/call`. Unknown tools and argument keys are JSON-RPC errors; a failed call is a
/// tool result with `isError`.
pub fn call_tool(b: &mut dyn Backend, name: &str, args: &Value) -> Result<Value, RpcError> {
    let empty = Map::new();
    let args = match args {
        Value::Object(m) => m,
        Value::Null => &empty,
        _ => return Err(invalid("tool arguments must be an object")),
    };
    check_arguments(name, args)?;
    let failed = |e: McpError| text_tool_result(&e.to_string(), true);
    if let Some((method, params)) = ui_request(name, args)? {
        if !b.has_ui() {
            return Ok(text_tool_result(&needs_desktop(name), true));
        }
        return Ok(ui_call(b, method, params).unwrap_or_else(failed));
    }
    Ok(dispatch(b, name, args).unwrap_or_else(failed))
}
