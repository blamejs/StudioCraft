use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use serde_json::{Value, json};
use studiocraft_engine::links;
use studiocraft_studio::StudioError;

use crate::headless::STREAMING_REFUSED;
use crate::{Backend, Headless, MAX_BATCH_STEPS, McpError, Remote, Roots, Server, ServerOptions};

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

const TOOLS: [&str; 26] = [
    "command_list",
    "command_run",
    "command_batch",
    "doc_inspect",
    "render_preview",
    "undo",
    "redo",
    "scene_add",
    "scene_select",
    "source_add",
    "item_transform",
    "source_audio",
    "transition_set",
    "studio_transition",
    "collection_open",
    "collection_save",
    "app_links",
    "source_schema",
    "record_start",
    "record_stop",
    "stream_start",
    "stream_stop",
    "replay_start",
    "replay_stop",
    "replay_save",
    "output_status",
];

const UI_TOOLS: [&str; 10] =
    ["ui_inspect", "ui_screenshot", "ui_click", "ui_drag", "ui_key", "ui_type", "ui_menu_list", "ui_menu_invoke", "ui_dialog", "app_quit"];

fn server() -> Server {
    Server::new(Box::new(Headless::with_gpu(None).unwrap())).unwrap()
}

fn ask(s: &mut Server, msg: Value) -> Value {
    serde_json::from_str(&s.handle_line(msg.to_string().as_bytes()).unwrap()).unwrap()
}

/// A `tools/call` reply.
fn call(s: &mut Server, name: &str, args: Value) -> Value {
    ask(s, json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args}}))
}

/// A successful tool result's text, as JSON.
fn ok_json(reply: &Value) -> Value {
    let r = &reply["result"];
    assert_eq!(r["isError"], false, "{reply}");
    serde_json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap()
}

/// A failed tool result's text.
fn error_text(reply: &Value) -> String {
    let r = &reply["result"];
    assert_eq!(r["isError"], true, "{reply}");
    r["content"][0]["text"].as_str().unwrap().to_owned()
}

fn scene_names(s: &mut Server) -> Vec<String> {
    let doc = ok_json(&call(s, "doc_inspect", json!({})));
    doc["collection"]["scenes"].as_array().unwrap().iter().map(|sc| sc["name"].as_str().unwrap().to_owned()).collect()
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("studiocraft-mcp-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn initialize_names_the_server_the_backend_and_the_community() {
    let mut s = server();
    let init = ask(
        &mut s,
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": craft_mcp::PROTOCOL_VERSION, "capabilities": {}, "clientInfo": {"name": "test", "version": "1"},
        }}),
    );
    let r = &init["result"];
    assert_eq!(r["serverInfo"]["name"], "studiocraft");
    assert_eq!(r["protocolVersion"], craft_mcp::PROTOCOL_VERSION);
    assert!(r["capabilities"]["tools"].is_object() && r["capabilities"]["resources"].is_object(), "{r}");
    assert!(r["capabilities"]["prompts"].is_object() && r["capabilities"]["completions"].is_object(), "{r}");
    let text = r["instructions"].as_str().unwrap();
    assert!(text.contains("Backend: headless"), "{text}");
    assert!(text.contains("studiocraft://plugin/{kind}") && text.contains("completion/complete"), "{text}");
    assert!(text.contains(links::DISCORD) && text.contains(links::GITHUB), "{text}");
    assert!(!text.contains('\u{2014}'), "no em dash");
}

#[test]
fn every_tool_is_listed_once_with_a_title_and_annotations() {
    let mut s = server();
    let list = ask(&mut s, json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}));
    let tools = list["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    let unique: BTreeSet<&str> = names.iter().copied().collect();
    assert_eq!(unique.len(), names.len(), "{names:?}");
    assert_eq!(unique, TOOLS.iter().copied().collect::<BTreeSet<_>>());
    for t in tools {
        assert!(t["title"].is_string(), "{} has no title", t["name"]);
        assert!(t["description"].is_string());
        assert_eq!(t["inputSchema"]["type"], "object");
        for h in ["readOnlyHint", "destructiveHint", "idempotentHint", "openWorldHint"] {
            assert!(t["annotations"][h].is_boolean(), "{} lacks {h}", t["name"]);
        }
    }
    let ro = |n: &str| tools.iter().find(|t| t["name"] == n).map(|t| t["annotations"]["readOnlyHint"].clone());
    assert_eq!((ro("command_list"), ro("command_run"), ro("render_preview")), (Some(json!(true)), Some(json!(false)), Some(json!(true))));
}

#[test]
fn command_run_adds_a_scene_that_doc_inspect_shows() {
    let mut s = server();
    let r = ok_json(&call(&mut s, "command_run", json!({"id": "scene.add", "params": {"name": "Intro"}})));
    assert!(r["scene"].is_u64(), "{r}");
    assert!(scene_names(&mut s).contains(&"Intro".to_owned()));
    // The convenience tool runs the same command.
    ok_json(&call(&mut s, "scene_add", json!({"name": "Outro"})));
    assert!(scene_names(&mut s).contains(&"Outro".to_owned()));
    ok_json(&call(&mut s, "scene_select", json!({"scene": "Intro"})));
    let doc = ok_json(&call(&mut s, "doc_inspect", json!({"detail": "full"})));
    assert_eq!(doc["program_scene"], r["scene"]);
}

#[test]
fn command_list_filters_and_keeps_enabled_commands() {
    let mut s = server();
    let all = ok_json(&call(&mut s, "command_list", json!({"filter": "scene."})));
    assert!(all.as_array().unwrap().iter().all(|c| c["id"].as_str().unwrap().contains("scene")), "{all}");
    let undo = |s: &mut Server| {
        let list = ok_json(&call(s, "command_list", json!({"filter": "edit.undo", "enabled_only": true})));
        list.as_array().unwrap().len()
    };
    assert_eq!(undo(&mut s), 0, "nothing to undo yet");
    ok_json(&call(&mut s, "scene_add", json!({})));
    assert_eq!(undo(&mut s), 1);
}

#[test]
fn command_batch_stops_at_the_first_bad_step_and_reports_each_step() {
    let mut s = server();
    let reply = call(
        &mut s,
        "command_batch",
        json!({"steps": [
            {"id": "scene.add", "params": {"name": "A"}},
            {"id": "scene.fly"},
            {"id": "scene.add", "params": {"name": "B"}},
        ]}),
    );
    let report: Value = serde_json::from_str(&error_text(&reply)).unwrap();
    assert_eq!((report["completed"].as_u64(), report["failed"].as_u64(), report["skipped"].as_u64()), (Some(1), Some(1), Some(1)), "{report}");
    let steps = report["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 2);
    assert_eq!((steps[0]["ok"].as_bool(), steps[1]["ok"].as_bool()), (Some(true), Some(false)));
    assert!(steps[1]["error"].as_str().unwrap().contains("scene.fly"), "{report}");
    let names = scene_names(&mut s);
    assert!(names.contains(&"A".to_owned()) && !names.contains(&"B".to_owned()), "{names:?}");
    // Without stop_on_error every step runs.
    let reply =
        call(&mut s, "command_batch", json!({"stop_on_error": false, "steps": [{"id": "scene.fly"}, {"id": "scene.add", "params": {"name": "B"}}]}));
    let report: Value = serde_json::from_str(&error_text(&reply)).unwrap();
    assert_eq!((report["completed"].as_u64(), report["failed"].as_u64()), (Some(1), Some(1)));
    assert!(scene_names(&mut s).contains(&"B".to_owned()));
}

#[test]
fn a_batch_over_the_cap_runs_nothing() {
    let mut s = server();
    let before = scene_names(&mut s).len();
    let steps: Vec<Value> = (0..=MAX_BATCH_STEPS).map(|i| json!({"id": "scene.add", "params": {"name": format!("S{i}")}})).collect();
    let text = error_text(&call(&mut s, "command_batch", json!({"steps": steps})));
    assert!(text.contains("1001") && text.contains("1000"), "{text}");
    assert_eq!(scene_names(&mut s).len(), before);
    // A malformed step also stops the batch before anything runs.
    error_text(&call(&mut s, "command_batch", json!({"steps": [{"id": "scene.add"}, {"command": "scene.add"}]})));
    assert_eq!(scene_names(&mut s).len(), before);
}

#[test]
fn undo_takes_back_scene_add_and_redo_makes_it_again() {
    let mut s = server();
    ok_json(&call(&mut s, "scene_add", json!({"name": "Temporary"})));
    assert!(scene_names(&mut s).contains(&"Temporary".to_owned()));
    ok_json(&call(&mut s, "undo", json!({})));
    assert!(!scene_names(&mut s).contains(&"Temporary".to_owned()));
    ok_json(&call(&mut s, "redo", json!({})));
    assert!(scene_names(&mut s).contains(&"Temporary".to_owned()));
}

#[test]
fn source_add_with_an_unknown_kind_is_a_tool_error() {
    let mut s = server();
    let text = error_text(&call(&mut s, "source_add", json!({"kind": "hologram"})));
    assert!(text.contains("hologram"), "{text}");
    error_text(&call(&mut s, "source_add", json!({"kind": 5})));
    let added = ok_json(&call(
        &mut s,
        "source_add",
        json!({"kind": "color", "name": "Backdrop", "settings": {"color": "#202830ff", "width": 640, "height": 360}}),
    ));
    assert!(added.is_object(), "{added}");
    ok_json(&call(&mut s, "item_transform", json!({"item": "Backdrop", "transform": {"rotation": 15.0}})));
    let doc = ok_json(&call(&mut s, "doc_inspect", json!({})));
    let items = doc["collection"]["scenes"][0]["items"].as_array().unwrap();
    assert!(items.iter().any(|i| i["transform"]["rotation"] == json!(15.0)), "{doc}");
}

#[test]
fn source_schema_lists_a_camera_sources_fields() {
    let mut s = server();
    let r = ok_json(&call(&mut s, "source_schema", json!({"kind": "camera", "settings": {"device": "Desk\u{1f}1"}})));
    let fields = r["schema"]["fields"].as_array().unwrap();
    let keys: Vec<&str> = fields.iter().filter_map(|f| f["key"].as_str()).collect();
    assert_eq!(keys, ["device", "format"], "{r}");
    assert_eq!(fields[1]["kind"]["options"][0]["value"], "", "the default format comes first");
    assert!(error_text(&call(&mut s, "source_schema", json!({"kind": "hologram"}))).contains("hologram"));
    error_text(&call(&mut s, "source_schema", json!({})));
}

#[test]
fn source_audio_sets_the_sync_offset() {
    let mut s = server();
    let set = ok_json(&call(&mut s, "source_audio", json!({"source": "Microphone", "sync_offset_ms": 180})));
    assert_eq!(set["audio"]["sync_offset_ms"], 180, "{set}");
    assert_eq!(set["name"], "Microphone");
    let id = set["source"].clone();
    let read = ok_json(&call(&mut s, "source_audio", json!({"source": id})));
    assert_eq!(read["audio"]["sync_offset_ms"], 180, "without a value it only reads: {read}");
    for ms in [json!(5001), json!(-1), json!(2.5), json!("late")] {
        error_text(&call(&mut s, "source_audio", json!({"source": "Microphone", "sync_offset_ms": ms})));
    }
    let read = ok_json(&call(&mut s, "source_audio", json!({"source": "Microphone"})));
    assert_eq!(read["audio"]["sync_offset_ms"], 180, "a refused value changes nothing");
    error_text(&call(&mut s, "source_audio", json!({"source": "ghost", "sync_offset_ms": 10})));
    error_text(&call(&mut s, "source_audio", json!({"source": "ghost"})));
    error_text(&call(&mut s, "source_audio", json!({"source": true})));
}

/// A sink with no devices, so the tests play nothing.
struct NoDevices;

impl studiocraft_studio::AudioSink for NoDevices {
    fn play(
        &self,
        _: &str,
        _: studiocraft_studio::Fill,
        _: studiocraft_studio::OnError,
    ) -> Result<Box<dyn studiocraft_studio::Playing>, studiocraft_studio::PlatformError> {
        Err(studiocraft_studio::PlatformError::Unsupported)
    }
}

#[test]
fn source_audio_sets_the_monitoring_mode() {
    let mut h = Headless::with_gpu(None).unwrap();
    h.studio_mut().set_audio_sink(Arc::new(NoDevices));
    let mut s = Server::new(Box::new(h)).unwrap();
    let set = ok_json(&call(&mut s, "source_audio", json!({"source": "Microphone", "monitor": "monitor_only"})));
    assert_eq!(set["audio"]["monitor"], "monitor_only", "{set}");
    let both = ok_json(&call(&mut s, "source_audio", json!({"source": "Microphone", "monitor": "monitor_and_output", "sync_offset_ms": 40})));
    assert_eq!((both["audio"]["monitor"].clone(), both["audio"]["sync_offset_ms"].clone()), (json!("monitor_and_output"), json!(40)), "{both}");
    for mode in [json!("loud"), json!(1), json!(true)] {
        error_text(&call(&mut s, "source_audio", json!({"source": "Microphone", "monitor": mode, "sync_offset_ms": 99})));
    }
    let read = ok_json(&call(&mut s, "source_audio", json!({"source": "Microphone"})));
    assert_eq!(
        (read["audio"]["monitor"].clone(), read["audio"]["sync_offset_ms"].clone()),
        (json!("monitor_and_output"), json!(40)),
        "a refused value changes nothing"
    );
    error_text(&call(&mut s, "source_audio", json!({"source": "ghost", "monitor": "off"})));
}

#[test]
fn transition_tools_set_the_kind_and_length() {
    let mut s = server();
    ok_json(&call(&mut s, "transition_set", json!({"kind": "cut", "ms": 450})));
    let doc = ok_json(&call(&mut s, "doc_inspect", json!({})));
    assert_eq!((doc["collection"]["transition"]["kind"].as_str(), doc["collection"]["transition"]["duration_ms"].as_u64()), (Some("cut"), Some(450)));
    error_text(&call(&mut s, "transition_set", json!({"kind": "teleport"})));
    // A bad length changes nothing, the kind included.
    for ms in [json!(1_000_000), json!(-5), json!(2.5), json!("long")] {
        error_text(&call(&mut s, "transition_set", json!({"kind": "fade", "ms": ms})));
    }
    let doc = ok_json(&call(&mut s, "doc_inspect", json!({})));
    assert_eq!(doc["collection"]["transition"]["kind"].as_str(), Some("cut"));
    let text = error_text(&call(&mut s, "studio_transition", json!({})));
    assert!(text.contains("studio mode"), "{text}");
    ok_json(&call(&mut s, "command_run", json!({"id": "studio.toggle"})));
    ok_json(&call(&mut s, "studio_transition", json!({"kind": "cut"})));
}

#[test]
fn render_preview_returns_an_image_or_says_nothing_is_drawn() {
    let mut s = Server::new(Box::new(Headless::new().unwrap())).unwrap();
    let reply = call(&mut s, "render_preview", json!({"max_side": 64}));
    let r = &reply["result"];
    if r["isError"] == true {
        assert_eq!(r["content"][0]["text"], StudioError::NoGpu.to_string(), "{reply}");
    } else {
        assert_eq!(r["content"][0]["type"], "image", "{reply}");
        assert_eq!(r["content"][0]["mimeType"], "image/png");
        assert!(!r["content"][0]["data"].as_str().unwrap().is_empty());
    }
    error_text(&call(&mut s, "render_preview", json!({"max_side": 5000})));
    error_text(&call(&mut s, "render_preview", json!({"view": "side"})));
}

#[test]
fn render_preview_waits_for_the_transition_and_new_sources() {
    let mut s = Server::new(Box::new(Headless::new().unwrap())).unwrap();
    ok_json(&call(&mut s, "transition_set", json!({"kind": "fade", "ms": 1000})));
    ok_json(&call(&mut s, "scene_add", json!({"name": "Intro"})));
    let fill = json!({"color": "#ff0000ff", "width": 1920, "height": 1080});
    ok_json(&call(&mut s, "source_add", json!({"kind": "color", "scene": "Intro", "settings": fill})));
    let reply = call(&mut s, "render_preview", json!({"max_side": 64}));
    let r = &reply["result"];
    if r["isError"] == true {
        assert_eq!(r["content"][0]["text"], StudioError::NoGpu.to_string(), "{reply}");
        return;
    }
    let png = base64::engine::general_purpose::STANDARD.decode(r["content"][0]["data"].as_str().unwrap()).unwrap();
    let picture = image::load_from_memory(&png).unwrap().to_rgba8();
    let center = picture.get_pixel(picture.width() / 2, picture.height() / 2).0;
    assert_eq!(center, [255, 0, 0, 255], "the 1 s fade has ended and the new source is drawn");
}

#[test]
fn window_calls_refuse_in_the_headless_studio() {
    let mut s = server();
    let list = ask(&mut s, json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}));
    assert!(list["result"]["tools"].as_array().unwrap().iter().all(|t| !t["name"].as_str().unwrap().starts_with("ui_")));
    for id in ["view.stats", "app.exit", "app.settings"] {
        let text = error_text(&call(&mut s, "command_run", json!({"id": id})));
        assert!(text.contains("studiocraft --control"), "{id}: {text}");
    }
    let mut h = Headless::with_gpu(None).unwrap();
    assert!(!h.has_ui());
    assert!(h.describe().starts_with("headless"));
    match h.call("ui.inspect", json!({})) {
        Err(McpError::NeedsApp(text)) => assert!(text.contains("studiocraft-cli mcp"), "{text}"),
        other => panic!("expected NeedsApp, got {other:?}"),
    }
    assert!(matches!(h.call("ui.fly", json!({})), Err(McpError::NeedsApp(_))));
    assert!(matches!(h.call("document.inspect", json!([1])), Err(McpError::Invalid(_))));
}

#[test]
fn collection_save_and_open_round_trip() {
    let dir = temp_dir("round");
    let path = dir.join("show.json");
    let path_text = path.to_string_lossy().into_owned();
    let mut s = server();
    ok_json(&call(&mut s, "scene_add", json!({"name": "Saved"})));
    ok_json(&call(&mut s, "collection_save", json!({"path": path_text})));
    assert!(path.is_file());
    ok_json(&call(&mut s, "command_run", json!({"id": "scene.remove", "params": {"scene": "Saved"}})));
    ok_json(&call(&mut s, "scene_add", json!({"name": "Later"})));
    ok_json(&call(&mut s, "collection_open", json!({"path": path_text})));
    let names = scene_names(&mut s);
    assert!(names.contains(&"Saved".to_owned()) && !names.contains(&"Later".to_owned()), "{names:?}");
    let text = error_text(&call(&mut s, "collection_open", json!({"path": dir.join("absent.json").to_string_lossy()})));
    assert!(text.contains("absent.json"), "{text}");
    error_text(&call(&mut s, "collection_save", json!({"path": " "})));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resources_list_and_read() {
    let mut s = server();
    let list = ask(&mut s, json!({"jsonrpc": "2.0", "id": 1, "method": "resources/list"}));
    let uris: Vec<&str> = list["result"]["resources"].as_array().unwrap().iter().map(|r| r["uri"].as_str().unwrap()).collect();
    assert_eq!(uris, ["studiocraft://document", "studiocraft://commands"]);
    let read = |s: &mut Server, uri: &str| ask(s, json!({"jsonrpc": "2.0", "id": 2, "method": "resources/read", "params": {"uri": uri}}));
    let doc = read(&mut s, "studiocraft://document");
    let content = &doc["result"]["contents"][0];
    assert_eq!(content["mimeType"], "application/json");
    let state: Value = serde_json::from_str(content["text"].as_str().unwrap()).unwrap();
    // The full document carries source settings.
    assert!(state["collection"]["sources"][0]["settings"].is_object(), "{state}");
    let commands = read(&mut s, "studiocraft://commands");
    let list: Value = serde_json::from_str(commands["result"]["contents"][0]["text"].as_str().unwrap()).unwrap();
    assert!(list.as_array().unwrap().iter().any(|c| c["id"] == "scene.add"));
    assert_eq!(read(&mut s, "studiocraft://elsewhere")["error"]["code"], -32002);
    assert_eq!(ask(&mut s, json!({"jsonrpc": "2.0", "id": 3, "method": "resources/read", "params": {}}))["error"]["code"], -32602);
}

fn rpc(s: &mut Server, method: &str, params: Value) -> Value {
    ask(s, json!({"jsonrpc": "2.0", "id": 9, "method": method, "params": params}))
}

/// A `resources/read` reply's text as JSON, or the error code.
fn read_resource(s: &mut Server, uri: &str) -> Result<Value, i64> {
    let reply = rpc(s, "resources/read", json!({"uri": uri}));
    match reply["error"]["code"].as_i64() {
        Some(code) => Err(code),
        None => Ok(serde_json::from_str(reply["result"]["contents"][0]["text"].as_str().unwrap()).unwrap()),
    }
}

fn complete(s: &mut Server, reference: Value, name: &str, value: &str) -> Value {
    rpc(s, "completion/complete", json!({"ref": reference, "argument": {"name": name, "value": value}}))["result"]["completion"].clone()
}

#[test]
fn resource_templates_list_and_read_each_kind() {
    let mut s = server();
    let list = rpc(&mut s, "resources/templates/list", json!({}));
    let uris: Vec<&str> = list["result"]["resourceTemplates"].as_array().unwrap().iter().map(|t| t["uriTemplate"].as_str().unwrap()).collect();
    assert_eq!(uris, ["studiocraft://scene/{id}", "studiocraft://source/{id}", "studiocraft://command/{id}", "studiocraft://plugin/{kind}"]);
    let scene = ok_json(&call(&mut s, "scene_add", json!({"name": "Be right back"})))["scene"].as_u64().unwrap();
    let added = ok_json(&call(&mut s, "source_add", json!({"kind": "color", "scene": scene, "name": "Backdrop"})));
    let source = added["source"].as_u64().unwrap();

    // A scene by id and by its percent-encoded name, with each item's source name.
    let by_id = read_resource(&mut s, &format!("studiocraft://scene/{scene}")).unwrap();
    assert_eq!(by_id["name"], "Be right back");
    assert_eq!(by_id["items"][0]["source_name"], "Backdrop");
    // A new scene goes to the program; the first one leaves it.
    assert_eq!((by_id["program"].clone(), by_id["preview"].clone()), (json!(true), json!(false)));
    assert_eq!(read_resource(&mut s, "studiocraft://scene/Scene").unwrap()["program"], false);
    assert_eq!(read_resource(&mut s, "studiocraft://scene/Be%20right%20back").unwrap()["id"], scene);

    // A source with its settings and the scenes that place it.
    let src = read_resource(&mut s, "studiocraft://source/Backdrop").unwrap();
    assert_eq!(src["id"], source);
    assert!(src["settings"].is_object(), "{src}");
    assert_eq!(src["scenes"], json!(["Be right back"]));
    assert_eq!(read_resource(&mut s, &format!("studiocraft://source/{source}")).unwrap()["name"], "Backdrop");
    assert_eq!(read_resource(&mut s, "studiocraft://source/Microphone").unwrap()["global_audio"], true);

    let cmd = read_resource(&mut s, "studiocraft://command/item.transform").unwrap();
    assert_eq!(cmd["id"], "item.transform");
    assert!(cmd["params"].as_str().unwrap().contains("transform"), "{cmd}");

    // Plug-in kinds of each type, with their settings schema.
    let text = read_resource(&mut s, "studiocraft://plugin/text").unwrap();
    assert_eq!(text["type"], "source");
    assert!(text["schema"]["fields"].as_array().unwrap().iter().any(|f| f["key"] == "color"), "{text}");
    assert_eq!(read_resource(&mut s, "studiocraft://plugin/chroma_key").unwrap()["type"], "filter");
    assert_eq!(read_resource(&mut s, "studiocraft://plugin/fade").unwrap()["type"], "transition");
}

#[test]
fn unknown_template_values_are_not_found() {
    let mut s = server();
    for uri in [
        "studiocraft://scene/9999",
        "studiocraft://scene/Nowhere",
        "studiocraft://scene/",
        "studiocraft://scene/{id}",
        "studiocraft://source/ghost",
        "studiocraft://command/no.such",
        "studiocraft://plugin/nothing",
        "studiocraft://plugin/",
    ] {
        assert_eq!(read_resource(&mut s, uri), Err(-32002), "{uri}");
    }
}

#[test]
fn prompts_list_and_get_fill_their_arguments() {
    let mut s = server();
    let list = rpc(&mut s, "prompts/list", json!({}));
    let names: Vec<&str> = list["result"]["prompts"].as_array().unwrap().iter().map(|p| p["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["build_scene", "lower_third", "check_collection"]);
    let get = |s: &mut Server, name: &str, args: Value| rpc(s, "prompts/get", json!({"name": name, "arguments": args}));
    let text_of = |reply: &Value| reply["result"]["messages"][0]["content"]["text"].as_str().unwrap().to_owned();

    let build = text_of(&get(&mut s, "build_scene", json!({"description": "a webcam in the corner over a game capture"})));
    assert!(build.contains("a webcam in the corner over a game capture"), "{build}");
    let at = |s: &str| build.find(s).unwrap_or(usize::MAX);
    assert!(at("scene_add") < at("source_add") && at("source_add") < at("item_transform") && at("item_transform") < at("render_preview"));

    let lower = text_of(&get(&mut s, "lower_third", json!({"text": "Ada Lovelace, host", "color": "#ffd400ff"})));
    assert!(lower.contains("Ada Lovelace, host") && lower.contains("#ffd400ff") && lower.contains("kind `text`"), "{lower}");
    assert!(lower.contains("80 percent of the canvas height"), "{lower}");
    // Left out, the color takes its default; a value that looks like a placeholder stays as written.
    let plain = text_of(&get(&mut s, "lower_third", json!({"text": "{color}"})));
    assert!(plain.contains("Text: {color}") && plain.contains("#ffffffff"), "{plain}");

    let check = text_of(&get(&mut s, "check_collection", json!({})));
    for part in ["Missing files", "Empty scenes", "Hidden-only scenes"] {
        assert!(check.contains(part), "{part}: {check}");
    }

    // A missing or empty required argument, an unknown argument and an unknown prompt are invalid params.
    for (name, args) in [
        ("build_scene", json!({})),
        ("lower_third", json!({"text": "  "})),
        ("lower_third", json!({"text": "x", "size": "40"})),
        ("lower_third", json!({"text": 5})),
        ("no_such_prompt", json!({})),
    ] {
        let reply = get(&mut s, name, args.clone());
        assert_eq!(reply["error"]["code"], -32602, "{name} {args}: {reply}");
    }
    let missing = get(&mut s, "build_scene", json!({}));
    assert!(missing["error"]["message"].as_str().unwrap().contains("description"), "{missing}");
    assert_eq!(rpc(&mut s, "prompts/get", json!({}))["error"]["code"], -32602);
}

#[test]
fn completion_finishes_commands_scenes_sources_and_plugin_kinds() {
    let mut s = server();
    let command = json!({"type": "ref/resource", "uri": "studiocraft://command/{id}"});
    let sce = complete(&mut s, command.clone(), "id", "sce");
    let values: Vec<&str> = sce["values"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    assert!(values.contains(&"scene.add") && values.contains(&"scene.select"), "{sce}");
    assert!(values.iter().all(|v| v.starts_with("scene.")), "{sce}");
    // Case is ignored, and a match inside a value does not count.
    assert_eq!(complete(&mut s, command.clone(), "id", "SCE")["values"], sce["values"]);
    assert_eq!(complete(&mut s, command, "id", "add")["values"], json!([]));

    ok_json(&call(&mut s, "scene_add", json!({"name": "Interview"})));
    ok_json(&call(&mut s, "source_add", json!({"kind": "color", "name": "Interview backdrop"})));
    let scenes = complete(&mut s, json!({"type": "ref/resource", "uri": "studiocraft://scene/{id}"}), "id", "int");
    assert_eq!(scenes["values"], json!(["Interview"]));
    let sources = complete(&mut s, json!({"type": "ref/resource", "uri": "studiocraft://source/{id}"}), "id", "int");
    assert_eq!(sources["values"], json!(["Interview backdrop"]));
    let kinds = complete(&mut s, json!({"type": "ref/resource", "uri": "studiocraft://plugin/{kind}"}), "kind", "c");
    let kinds: Vec<&str> = kinds["values"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    assert!(kinds.contains(&"color") && kinds.contains(&"chroma_key") && kinds.contains(&"cut"), "{kinds:?}");

    // Prompt arguments: live scene names and the fixed color list.
    let lower = json!({"type": "ref/prompt", "name": "lower_third"});
    assert_eq!(complete(&mut s, lower.clone(), "scene", "in")["values"], json!(["Interview"]));
    assert_eq!(complete(&mut s, lower.clone(), "color", "#ffff")["values"], json!(["#ffffffff"]));
    // Unknown prompts, arguments and templates give no values.
    for (reference, name) in [
        (lower, "nope"),
        (json!({"type": "ref/prompt", "name": "nope"}), "text"),
        (json!({"type": "ref/resource", "uri": "studiocraft://nope/{id}"}), "id"),
        (json!({"type": "ref/other"}), "id"),
    ] {
        let c = complete(&mut s, reference.clone(), name, "");
        assert_eq!((c["values"].clone(), c["total"].clone(), c["hasMore"].clone()), (json!([]), json!(0), json!(false)), "{reference}");
    }
}

#[test]
fn completion_returns_at_most_100_values() {
    let mut s = server();
    let steps: Vec<Value> = (0..150).map(|i| json!({"id": "scene.add", "params": {"name": format!("Scene {i:03}")}})).collect();
    let batch = ok_json(&call(&mut s, "command_batch", json!({"steps": steps})));
    assert_eq!(batch["completed"], 150, "{batch}");
    let c = complete(&mut s, json!({"type": "ref/resource", "uri": "studiocraft://scene/{id}"}), "id", "scene ");
    assert_eq!(c["values"].as_array().unwrap().len(), 100);
    assert_eq!(c["total"], 150);
    assert_eq!(c["hasMore"], true);
    let one = complete(&mut s, json!({"type": "ref/resource", "uri": "studiocraft://scene/{id}"}), "id", "scene 14");
    assert_eq!(one["total"], 10);
    assert_eq!(one["hasMore"], false);
}

#[test]
fn unknown_arguments_and_tools_are_invalid_params() {
    let mut s = server();
    let r = call(&mut s, "scene_add", json!({"name": "A", "colour": "red"}));
    assert_eq!(r["error"]["code"], -32602, "{r}");
    assert!(r["error"]["message"].as_str().unwrap().contains("colour"));
    assert_eq!(call(&mut s, "delete_everything", json!({}))["error"]["code"], -32602);
    assert_eq!(call(&mut s, "studio_state", json!({}))["error"]["code"], -32602);
    assert!(!scene_names(&mut s).contains(&"A".to_owned()));
    // Malformed input is a parse error, and the server keeps answering.
    let parse: Value = serde_json::from_str(&s.handle_line(b"{ not json").unwrap()).unwrap();
    assert_eq!(parse["error"]["code"], -32700);
    assert_eq!(s.handle_line(b"   "), None);
    assert_eq!(ask(&mut s, json!({"jsonrpc": "2.0", "id": 4, "method": "nothing/here"}))["error"]["code"], -32601);
}

#[test]
fn serve_answers_each_line() {
    let mut s = server();
    let input =
        format!("{}\n{}\n", json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}), json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    let mut out = Vec::new();
    s.serve(&mut input.as_bytes(), &mut out).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert_eq!(text.lines().count(), 1, "{text}");
}

/// A fake app: accepts one connection per entry in `replies`, counts the request lines it reads,
/// then drops the connection (`false`) or answers ok (`true`).
fn fake_app(replies: Vec<bool>) -> (String, Arc<AtomicUsize>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let seen = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&seen);
    std::thread::spawn(move || {
        for answer in replies {
            let Ok((mut sock, _)) = listener.accept() else { return };
            let mut line = String::new();
            if BufReader::new(sock.try_clone().unwrap()).read_line(&mut line).unwrap_or(0) > 0 {
                counter.fetch_add(1, Ordering::SeqCst);
                if answer {
                    let _ = writeln!(sock, "{}", json!({"id": 1, "ok": true, "result": {"done": true}}));
                }
            }
        }
    });
    (addr, seen)
}

#[test]
fn remote_does_not_repeat_a_mutation_after_a_lost_reply() {
    let (addr, seen) = fake_app(vec![false, true]);
    let mut r = Remote::new(&addr, TOKEN).unwrap();
    let err = r.call("engine.execute", json!({"command": "scene.add"})).unwrap_err();
    assert!(matches!(err, McpError::Lost { .. }), "{err:?}");
    assert!(err.to_string().contains("may or may not have run"), "{err}");
    assert_eq!(seen.load(Ordering::SeqCst), 1);
}

#[test]
fn remote_sends_a_mutation_on_a_new_connection_when_the_app_closed_the_old_one() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let (closed_tx, closed_rx) = std::sync::mpsc::channel();
    let seen = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&seen);
    std::thread::spawn(move || {
        for _ in 0..2 {
            let Ok((mut sock, _)) = listener.accept() else { return };
            let mut line = String::new();
            if BufReader::new(sock.try_clone().unwrap()).read_line(&mut line).unwrap_or(0) > 0 {
                counter.fetch_add(1, Ordering::SeqCst);
                let _ = writeln!(sock, "{}", json!({"id": 1, "ok": true, "result": {"done": true}}));
            }
            drop(sock);
            let _ = closed_tx.send(());
        }
    });
    let mut r = Remote::new(&addr, TOKEN).unwrap();
    assert_eq!(r.call("engine.execute", json!({"command": "scene.add"})).unwrap(), json!({"done": true}));
    closed_rx.recv().unwrap();
    // The app closed the first connection; the second request goes out on a new one.
    assert_eq!(r.call("engine.execute", json!({"command": "scene.add"})).unwrap(), json!({"done": true}));
    assert_eq!(seen.load(Ordering::SeqCst), 2);
}

#[test]
fn remote_retries_a_read_after_a_lost_reply() {
    let (addr, seen) = fake_app(vec![false, true]);
    let mut r = Remote::new(&addr, TOKEN).unwrap();
    assert_eq!(r.call("document.inspect", json!({})).unwrap(), json!({"done": true}));
    assert_eq!(seen.load(Ordering::SeqCst), 2);
    assert!(r.has_ui());
    assert_eq!(r.describe(), format!("StudioCraft app at {addr}"));
}

#[test]
fn remote_refuses_other_hosts_and_bad_tokens() {
    for target in ["10.0.0.5:9876", "192.168.1.20:9876", "example.com:9876"] {
        assert!(matches!(Remote::new(target, TOKEN), Err(McpError::BadTarget(_))), "{target}");
    }
    assert!(matches!(Remote::new("9876", "short"), Err(McpError::Token(_))));
    // Nothing listens: the error names the address.
    let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let mut r = Remote::new(&free.to_string(), TOKEN).unwrap();
    let err = r.call("document.inspect", json!({})).unwrap_err();
    assert!(matches!(err, McpError::Connect { .. }), "{err:?}");
}

/// Each control request the fake window read: its method and params.
type Seen = Arc<Mutex<Vec<(String, Value)>>>;

/// A PNG file's first bytes followed by filler, as base64.
fn png_base64() -> String {
    let mut bytes = vec![0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
    bytes.extend_from_slice(b"IHDR-and-the-rest");
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// A fake app window on one connection: records every request, then answers it with
/// `answer(method)`. The record is written before the reply, so it is complete when a call returns.
fn fake_window(answer: fn(&str) -> Value) -> (Server, Seen) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let record = Arc::clone(&seen);
    std::thread::spawn(move || {
        let Ok((mut sock, _)) = listener.accept() else { return };
        let mut reader = BufReader::new(sock.try_clone().unwrap());
        let mut line = String::new();
        while reader.read_line(&mut line).unwrap_or(0) > 0 {
            let req: Value = serde_json::from_str(&line).unwrap();
            let method = req["method"].as_str().unwrap().to_owned();
            record.lock().unwrap().push((method.clone(), req["params"].clone()));
            let reply = json!({"id": req["id"], "ok": true, "result": answer(&method)});
            if writeln!(sock, "{reply}").is_err() {
                return;
            }
            line.clear();
        }
    });
    (Server::new(Box::new(Remote::new(&addr, TOKEN).unwrap())).unwrap(), seen)
}

fn window_answers(method: &str) -> Value {
    match method {
        "ui.screenshot" => json!({"width": 800, "height": 600, "png_base64": png_base64()}),
        "ui.inspect" => json!({"window": {"width": 800, "height": 600}}),
        "app.quit" => json!({"confirming": false}),
        _ => json!({"queued": true}),
    }
}

fn last_request(seen: &Seen) -> (String, Value) {
    seen.lock().unwrap().last().cloned().unwrap()
}

#[test]
fn bridge_lists_the_window_tools_with_read_only_hints() {
    let (mut s, _) = fake_window(window_answers);
    let list = ask(&mut s, json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}));
    let tools = list["result"]["tools"].as_array().unwrap();
    let names: BTreeSet<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, TOOLS.iter().chain(UI_TOOLS.iter()).copied().collect::<BTreeSet<_>>());
    let ro = |n: &str| tools.iter().find(|t| t["name"] == n).map(|t| t["annotations"]["readOnlyHint"].clone());
    for name in ["ui_inspect", "ui_screenshot", "ui_menu_list"] {
        assert_eq!(ro(name), Some(json!(true)), "{name}");
    }
    for name in ["ui_click", "ui_drag", "ui_key", "ui_type", "ui_menu_invoke", "ui_dialog", "app_quit"] {
        assert_eq!(ro(name), Some(json!(false)), "{name}");
    }
    let init = ask(
        &mut s,
        json!({"jsonrpc": "2.0", "id": 2, "method": "initialize", "params": {
            "protocolVersion": craft_mcp::PROTOCOL_VERSION, "capabilities": {}, "clientInfo": {"name": "test", "version": "1"},
        }}),
    );
    assert!(init["result"]["instructions"].as_str().unwrap().contains("ui_screenshot"), "{init}");
}

#[test]
fn window_tools_send_their_control_methods() {
    let (mut s, seen) = fake_window(window_answers);
    let cases = [
        ("ui_inspect", json!({}), "ui.inspect", json!({})),
        ("ui_menu_list", json!({}), "ui.menu.list", json!({})),
        ("app_quit", json!({}), "app.quit", json!({})),
        ("ui_click", json!({"x": 10, "y": 20.5}), "ui.click", json!({"x": 10, "y": 20.5})),
        (
            "ui_click",
            json!({"x": 1, "y": 2, "button": "secondary", "count": 2, "modifiers": ["cmd", "shift"]}),
            "ui.click",
            json!({"x": 1, "y": 2, "button": "secondary", "count": 2, "modifiers": ["cmd", "shift"]}),
        ),
        (
            "ui_drag",
            json!({"from": {"x": 5, "y": 6}, "to": {"x": 50, "y": 60}, "steps": 4}),
            "ui.drag",
            json!({"from": {"x": 5, "y": 6}, "to": {"x": 50, "y": 60}, "steps": 4}),
        ),
        ("ui_key", json!({"key": "Cmd+Z"}), "ui.key", json!({"key": "Cmd+Z"})),
        ("ui_type", json!({"text": "Intro"}), "ui.type", json!({"text": "Intro"})),
        ("ui_menu_invoke", json!({"path": "Scene Collection > New..."}), "ui.menu.invoke", json!({"path": "Scene Collection > New..."})),
        ("ui_dialog", json!({"answer": "ok", "text": "Show B"}), "ui.dialog", json!({"answer": "ok", "text": "Show B"})),
        ("ui_dialog", json!({"answer": "cancel", "text": null}), "ui.dialog", json!({"answer": "cancel"})),
    ];
    for (tool, args, method, params) in cases {
        let reply = ok_json(&call(&mut s, tool, args));
        assert_eq!(last_request(&seen), (method.to_owned(), params), "{tool}");
        assert_eq!(reply, window_answers(method), "{tool}");
    }
    // The screenshot's default size and an explicit one.
    call(&mut s, "ui_screenshot", json!({}));
    assert_eq!(last_request(&seen), ("ui.screenshot".to_owned(), json!({"max_side": 1600})));
    call(&mut s, "ui_screenshot", json!({"max_side": 0}));
    assert_eq!(last_request(&seen), ("ui.screenshot".to_owned(), json!({"max_side": 0})));
}

#[test]
fn ui_screenshot_returns_the_window_as_a_png_image() {
    let (mut s, _) = fake_window(window_answers);
    let reply = call(&mut s, "ui_screenshot", json!({"max_side": 800}));
    let r = &reply["result"];
    assert_eq!(r["isError"], false, "{reply}");
    assert_eq!((r["content"][0]["type"].as_str(), r["content"][0]["mimeType"].as_str()), (Some("image"), Some("image/png")));
    assert_eq!(r["content"][0]["data"].as_str(), Some(png_base64().as_str()));
    assert_eq!(r["content"][1]["text"], "window, 800 x 600 pixels");
}

#[test]
fn a_screenshot_that_is_not_a_png_is_a_tool_error() {
    let (mut s, _) = fake_window(|_| json!({"width": 1, "height": 1, "png_base64": "not base64 at all!"}));
    let text = error_text(&call(&mut s, "ui_screenshot", json!({})));
    assert!(text.contains("base64"), "{text}");
    let (mut s, _) = fake_window(|_| json!({"width": 1, "height": 1, "png_base64": base64::engine::general_purpose::STANDARD.encode(b"GIF89a")}));
    let text = error_text(&call(&mut s, "ui_screenshot", json!({})));
    assert!(text.contains("not a PNG"), "{text}");
    let (mut s, _) = fake_window(|_| json!({"width": 1, "height": 1}));
    error_text(&call(&mut s, "ui_screenshot", json!({})));
}

#[test]
fn bad_window_arguments_are_invalid_params_and_send_nothing() {
    let (mut s, seen) = fake_window(window_answers);
    let bad = [
        ("ui_click", json!({"x": "10", "y": 20})),
        ("ui_click", json!({"x": 10})),
        ("ui_click", json!({"x": -1, "y": 20})),
        ("ui_click", json!({"x": 10, "y": true})),
        ("ui_click", json!({"x": 1, "y": 2, "button": "left"})),
        ("ui_click", json!({"x": 1, "y": 2, "count": 4})),
        ("ui_click", json!({"x": 1, "y": 2, "count": 0})),
        ("ui_click", json!({"x": 1, "y": 2, "modifiers": ["meta"]})),
        ("ui_click", json!({"x": 1, "y": 2, "modifiers": "cmd"})),
        ("ui_drag", json!({"from": {"x": 1, "y": 2}})),
        ("ui_drag", json!({"from": {"x": 1}, "to": {"x": 3, "y": 4}})),
        ("ui_drag", json!({"from": [1, 2], "to": {"x": 3, "y": 4}})),
        ("ui_drag", json!({"from": {"x": 1, "y": 2, "z": 0}, "to": {"x": 3, "y": 4}})),
        ("ui_drag", json!({"from": {"x": 1, "y": 2}, "to": {"x": 3, "y": 4}, "steps": 201})),
        ("ui_key", json!({"key": ""})),
        ("ui_key", json!({"key": 7})),
        ("ui_type", json!({"text": ""})),
        ("ui_type", json!({"text": "x".repeat(10_001)})),
        ("ui_menu_invoke", json!({"path": " "})),
        ("ui_dialog", json!({})),
        ("ui_dialog", json!({"answer": "yes"})),
        ("ui_dialog", json!({"answer": true})),
        ("ui_dialog", json!({"answer": "ok", "text": 5})),
        ("ui_screenshot", json!({"max_side": 4097})),
        ("ui_screenshot", json!({"max_side": "big"})),
        ("ui_inspect", json!({"detail": "full"})),
    ];
    for (tool, args) in bad {
        let reply = call(&mut s, tool, args.clone());
        assert_eq!(reply["error"]["code"], -32602, "{tool} {args}: {reply}");
    }
    assert!(seen.lock().unwrap().is_empty(), "nothing reaches the window");
    // The longest text is accepted.
    ok_json(&call(&mut s, "ui_type", json!({"text": "x".repeat(10_000)})));
}

#[test]
fn window_tools_on_the_headless_studio_say_how_to_start_the_app() {
    let mut s = server();
    let list = ask(&mut s, json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}));
    let names: Vec<&str> = list["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(UI_TOOLS.iter().all(|t| !names.contains(t)), "{names:?}");
    let text = error_text(&call(&mut s, "ui_click", json!({"x": 1, "y": 2})));
    assert!(text.contains("needs the desktop app"), "{text}");
    assert!(text.contains("studiocraft --control PORT") && text.contains("STUDIOCRAFT_CONTROL_TOKEN"), "{text}");
    assert!(text.contains("studiocraft-cli mcp --connect PORT"), "{text}");
    for tool in ["ui_inspect", "ui_screenshot", "ui_menu_list", "app_quit"] {
        assert!(error_text(&call(&mut s, tool, json!({}))).contains("studiocraft --control PORT"), "{tool}");
    }
    // Bad arguments are still invalid params.
    assert_eq!(call(&mut s, "ui_click", json!({"x": "1", "y": 2}))["error"]["code"], -32602);
}

fn headless_with(options: ServerOptions) -> Server {
    Server::new(Box::new(Headless::with_options(None, options).unwrap())).unwrap()
}

/// A server held to `read` and `write` roots, without streaming.
fn confined(read: &[std::path::PathBuf], write: &[std::path::PathBuf]) -> Server {
    headless_with(ServerOptions { allow_streaming: false, roots: Some(Roots::new(read, write).unwrap()) })
}

fn outputs(s: &mut Server) -> Value {
    ok_json(&call(s, "output_status", json!({})))
}

fn source_count(s: &mut Server) -> usize {
    ok_json(&call(s, "doc_inspect", json!({})))["collection"]["sources"].as_array().unwrap().len()
}

fn text_of(p: &std::path::Path) -> String {
    p.to_string_lossy().into_owned()
}

#[test]
fn output_tools_carry_their_hints() {
    let mut s = server();
    let list = ask(&mut s, json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}));
    let tools = list["result"]["tools"].as_array().unwrap();
    let hints = |n: &str| tools.iter().find(|t| t["name"] == n).map(|t| t["annotations"].clone()).unwrap();
    for name in ["record_start", "record_stop"] {
        let h = hints(name);
        assert_eq!(
            (h["readOnlyHint"].as_bool(), h["destructiveHint"].as_bool(), h["openWorldHint"].as_bool()),
            (Some(false), Some(false), Some(false)),
            "{name}"
        );
    }
    for name in ["stream_start", "stream_stop"] {
        let h = hints(name);
        assert_eq!(
            (h["readOnlyHint"].as_bool(), h["destructiveHint"].as_bool(), h["openWorldHint"].as_bool()),
            (Some(false), Some(false), Some(true)),
            "{name}"
        );
    }
    assert_eq!(hints("output_status")["readOnlyHint"], true);
    assert_eq!(hints("command_run")["openWorldHint"], false);
}

#[test]
fn output_status_starts_with_nothing_running() {
    let mut s = server();
    let state = outputs(&mut s);
    assert_eq!((state["recording"]["active"].as_bool(), state["streaming"]["active"].as_bool()), (Some(false), Some(false)), "{state}");
    // Stopping what does not run changes nothing.
    assert_eq!(ok_json(&call(&mut s, "record_stop", json!({})))["recording"]["active"], false);
    assert_eq!(ok_json(&call(&mut s, "stream_stop", json!({})))["streaming"]["active"], false);
}

#[test]
fn record_start_without_ffmpeg_returns_the_studio_error_and_leaves_nothing_running() {
    let dir = temp_dir("no-ffmpeg");
    let mut h = Headless::new().unwrap();
    let mut profile = h.studio().profile().clone();
    profile.output.ffmpeg_path = text_of(&dir.join("no-ffmpeg-here"));
    profile.output.folder = text_of(&dir);
    h.studio_mut().set_profile(profile).unwrap();
    let mut s = Server::new(Box::new(h)).unwrap();
    let text = error_text(&call(&mut s, "record_start", json!({})));
    // Without a graphics device the studio stops before it looks for ffmpeg.
    let no_gpu = StudioError::Output("nothing can be recorded or streamed without the graphics device".into()).to_string();
    assert!(text == no_gpu || text.contains("no-ffmpeg-here"), "{text}");
    assert_eq!(outputs(&mut s)["recording"]["active"], false);
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0, "no file was made");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn stream_start_headless_is_refused_without_allow_streaming() {
    let mut s = server();
    let text = error_text(&call(&mut s, "stream_start", json!({})));
    assert_eq!(text, STREAMING_REFUSED);
    assert!(text.contains("--allow-streaming"), "{text}");
    // The same command through command_run, command_batch and the toggle is refused too.
    for id in ["stream.start", "stream.toggle"] {
        assert_eq!(error_text(&call(&mut s, "command_run", json!({"id": id}))), STREAMING_REFUSED, "{id}");
    }
    let report: Value = serde_json::from_str(&error_text(&call(&mut s, "command_batch", json!({"steps": [{"id": "stream.start"}]})))).unwrap();
    assert_eq!(report["steps"][0]["error"], STREAMING_REFUSED, "{report}");
    assert_eq!(outputs(&mut s)["streaming"]["active"], false);
    // Allowed, the call reaches the studio, which reports why it can't stream here.
    let mut s = headless_with(ServerOptions { allow_streaming: true, roots: None });
    let text = error_text(&call(&mut s, "stream_start", json!({})));
    assert!(!text.contains("--allow-streaming"), "{text}");
    assert_eq!(outputs(&mut s)["streaming"]["active"], false);
}

#[test]
fn describe_names_the_roots_and_the_streaming_permission() {
    let dir = temp_dir("describe");
    let h = Headless::with_options(None, ServerOptions { allow_streaming: false, roots: Some(Roots::new(std::slice::from_ref(&dir), &[]).unwrap()) })
        .unwrap();
    let text = h.describe();
    assert!(text.starts_with("headless") && text.contains("read roots") && text.contains("write roots none"), "{text}");
    assert!(text.contains("streaming is off"), "{text}");
    let h = Headless::with_options(None, ServerOptions { allow_streaming: true, roots: None }).unwrap();
    assert!(h.describe().ends_with("streaming is allowed"), "{}", h.describe());
    let _ = std::fs::remove_dir_all(&dir);
}

/// `in`, `out` and `outside` folders, with a scene collection file in `in` and in `outside`.
fn root_folders(tag: &str) -> std::path::PathBuf {
    let dir = temp_dir(tag);
    for d in ["in", "out", "outside"] {
        std::fs::create_dir_all(dir.join(d)).unwrap();
    }
    let mut s = server();
    ok_json(&call(&mut s, "scene_add", json!({"name": "Saved"})));
    for d in ["in", "outside"] {
        ok_json(&call(&mut s, "collection_save", json!({"path": text_of(&dir.join(d).join("show.json"))})));
    }
    dir
}

#[test]
fn collection_open_is_held_to_the_read_roots() {
    let dir = root_folders("open-roots");
    let mut s = confined(&[dir.join("in")], &[dir.join("out")]);
    let text = error_text(&call(&mut s, "collection_open", json!({"path": text_of(&dir.join("outside/show.json"))})));
    assert!(text.contains("outside the read roots"), "{text}");
    let escape = dir.join("in").join("..").join("outside").join("show.json");
    let text = error_text(&call(&mut s, "collection_open", json!({"path": text_of(&escape)})));
    assert!(text.contains("outside the read roots"), "{text}");
    let below_missing = dir.join("in").join("none").join("..").join("..").join("outside").join("show.json");
    let text = error_text(&call(&mut s, "collection_open", json!({"path": text_of(&below_missing)})));
    assert!(text.contains("does not exist"), "{text}");
    // A file in the write root can't be read either.
    std::fs::copy(dir.join("in/show.json"), dir.join("out/show.json")).unwrap();
    error_text(&call(&mut s, "collection_open", json!({"path": text_of(&dir.join("out/show.json"))})));
    assert!(!scene_names(&mut s).contains(&"Saved".to_owned()), "nothing was opened");
    ok_json(&call(&mut s, "collection_open", json!({"path": text_of(&dir.join("in/show.json"))})));
    assert!(scene_names(&mut s).contains(&"Saved".to_owned()));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn collection_save_is_held_to_the_write_roots() {
    let dir = root_folders("save-roots");
    let mut s = confined(&[dir.join("in")], &[dir.join("out")]);
    for path in [dir.join("outside/new.json"), dir.join("in/new.json"), dir.join("out").join("..").join("outside").join("new.json")] {
        let text = error_text(&call(&mut s, "collection_save", json!({"path": text_of(&path)})));
        assert!(text.contains("outside the write roots"), "{text}");
        assert!(!path.exists(), "{}", path.display());
    }
    ok_json(&call(&mut s, "collection_save", json!({"path": text_of(&dir.join("out/new.json"))})));
    assert!(dir.join("out/new.json").is_file());
    // Without a write root nothing is written.
    let mut s = confined(&[dir.join("in")], &[]);
    let text = error_text(&call(&mut s, "collection_save", json!({"path": text_of(&dir.join("out/other.json"))})));
    assert!(text.contains("--write-root"), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn source_files_are_held_to_the_read_roots() {
    let dir = root_folders("source-roots");
    std::fs::write(dir.join("in/logo.png"), b"not a picture").unwrap();
    std::fs::write(dir.join("outside/logo.png"), b"not a picture").unwrap();
    let mut s = confined(&[dir.join("in")], &[dir.join("out")]);
    let before = source_count(&mut s);
    let outside = text_of(&dir.join("outside/logo.png"));
    let text = error_text(&call(&mut s, "source_add", json!({"kind": "image", "settings": {"file": outside}})));
    assert!(text.contains("outside the read roots"), "{text}");
    let text = error_text(&call(&mut s, "command_run", json!({"id": "source.add", "params": {"kind": "media", "settings": {"file": outside}}})));
    assert!(text.contains("outside the read roots"), "{text}");
    let escape = text_of(&dir.join("in").join("..").join("outside").join("logo.png"));
    error_text(&call(&mut s, "command_batch", json!({"steps": [{"id": "source.add", "params": {"kind": "image", "settings": {"file": escape}}}]})));
    assert_eq!(source_count(&mut s), before, "nothing was added");
    // A file inside the root is fine, and so is a source without a file.
    let inside = text_of(&dir.join("in/logo.png"));
    ok_json(&call(&mut s, "source_add", json!({"kind": "image", "name": "Logo", "settings": {"file": inside}})));
    ok_json(&call(&mut s, "source_add", json!({"kind": "color", "name": "Back"})));
    assert_eq!(source_count(&mut s), before + 2);
    // Changing the file later is held to the roots too.
    let text =
        error_text(&call(&mut s, "command_run", json!({"id": "source.settings", "params": {"source": "Logo", "settings": {"file": outside}}})));
    assert!(text.contains("outside the read roots"), "{text}");
    let doc = ok_json(&call(&mut s, "doc_inspect", json!({"detail": "full"})));
    let logo = doc["collection"]["sources"].as_array().unwrap().iter().find(|x| x["name"] == "Logo").cloned().unwrap();
    assert_eq!(logo["settings"]["file"].as_str(), Some(inside.as_str()), "{logo}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_collection_naming_files_outside_the_read_roots_is_not_opened() {
    let dir = root_folders("collection-files");
    std::fs::write(dir.join("outside/logo.png"), b"not a picture").unwrap();
    let mut free = server();
    ok_json(&call(&mut free, "source_add", json!({"kind": "image", "name": "Logo", "settings": {"file": text_of(&dir.join("outside/logo.png"))}})));
    ok_json(&call(&mut free, "scene_add", json!({"name": "Leaky"})));
    ok_json(&call(&mut free, "collection_save", json!({"path": text_of(&dir.join("in/leaky.json"))})));
    let mut s = confined(&[dir.join("in")], &[]);
    let text = error_text(&call(&mut s, "collection_open", json!({"path": text_of(&dir.join("in/leaky.json"))})));
    assert!(text.contains("logo.png") && text.contains("outside the read roots"), "{text}");
    assert!(!scene_names(&mut s).contains(&"Leaky".to_owned()));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn record_start_is_held_to_the_write_roots() {
    let dir = root_folders("record-roots");
    let roots = Roots::new(&[dir.join("in")], &[dir.join("out")]).unwrap();
    let mut h = Headless::with_options(None, ServerOptions { allow_streaming: false, roots: Some(roots) }).unwrap();
    let mut profile = h.studio().profile().clone();
    profile.output.folder = text_of(&dir.join("outside"));
    h.studio_mut().set_profile(profile).unwrap();
    let mut s = Server::new(Box::new(h)).unwrap();
    for (tool, args) in [("record_start", json!({})), ("command_run", json!({"id": "record.toggle"}))] {
        let text = error_text(&call(&mut s, tool, args));
        assert!(text.contains("outside the write roots"), "{tool}: {text}");
    }
    assert_eq!(outputs(&mut s)["recording"]["active"], false);
    // Inside the write root the call reaches the studio, which has no graphics device here.
    let mut h =
        Headless::with_options(None, ServerOptions { allow_streaming: false, roots: Some(Roots::new(&[], &[dir.join("out")]).unwrap()) }).unwrap();
    let mut profile = h.studio().profile().clone();
    profile.output.folder = text_of(&dir.join("out"));
    h.studio_mut().set_profile(profile).unwrap();
    let mut s = Server::new(Box::new(h)).unwrap();
    let text = error_text(&call(&mut s, "record_start", json!({})));
    assert!(!text.contains("write roots"), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn replay_tools_report_the_replay_buffer() {
    let mut s = server();
    let state = outputs(&mut s);
    assert_eq!(state["replay"]["active"], false, "{state}");
    assert_eq!(state["replay"]["saving"], 0, "{state}");
    assert_eq!(ok_json(&call(&mut s, "replay_stop", json!({})))["replay"]["active"], false);
    let text = error_text(&call(&mut s, "replay_save", json!({})));
    assert_eq!(text, StudioError::Output("the replay buffer is not running; start it first".into()).to_string());
    // Without a graphics device the replay buffer does not start.
    let text = error_text(&call(&mut s, "replay_start", json!({})));
    assert_eq!(text, StudioError::Output("nothing can be recorded or streamed without the graphics device".into()).to_string());
    assert_eq!(outputs(&mut s)["replay"]["active"], false);
}

#[test]
fn replay_save_is_held_to_the_write_roots() {
    let dir = root_folders("replay-roots");
    let roots = Roots::new(&[dir.join("in")], &[dir.join("out")]).unwrap();
    let mut h = Headless::with_options(None, ServerOptions { allow_streaming: false, roots: Some(roots) }).unwrap();
    let mut profile = h.studio().profile().clone();
    profile.output.folder = text_of(&dir.join("outside"));
    h.studio_mut().set_profile(profile).unwrap();
    let mut s = Server::new(Box::new(h)).unwrap();
    for (tool, args) in [("replay_save", json!({})), ("command_run", json!({"id": "replay.save"}))] {
        let text = error_text(&call(&mut s, tool, args));
        assert!(text.contains("outside the write roots"), "{tool}: {text}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

fn output_window(method: &str) -> Value {
    match method {
        "document.inspect" => json!({
            "recording": {"active": true, "status": "running", "target": "/videos/a.mkv", "seconds": 1.5},
            "streaming": {"active": false},
        }),
        _ => json!({}),
    }
}

#[test]
fn bridge_output_tools_run_the_engine_commands_and_report_the_state() {
    let (mut s, seen) = fake_window(output_window);
    let reply = ok_json(&call(&mut s, "record_start", json!({})));
    assert_eq!(reply, json!({"recording": output_window("document.inspect")["recording"]}));
    let sent = seen.lock().unwrap().clone();
    assert_eq!(sent[0], ("engine.execute".to_owned(), json!({"command": "record.start", "params": {}})));
    assert_eq!(sent[1].0, "document.inspect");
    // The app streams as its menu item does; the server's own permission is for the headless studio.
    ok_json(&call(&mut s, "stream_start", json!({})));
    assert!(seen.lock().unwrap().iter().any(|(m, p)| m == "engine.execute" && p["command"] == "stream.start"));
    let state = ok_json(&call(&mut s, "output_status", json!({})));
    assert_eq!((state["recording"]["active"].as_bool(), state["streaming"]["active"].as_bool()), (Some(true), Some(false)), "{state}");
    assert_eq!(last_request(&seen).0, "document.inspect");
}
