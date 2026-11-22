//! The local control channel: JSON-lines requests on `127.0.0.1`, answered on the window's
//! thread. `docs/control-protocol.md` lists the methods.
//!
//! - `engine.execute`, `engine.commands`, `document.inspect` and `render.preview` run commands and
//!   read the scene collection; `run`, `commands` and `state` are the older names.
//! - `ui.inspect`, `ui.menu.list` and `ui.menu.invoke` read the window and run its menu items.
//! - `ui.click`, `ui.drag`, `ui.key` and `ui.type` queue input for the next frames, which
//!   `Control::inject` adds to the window's input.
//! - `ui.dialog` answers the open name prompt or exit confirmation as its buttons do.
//! - `ui.screenshot` answers once the window has delivered its picture; `app.quit` closes the
//!   window as File › Exit does.

use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, channel, sync_channel};
use std::time::{Duration, Instant};

use base64::Engine as _;
use craft_keymap::{Binding, CommandRef, Mods};
use egui::{ColorImage, PointerButton, Pos2};
use serde_json::{Map, Value, json};
use studiocraft_studio::capture::{MAX_PNG_SIDE, png, scaled_size};
use studiocraft_studio::{Capture, View};
use studiocraft_ui_egui::StudioApp;

use crate::input::{self, EventQueue};

/// The environment variable holding the session token.
pub const TOKEN_ENV: &str = "STUDIOCRAFT_CONTROL_TOKEN";
/// Longest request line accepted.
pub const MAX_LINE: usize = 1 << 20;
/// Connections served at once, and screenshots waiting at once.
pub const MAX_CONNECTIONS: usize = 8;
/// Longest text `ui.type` accepts, in characters.
pub const MAX_TYPED: usize = 10_000;
/// Most moves a `ui.drag` makes.
pub const MAX_DRAG_STEPS: u64 = 200;
/// Moves a `ui.drag` makes when `steps` is left out.
const DRAG_STEPS: u64 = 12;
/// Most presses a `ui.click` makes.
pub const MAX_CLICKS: u64 = 3;
/// How long a connection may stay idle.
const IDLE: Duration = Duration::from_secs(300);
/// How long a request waits for the window.
const ANSWER: Duration = Duration::from_secs(30);
/// How long `render.preview` waits for the compositor's picture.
const PREVIEW_WAIT: Duration = Duration::from_secs(5);

type Params = Map<String, Value>;

struct Request {
    method: String,
    params: Value,
    reply: SyncSender<Value>,
}

pub struct Control {
    requests: Receiver<Request>,
    bridge: Bridge,
}

fn ok(result: Value) -> Value {
    json!({"ok": true, "result": result})
}

fn err(e: impl std::fmt::Display) -> Value {
    json!({"ok": false, "error": e.to_string()})
}

fn answer(r: Result<Value, String>) -> Value {
    r.map_or_else(err, ok)
}

impl Control {
    /// Listen on `127.0.0.1:port`.
    pub fn start(port: u16) -> Result<Control, String> {
        let token = std::env::var(TOKEN_ENV).map_err(|_| format!("--control needs the session token in {TOKEN_ENV}"))?;
        if !craft_control_protocol::valid_token(&token) {
            return Err(format!("{TOKEN_ENV} must be 64 hexadecimal characters"));
        }
        let listener = TcpListener::bind(("127.0.0.1", port)).map_err(|e| format!("port {port} can't be used for control: {e}"))?;
        let (tx, requests) = sync_channel::<Request>(64);
        let open = Arc::new(AtomicUsize::new(0));
        let token: Arc<str> = token.into();
        std::thread::Builder::new()
            .name("control".into())
            .spawn(move || {
                for stream in listener.incoming().flatten() {
                    if open.fetch_add(1, Ordering::AcqRel) >= MAX_CONNECTIONS {
                        open.fetch_sub(1, Ordering::AcqRel);
                        continue;
                    }
                    let (tx, token, slot) = (tx.clone(), Arc::clone(&token), Arc::clone(&open));
                    let spawned = std::thread::Builder::new().name("control connection".into()).spawn(move || {
                        let _ = stream.set_read_timeout(Some(IDLE));
                        if let Ok(out) = stream.try_clone() {
                            craft_control_protocol::serve_lines(&stream, out, &token, MAX_LINE, |method, params| {
                                let (reply, answer) = sync_channel(1);
                                tx.send(Request { method, params, reply }).ok()?;
                                Some(answer.recv_timeout(ANSWER).unwrap_or_else(|_| err("the app did not answer in time")))
                            });
                        }
                        slot.fetch_sub(1, Ordering::AcqRel);
                    });
                    if spawned.is_err() {
                        // The connection did not start, so its slot is free again.
                        open.fetch_sub(1, Ordering::AcqRel);
                        log::warn!("no thread for a control connection");
                    }
                }
            })
            .map_err(|e| format!("the control thread could not start: {e}"))?;
        log::info!("control requests are accepted on 127.0.0.1:{port}");
        Ok(Control { requests, bridge: Bridge::default() })
    }

    /// Answer the requests that arrived since the last frame, and the screenshots whose picture
    /// arrived.
    pub fn serve(&mut self, ctx: &egui::Context, app: &mut StudioApp) {
        while let Ok(r) = self.requests.try_recv() {
            if let Some(reply) = self.bridge.handle(ctx, app, &r.method, &r.params, &r.reply) {
                let _ = r.reply.send(reply);
            }
        }
        self.bridge.collect_screenshots(Instant::now());
    }

    /// Add the next frame's queued input.
    pub fn inject(&mut self, raw: &mut egui::RawInput) {
        self.bridge.inject(raw);
    }
}

/// A `ui.screenshot` waiting for the window's picture.
struct PendingShot {
    picture: Receiver<Arc<ColorImage>>,
    reply: SyncSender<Value>,
    max_side: u32,
    deadline: Instant,
}

/// The control channel's state between frames: queued input and waiting screenshots.
#[derive(Default)]
pub struct Bridge {
    events: EventQueue,
    shots: Vec<PendingShot>,
}

/// The request's parameters as an object; a missing one is empty.
fn object(params: &Value) -> Result<&Params, String> {
    static EMPTY: std::sync::OnceLock<Params> = std::sync::OnceLock::new();
    match params {
        Value::Object(m) => Ok(m),
        Value::Null => Ok(EMPTY.get_or_init(Map::new)),
        _ => Err("`params` must be an object".into()),
    }
}

fn opt_str<'a>(p: &'a Params, key: &str) -> Result<Option<&'a str>, String> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(format!("`{key}` must be a string")),
    }
}

fn req_str<'a>(p: &'a Params, key: &str) -> Result<&'a str, String> {
    opt_str(p, key)?.ok_or_else(|| format!("`{key}` is needed"))
}

/// A whole number from `min` to `max`, or `default` when left out.
fn opt_count(p: &Params, key: &str, default: u64, min: u64, max: u64) -> Result<u64, String> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v.as_u64().filter(|n| (min..=max).contains(n)).ok_or_else(|| format!("`{key}` must be a whole number from {min} to {max}")),
    }
}

/// `max_side`: the longest side of the returned picture, 0 for full size.
fn max_side(p: &Params) -> Result<u32, String> {
    let n = opt_count(p, "max_side", 0, 0, u64::from(MAX_PNG_SIDE))?;
    u32::try_from(n).map_err(|e| e.to_string())
}

fn coord(v: &Params, key: &str, name: &str) -> Result<f32, String> {
    v.get(key).and_then(Value::as_f64).filter(|f| f.is_finite()).map(|f| f as f32).ok_or_else(|| format!("`{name}` needs a number `{key}`"))
}

/// A point in window points, which must lie inside the window.
fn point(ctx: &egui::Context, v: &Params, name: &str) -> Result<Pos2, String> {
    let pos = Pos2::new(coord(v, "x", name)?, coord(v, "y", name)?);
    let window = ctx.viewport_rect();
    if !window.contains(pos) {
        return Err(format!("({}, {}) is outside the window, which is {} x {} points", pos.x, pos.y, window.width(), window.height()));
    }
    Ok(pos)
}

fn nested_point(ctx: &egui::Context, p: &Params, key: &str) -> Result<Pos2, String> {
    match p.get(key) {
        Some(Value::Object(m)) => point(ctx, m, key),
        _ => Err(format!("`{key}` must be an object with `x` and `y`")),
    }
}

/// `modifiers`: a list of `cmd`, `ctrl`, `alt` and `shift`.
fn modifiers(p: &Params) -> Result<egui::Modifiers, String> {
    let mut m = Mods::NONE;
    match p.get("modifiers") {
        None | Some(Value::Null) => {}
        Some(Value::Array(list)) => {
            for v in list {
                match v.as_str() {
                    Some("cmd") => m.cmd = true,
                    Some("ctrl") => m.ctrl = true,
                    Some("alt") => m.alt = true,
                    Some("shift") => m.shift = true,
                    _ => return Err(format!("`modifiers` takes cmd, ctrl, alt and shift, not {v}")),
                }
            }
        }
        Some(_) => return Err("`modifiers` must be a list such as [\"cmd\", \"shift\"]".into()),
    }
    Ok(input::modifiers(m))
}

fn button(p: &Params) -> Result<PointerButton, String> {
    match opt_str(p, "button")? {
        None | Some("primary") => Ok(PointerButton::Primary),
        Some("secondary") => Ok(PointerButton::Secondary),
        Some("middle") => Ok(PointerButton::Middle),
        Some(other) => Err(format!("`button` is primary, secondary or middle, not `{other}`")),
    }
}

/// `document.inspect`'s summary: the state without source, filter and transition settings.
fn summary(mut state: Value) -> Value {
    if let Some(sources) = state.pointer_mut("/collection/sources").and_then(Value::as_array_mut) {
        for s in sources.iter_mut().filter_map(Value::as_object_mut) {
            s.remove("settings");
            if let Some(filters) = s.get_mut("filters").and_then(Value::as_array_mut) {
                for f in filters.iter_mut().filter_map(Value::as_object_mut) {
                    f.remove("settings");
                }
            }
        }
    }
    if let Some(t) = state.pointer_mut("/collection/transition").and_then(Value::as_object_mut) {
        t.remove("settings");
    }
    state
}

/// A picture as `{width, height, png_base64}`, its longer side at most `max_side` (0 for full
/// size, at most `MAX_PNG_SIDE` either way).
fn encoded(capture: &Capture, max_side: u32) -> Result<Value, String> {
    let bytes = png(capture, max_side).map_err(|e| e.to_string())?;
    let (width, height) = scaled_size(capture.width, capture.height, max_side);
    Ok(json!({"width": width, "height": height, "png_base64": base64::engine::general_purpose::STANDARD.encode(bytes)}))
}

fn window_picture(img: &ColorImage, max_side: u32) -> Result<Value, String> {
    let [w, h] = img.size;
    let width = u32::try_from(w).map_err(|e| e.to_string())?;
    let height = u32::try_from(h).map_err(|e| e.to_string())?;
    let rgba = img.pixels.iter().flat_map(|c| c.to_srgba_unmultiplied()).collect();
    encoded(&Capture { width, height, rgba }, max_side)
}

impl Bridge {
    /// Add the next frame's queued input.
    pub fn inject(&mut self, raw: &mut egui::RawInput) {
        self.events.inject(raw);
    }

    /// Answer one request, or `None` when the answer goes to `reply` later (`ui.screenshot`).
    pub fn handle(&mut self, ctx: &egui::Context, app: &mut StudioApp, method: &str, params: &Value, reply: &SyncSender<Value>) -> Option<Value> {
        let p = match object(params) {
            Ok(p) => p,
            Err(e) => return Some(err(e)),
        };
        if method == "ui.screenshot" {
            return self.screenshot(ctx, p, reply).err().map(err);
        }
        Some(answer(self.answer(ctx, app, method, p)))
    }

    fn answer(&mut self, ctx: &egui::Context, app: &mut StudioApp, method: &str, p: &Params) -> Result<Value, String> {
        match method {
            "state" => Ok(app.hud()),
            "engine.commands" | "commands" => Ok(Value::Array(app.command_list(opt_str(p, "filter")?))),
            "engine.execute" | "run" => {
                let id = req_str(p, "command")?;
                let params = match p.get("params") {
                    None | Some(Value::Null) => Value::Null,
                    Some(v @ Value::Object(_)) => v.clone(),
                    Some(_) => return Err("the command's `params` must be an object".into()),
                };
                app.perform(ctx, &CommandRef::with_params(id, params))
            }
            "document.inspect" => match opt_str(p, "detail")? {
                None | Some("summary") => Ok(summary(app.studio().state_json())),
                Some("full") => Ok(app.studio().state_json()),
                Some(other) => Err(format!("`detail` is summary or full, not `{other}`")),
            },
            "render.preview" => {
                let view = match opt_str(p, "view")? {
                    None | Some("program") => View::Program,
                    Some("preview") => View::Preview,
                    Some(other) => return Err(format!("`view` is program or preview, not `{other}`")),
                };
                let side = max_side(p)?;
                let capture = app.studio().capture(view, PREVIEW_WAIT).map_err(|e| e.to_string())?;
                encoded(&capture, side)
            }
            "ui.inspect" => {
                let mut v = app.ui_inspect(ctx);
                v["input_waiting"] = json!(!self.events.is_empty());
                Ok(v)
            }
            "ui.menu.list" => Ok(app.menu_tree()),
            "ui.menu.invoke" => app.invoke_menu(ctx, req_str(p, "path")?),
            "ui.click" => {
                let pos = point(ctx, p, "ui.click")?;
                let (button, mods) = (button(p)?, modifiers(p)?);
                let count = u32::try_from(opt_count(p, "count", 1, 1, MAX_CLICKS)?).map_err(|e| e.to_string())?;
                self.events.push(input::click(pos, button, count, mods))?;
                Ok(json!({"queued": true}))
            }
            "ui.drag" => {
                let (from, to) = (nested_point(ctx, p, "from")?, nested_point(ctx, p, "to")?);
                let steps = u32::try_from(opt_count(p, "steps", DRAG_STEPS, 1, MAX_DRAG_STEPS)?).map_err(|e| e.to_string())?;
                self.events.push(input::drag(from, to, steps, modifiers(p)?))?;
                Ok(json!({"queued": true}))
            }
            "ui.key" => {
                let text = req_str(p, "key")?;
                let binding = Binding::parse(text).map_err(|e| format!("`{text}` is not a key: {e}"))?;
                if binding.scope.is_some() {
                    return Err(format!("`{text}` names a scope; give only the key, such as Cmd+Z"));
                }
                self.events.push(input::key(binding.trigger))?;
                Ok(json!({"queued": true}))
            }
            "ui.type" => {
                let text = req_str(p, "text")?;
                if text.chars().count() > MAX_TYPED {
                    return Err(format!("`ui.type` takes at most {MAX_TYPED} characters"));
                }
                self.events.push(input::text(text))?;
                Ok(json!({"queued": true}))
            }
            "ui.dialog" => {
                let ok = match req_str(p, "answer")? {
                    "ok" => true,
                    "cancel" => false,
                    other => return Err(format!("`answer` is ok or cancel, not `{other}`")),
                };
                app.answer_dialog(ctx, ok, opt_str(p, "text")?)
            }
            "app.quit" => {
                app.perform(ctx, &CommandRef::new("app.exit"))?;
                Ok(json!({"confirming": app.confirming_exit()}))
            }
            other => Err(format!(
                "unknown method `{other}`; use engine.execute, engine.commands, document.inspect, render.preview, ui.inspect, \
                 ui.menu.list, ui.menu.invoke, ui.click, ui.drag, ui.key, ui.type, ui.dialog, ui.screenshot or app.quit"
            )),
        }
    }

    /// Ask the window for its picture; the answer goes to `reply` once it arrives.
    fn screenshot(&mut self, ctx: &egui::Context, p: &Params, reply: &SyncSender<Value>) -> Result<(), String> {
        let max_side = max_side(p)?;
        if self.shots.len() >= MAX_CONNECTIONS {
            return Err(format!("at most {MAX_CONNECTIONS} screenshots can wait at once"));
        }
        let (tx, picture) = channel();
        ctx.request_screenshot(move |img| {
            let _ = tx.send(img);
        });
        self.shots.push(PendingShot { picture, reply: reply.clone(), max_side, deadline: Instant::now() + ANSWER });
        ctx.request_repaint();
        Ok(())
    }

    /// Answer the screenshots whose picture arrived, and the ones that waited past their time.
    pub fn collect_screenshots(&mut self, now: Instant) {
        self.shots.retain(|s| {
            let reply = match s.picture.try_recv() {
                Ok(img) => answer(window_picture(&img, s.max_side)),
                Err(TryRecvError::Disconnected) => err("the window did not deliver its picture"),
                Err(TryRecvError::Empty) if now >= s.deadline => err(format!("no picture arrived within {} s", ANSWER.as_secs())),
                Err(TryRecvError::Empty) => return true,
            };
            let _ = s.reply.send(reply);
            false
        });
    }
}

#[cfg(test)]
mod tests {
    use egui::{Color32, RawInput, ViewportCommand, ViewportId};
    use studiocraft_studio::StudioError;
    use studiocraft_ui_egui::store::Store;

    use super::*;

    struct Rig {
        ctx: egui::Context,
        app: StudioApp,
        bridge: Bridge,
        reply: SyncSender<Value>,
        answers: Receiver<Value>,
    }

    fn raw() -> RawInput {
        RawInput { screen_rect: Some(egui::Rect::from_min_size(Pos2::ZERO, egui::vec2(1280.0, 800.0))), ..RawInput::default() }
    }

    impl Rig {
        fn new() -> Rig {
            let (reply, answers) = sync_channel(8);
            let app = StudioApp::boot(Store::memory(), None, Vec::new());
            let ctx = egui::Context::default();
            if let Err(e) = studiocraft_ui_egui::install_runtime(&ctx, None) {
                panic!("the UI runtime did not install: {e:?}");
            }
            let mut r = Rig { ctx, app, bridge: Bridge::default(), reply, answers };
            r.frame();
            r
        }

        fn call(&mut self, method: &str, params: Value) -> Value {
            let v = self.bridge.handle(&self.ctx, &mut self.app, method, &params, &self.reply);
            v.unwrap_or_else(|| panic!("{method} answered later"))
        }

        /// One frame, with the next batch of queued input, as the window draws it.
        fn frame(&mut self) {
            let mut input = raw();
            self.bridge.inject(&mut input);
            let app = &mut self.app;
            app.before_input(&self.ctx);
            let mut out = self.ctx.run_ui(input, |ui| app.frame(ui));
            out.textures_delta.clear();
        }
    }

    fn ok_result(v: &Value) -> &Value {
        assert_eq!(v["ok"], json!(true), "{v}");
        &v["result"]
    }

    fn error_text(v: &Value) -> String {
        assert_eq!(v["ok"], json!(false), "{v}");
        v["error"].as_str().unwrap_or_default().to_string()
    }

    #[test]
    fn the_older_methods_still_answer() {
        let mut r = Rig::new();
        ok_result(&r.call("state", json!({})));
        let list = r.call("commands", json!({}));
        assert!(ok_result(&list).as_array().is_some_and(|l| l.iter().any(|c| c["id"] == "record.toggle")));
        ok_result(&r.call("run", json!({"command": "scene.add", "params": {"name": "From control"}})));
        assert!(r.app.studio().collection().scene_by_name("From control").is_some());
        error_text(&r.call("run", json!({})));
        error_text(&r.call("run", json!({"command": "scene.add", "params": 5})));
        error_text(&r.call("launch", json!({})));
        error_text(&r.call("state", json!([1])));
    }

    #[test]
    fn engine_commands_lists_both_scopes() {
        let mut r = Rig::new();
        let v = r.call("engine.commands", json!({}));
        let list = ok_result(&v).as_array().cloned().unwrap_or_default();
        let find = |id: &str| list.iter().find(|c| c["id"] == id).cloned().unwrap_or(Value::Null);
        let add = find("scene.add");
        assert_eq!((add["scope"].as_str(), add["undo"].as_bool(), add["enabled"].as_bool()), (Some("engine"), Some(true), Some(true)));
        let import = find("collection.import");
        assert_eq!((import["scope"].as_str(), import["params"].as_str()), (Some("window"), Some("path?: string")));
        assert_eq!(find("help.github")["scope"], json!("window"));
        assert_eq!(find("edit.undo")["shortcut"], json!("Cmd+Z"));
        assert_eq!(find("edit.undo")["enabled"], json!(false));
        let v = r.call("engine.commands", json!({"filter": "COLLECTION"}));
        let filtered = ok_result(&v).as_array().cloned().unwrap_or_default();
        assert!(!filtered.is_empty() && filtered.iter().all(|c| c["id"].as_str().is_some_and(|id| id.starts_with("collection."))));
        error_text(&r.call("engine.commands", json!({"filter": 3})));
        ok_result(&r.call("engine.execute", json!({"command": "scene.add", "params": {"name": "Second"}})));
        assert_eq!(r.app.studio().collection().scenes.len(), 2);
    }

    #[test]
    fn document_inspect_summary_leaves_out_settings() {
        let mut r = Rig::new();
        ok_result(
            &r.call("engine.execute", json!({"command": "source.add", "params": {"kind": "text", "name": "Title", "settings": {"text": "Hi"}}})),
        );
        let full = r.call("document.inspect", json!({"detail": "full"}));
        let summary = r.call("document.inspect", json!({}));
        let title = |v: &Value| ok_result(v)["collection"]["sources"].as_array().and_then(|l| l.iter().find(|s| s["name"] == "Title").cloned());
        assert_eq!(title(&full).map(|s| s["settings"]["text"].clone()), Some(json!("Hi")));
        assert!(title(&summary).is_some_and(|s| s.get("settings").is_none()));
        assert!(ok_result(&summary)["collection"]["transition"].get("settings").is_none());
        error_text(&r.call("document.inspect", json!({"detail": "everything"})));
        error_text(&r.call("document.inspect", json!({"detail": true})));
    }

    #[test]
    fn ui_menu_invoke_runs_the_item() {
        let mut r = Rig::new();
        let tree = r.call("ui.menu.list", json!({}));
        assert!(ok_result(&tree).as_array().is_some_and(|m| m.iter().any(|m| m["label"] == "Scene Collection")));
        let v = r.call("ui.menu.invoke", json!({"path": "Scene Collection > New…"}));
        assert_eq!(ok_result(&v)["command"], json!("collection.new"));
        let ui = r.call("ui.inspect", json!({}));
        assert_eq!(ok_result(&ui)["dialogs"]["prompt"]["title"], json!("New Scene Collection"));
        assert!(error_text(&r.call("ui.menu.invoke", json!({"path": "Scene Collection > Fly"}))).contains("no menu item"));
        assert!(error_text(&r.call("ui.menu.invoke", json!({"path": "Edit > Undo"}))).contains("disabled"));
        assert!(error_text(&r.call("ui.menu.invoke", json!({"path": "Edit > Transform"}))).contains("submenu"));
        error_text(&r.call("ui.menu.invoke", json!({"path": 7})));
    }

    #[test]
    fn ui_inspect_reports_the_layout() {
        let mut r = Rig::new();
        r.frame();
        let v = r.call("ui.inspect", json!({}));
        let ui = ok_result(&v);
        assert_eq!(ui["window"], json!({"width": 1280.0, "height": 800.0}));
        assert!(ui["docks"].as_array().is_some_and(|d| d.iter().any(|d| d["dock"] == "mixer")));
        assert!(ui["preview"]["scale"].as_f64().is_some_and(|s| s > 0.0));
        assert!(ui["menus"].as_array().is_some_and(|m| m.iter().any(|t| t == "Scene Collection")));
    }

    #[test]
    fn ui_key_undoes_once_the_events_reach_a_frame() {
        let mut r = Rig::new();
        for name in ["A", "B"] {
            ok_result(&r.call("engine.execute", json!({"command": "scene.add", "params": {"name": name}})));
            r.frame();
        }
        assert_eq!(r.app.studio().collection().scenes.len(), 3);
        ok_result(&r.call("ui.key", json!({"key": "Cmd+Z"})));
        assert_eq!(r.app.studio().collection().scenes.len(), 3, "nothing happens before a frame");
        r.frame();
        r.frame();
        let c = r.app.studio().collection();
        assert_eq!(c.scenes.len(), 2);
        assert!(c.scene_by_name("A").is_some() && c.scene_by_name("B").is_none());
        error_text(&r.call("ui.key", json!({"key": "Cmd+Nope"})));
        error_text(&r.call("ui.key", json!({"key": 26})));
    }

    #[test]
    fn ui_click_and_drag_stay_inside_the_window() {
        let mut r = Rig::new();
        assert!(error_text(&r.call("ui.click", json!({"x": 2000.0, "y": 10.0}))).contains("outside the window"));
        assert!(error_text(&r.call("ui.click", json!({"x": 10.0, "y": -1.0}))).contains("outside the window"));
        error_text(&r.call("ui.click", json!({"x": "10", "y": 10.0})));
        error_text(&r.call("ui.click", json!({"x": 10.0, "y": 10.0, "count": 4})));
        error_text(&r.call("ui.click", json!({"x": 10.0, "y": 10.0, "button": "left"})));
        error_text(&r.call("ui.click", json!({"x": 10.0, "y": 10.0, "modifiers": ["hyper"]})));
        ok_result(&r.call("ui.click", json!({"x": 10.0, "y": 10.0, "button": "secondary", "modifiers": ["shift"]})));
        error_text(&r.call("ui.drag", json!({"from": {"x": 10.0, "y": 10.0}, "to": {"x": 10.0, "y": 900.0}})));
        error_text(&r.call("ui.drag", json!({"from": {"x": 10.0, "y": 10.0}, "to": {"x": 20.0, "y": 20.0}, "steps": 0})));
        ok_result(&r.call("ui.drag", json!({"from": {"x": 10.0, "y": 10.0}, "to": {"x": 20.0, "y": 20.0}, "steps": 3})));
        assert_eq!(ok_result(&r.call("ui.inspect", json!({})))["input_waiting"], json!(true));
    }

    #[test]
    fn ui_type_refuses_long_text() {
        let mut r = Rig::new();
        error_text(&r.call("ui.type", json!({"text": "x".repeat(MAX_TYPED + 1)})));
        error_text(&r.call("ui.type", json!({"text": null})));
        ok_result(&r.call("ui.type", json!({"text": "é".repeat(MAX_TYPED)})));
    }

    #[test]
    fn ui_screenshot_answers_when_the_picture_arrives() {
        let mut r = Rig::new();
        error_text(&r.call("ui.screenshot", json!({"max_side": -1})));
        let (ctx, reply) = (r.ctx.clone(), r.reply.clone());
        let mut later = None;
        let (bridge, app) = (&mut r.bridge, &mut r.app);
        app.before_input(&ctx);
        let out = ctx.run_ui(raw(), |ui| {
            later = Some(bridge.handle(&ctx, app, "ui.screenshot", &json!({"max_side": 2}), &reply));
            app.frame(ui);
        });
        assert_eq!(later, Some(None), "the answer waits for the picture");
        r.bridge.collect_screenshots(Instant::now());
        assert!(r.answers.try_recv().is_err(), "no picture yet");
        let mut delivered = 0;
        if let Some(vp) = out.viewport_output.get(&ViewportId::ROOT) {
            for c in &vp.commands {
                if let ViewportCommand::Screenshot(cb) = c {
                    cb.clone().complete(Arc::new(ColorImage::filled([4, 2], Color32::RED)));
                    delivered += 1;
                }
            }
        }
        assert_eq!(delivered, 1);
        r.bridge.collect_screenshots(Instant::now());
        let v = r.answers.try_recv().unwrap_or_default();
        let shot = ok_result(&v);
        assert_eq!((shot["width"].as_u64(), shot["height"].as_u64()), (Some(2), Some(1)));
        assert!(shot["png_base64"].as_str().is_some_and(|s| s.starts_with("iVBORw0KGgo")));
    }

    #[test]
    fn render_preview_without_a_gpu_says_so() {
        let mut r = Rig::new();
        assert_eq!(error_text(&r.call("render.preview", json!({}))), StudioError::NoGpu.to_string());
        error_text(&r.call("render.preview", json!({"view": "side"})));
        error_text(&r.call("render.preview", json!({"max_side": 5000})));
    }

    #[test]
    fn ui_dialog_answers_the_name_prompt() {
        let mut r = Rig::new();
        assert!(error_text(&r.call("ui.dialog", json!({"answer": "ok"}))).contains("no dialog"));
        ok_result(&r.call("ui.menu.invoke", json!({"path": "Scene Collection > New…"})));
        error_text(&r.call("ui.dialog", json!({})));
        assert!(error_text(&r.call("ui.dialog", json!({"answer": "yes"}))).contains("ok or cancel"));
        error_text(&r.call("ui.dialog", json!({"answer": "ok", "text": 5})));
        let v = r.call("ui.dialog", json!({"answer": "ok", "text": "Show B"}));
        assert_eq!(ok_result(&v)["name"], json!("Show B"));
        assert_eq!(r.app.studio().collection().name, "Show B");
        let ui = r.call("ui.inspect", json!({}));
        assert_eq!(ok_result(&ui)["dialogs"]["prompt"], Value::Null);
        // A name already taken leaves the prompt open with the error; Cancel closes it.
        ok_result(&r.call("ui.menu.invoke", json!({"path": "Scene Collection > New…"})));
        assert!(error_text(&r.call("ui.dialog", json!({"answer": "ok", "text": "show b"}))).contains("already"));
        let ui = r.call("ui.inspect", json!({}));
        assert!(ok_result(&ui)["dialogs"]["prompt"]["error"].as_str().is_some_and(|e| e.contains("already")));
        assert_eq!(ok_result(&r.call("ui.dialog", json!({"answer": "cancel"})))["closed"], json!(true));
        let ui = r.call("ui.inspect", json!({}));
        assert_eq!(ok_result(&ui)["dialogs"]["prompt"], Value::Null);
        assert_eq!(r.app.studio().collection().name, "Show B");
    }

    #[test]
    fn app_quit_closes_when_nothing_runs() {
        let mut r = Rig::new();
        assert_eq!(ok_result(&r.call("app.quit", json!({})))["confirming"], json!(false));
    }
}
