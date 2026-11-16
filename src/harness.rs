//! `--screenshot` and `--self-test`: the window drawn for a while, then its picture saved, or a
//! scripted session (a mouse drag in the preview and a series of commands) written as JSON.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use craft_keymap::CommandRef;
use egui::{ColorImage, Modifiers, Pos2};
use serde_json::{Value, json};
use studiocraft_ui_egui::StudioApp;

use crate::args::Args;
use crate::input::{self, EventQueue};

/// Frames drawn before the screenshot is taken.
pub const SCREENSHOT_FRAMES: u64 = 120;
/// The longest either mode may run.
const LIMIT: Duration = Duration::from_secs(120);
/// How far the self-test drags the item, in canvas pixels.
const DRAG: (f32, f32) = (240.0, 120.0);

enum Mode {
    Screenshot { picture: Option<Receiver<Arc<ColorImage>>> },
    SelfTest { stage: u32, wait: u64, from: Option<(f32, f32)>, item: Option<u64> },
}

/// The result of one self-test step.
struct Outcome {
    passed: bool,
    detail: String,
}

pub struct Harness {
    mode: Mode,
    out: PathBuf,
    frames: u64,
    started: Instant,
    events: EventQueue,
    /// Every command and state snapshot of the self-test, in order.
    log: Vec<Value>,
    moved: Option<Outcome>,
    acted: Option<Outcome>,
    shown: Option<Outcome>,
    done: bool,
}

/// The self-test's commands after the drag.
const SCRIPT: &[(&str, &str)] = &[
    ("scene.add", r#"{"name": "Second"}"#),
    ("source.add", r#"{"kind": "text", "name": "Title", "settings": {"text": "StudioCraft", "size": 96}}"#),
    ("scene.select", r#"{"scene": "Scene"}"#),
    ("studio.toggle", "{}"),
    ("scene.select", r#"{"scene": "Second"}"#),
    ("studio.transition", r#"{"kind": "cut"}"#),
    ("studio.toggle", "{}"),
    ("source.volume", r#"{"source": "Microphone", "gain": 0.5}"#),
    ("edit.undo", "{}"),
    ("view.stats", "{}"),
];

impl Harness {
    pub fn from_args(a: &Args) -> Option<Harness> {
        let (mode, out) = if let Some(p) = &a.screenshot {
            (Mode::Screenshot { picture: None }, p.clone())
        } else {
            (Mode::SelfTest { stage: 0, wait: 0, from: None, item: None }, a.self_test.clone()?)
        };
        Some(Harness {
            mode,
            out,
            frames: 0,
            started: Instant::now(),
            events: EventQueue::default(),
            log: Vec::new(),
            moved: None,
            acted: None,
            shown: None,
            done: false,
        })
    }

    /// Add this frame's scripted mouse events.
    pub fn inject(&mut self, raw: &mut egui::RawInput) {
        self.events.inject(raw);
    }

    fn finish(&mut self, ctx: &egui::Context, result: Result<(), String>) {
        if self.done {
            return;
        }
        self.done = true;
        let error = result.err();
        if let Some(e) = &error {
            eprintln!("studiocraft: {e}");
        }
        if matches!(self.mode, Mode::SelfTest { .. }) {
            let not_run = || Outcome { passed: false, detail: format!("did not run: {}", error.as_deref().unwrap_or("the test stopped")) };
            let steps: Vec<Value> = [("move", self.moved.take()), ("act", self.acted.take()), ("hud", self.shown.take())]
                .into_iter()
                .map(|(step, o)| {
                    let o = o.unwrap_or_else(not_run);
                    json!({"step": step, "passed": o.passed, "detail": o.detail})
                })
                .collect();
            let doc = json!({"version": 1, "steps": steps, "error": error, "log": self.log});
            let text = serde_json::to_string_pretty(&doc).unwrap_or_default();
            if let Err(e) = std::fs::write(&self.out, text) {
                eprintln!("studiocraft: {}: {e}", self.out.display());
            }
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn act(&mut self, ctx: &egui::Context, app: &mut StudioApp, id: &str, params: Value) -> Result<Value, String> {
        let r = app.perform(ctx, &CommandRef::with_params(id, params.clone()));
        self.log.push(match &r {
            Ok(v) => json!({"command": id, "params": params, "ok": true, "result": v}),
            Err(e) => json!({"command": id, "params": params, "ok": false, "error": e}),
        });
        r
    }

    fn snapshot(&mut self, app: &StudioApp, label: &str) {
        self.log.push(json!({"snapshot": label, "frame": self.frames, "state": app.hud()}));
    }

    /// One frame of the script, before the window is drawn.
    pub fn step(&mut self, ctx: &egui::Context, app: &mut StudioApp) {
        if self.done {
            return;
        }
        self.frames += 1;
        if self.started.elapsed() > LIMIT {
            self.finish(ctx, Err(format!("stopped after {} s", LIMIT.as_secs())));
            return;
        }
        match &mut self.mode {
            Mode::Screenshot { picture } => {
                if self.frames == SCREENSHOT_FRAMES {
                    let (tx, rx) = channel();
                    ctx.request_screenshot(move |img| {
                        let _ = tx.send(img);
                    });
                    *picture = Some(rx);
                }
                let got = picture.as_ref().and_then(|rx| rx.try_recv().ok());
                if let Some(img) = got {
                    let r = save_png(&self.out, &img);
                    self.finish(ctx, r);
                }
            }
            Mode::SelfTest { .. } => self.self_test(ctx, app),
        }
    }

    fn set(&mut self, stage: u32, from: Option<(f32, f32)>, item: Option<u64>) {
        self.mode = Mode::SelfTest { stage, wait: 0, from, item };
    }

    fn self_test(&mut self, ctx: &egui::Context, app: &mut StudioApp) {
        let Mode::SelfTest { stage, wait, from, item } = &mut self.mode else { return };
        *wait += 1;
        let (stage, wait, from, item) = (*stage, *wait, *from, *item);
        match stage {
            // Let the compositor draw a few frames.
            0 if wait >= 30 && (app.hud()["ui"]["preview_drawn"] == json!(true) || wait >= 300) => {
                self.snapshot(app, "start");
                let r = self.act(ctx, app, "source.add", json!({"kind": "color", "name": "Box", "settings": {"color": "#3a7bd5ff", "width": 480, "height": 270}, "transform": {"position": {"x": 160.0, "y": 120.0}}}));
                self.set(1, None, r.ok().and_then(|v| v["item"].as_u64()));
            }
            // Drag the new item once its size is known and the preview has been laid out.
            1 => {
                let Some(id) = item else {
                    self.finish(ctx, Err("the color source could not be added".into()));
                    return;
                };
                let it = app.studio().collection().scenes[0].items.iter().find(|i| i.id.0 == id).map(|i| (i.source, i.transform));
                let Some((source, t)) = it else { return };
                let (Some(native), Some((rect, scale))) = (app.studio().source_size(source), app.preview_view()) else {
                    if wait > 600 {
                        self.finish(ctx, Err("the color source never showed".into()));
                    }
                    return;
                };
                let size = t.size(native);
                let center = (t.position.x + size.x * 0.5, t.position.y + size.y * 0.5);
                let at = |x: f32, y: f32| Pos2::new(rect.min.x + x * scale, rect.min.y + y * scale);
                let p0 = at(center.0, center.1);
                let p1 = at(center.0 + DRAG.0, center.1 + DRAG.1);
                self.set(2, Some((t.position.x, t.position.y)), item);
                if let Err(e) = self.events.push(input::drag(p0, p1, 12, Modifiers::NONE)) {
                    self.finish(ctx, Err(e));
                }
            }
            2 if self.events.is_empty() && wait > 3 => {
                let (fx, fy) = from.unwrap_or_default();
                let id = item.unwrap_or_default();
                let to = app.studio().collection().scenes[0]
                    .items
                    .iter()
                    .find(|i| i.id.0 == id)
                    .map(|i| (i.transform.position.x, i.transform.position.y))
                    .unwrap_or_default();
                // Snapping may pull the item up to 8 screen points.
                let tolerance = 8.0 / app.preview_view().map_or(1.0, |v| v.1) + 1.0;
                let (dx, dy) = (to.0 - fx, to.1 - fy);
                let passed = (dx - DRAG.0).abs() <= tolerance && (dy - DRAG.1).abs() <= tolerance;
                self.set(3, from, item);
                self.moved = Some(Outcome {
                    passed,
                    detail: format!(
                        "dragged the color source {} x {} canvas px in the preview with the mouse; it moved {dx:.1} x {dy:.1}",
                        DRAG.0, DRAG.1
                    ),
                });
            }
            3 => {
                self.set(4, from, item);
                let mut failed = Vec::new();
                for (id, params) in SCRIPT {
                    let params: Value = serde_json::from_str(params).unwrap_or(Value::Null);
                    if let Err(e) = self.act(ctx, app, id, params) {
                        failed.push(format!("{id}: {e}"));
                    }
                }
                let c = app.studio().collection();
                let program = c.scene(c.program_scene).map(|s| s.name.clone()).unwrap_or_default();
                let mic = c.sources.iter().find(|s| s.name == "Microphone").map(|s| s.audio.gain);
                let mut problems = failed;
                if c.scenes.len() != 2 {
                    problems.push(format!("expected 2 scenes, found {}", c.scenes.len()));
                }
                if program != "Second" {
                    problems.push(format!("the cut left \u{201c}{program}\u{201d} on program instead of \u{201c}Second\u{201d}"));
                }
                if mic != Some(1.0) {
                    problems.push(format!("undo left the microphone at {mic:?} instead of 1.0"));
                }
                let detail = if problems.is_empty() {
                    format!(
                        "ran {} commands: added a scene and a text source, cut to it in studio mode, changed and undid a volume, opened the stats",
                        SCRIPT.len()
                    )
                } else {
                    problems.join("; ")
                };
                self.acted = Some(Outcome { passed: problems.is_empty(), detail });
            }
            4 if wait > 20 => {
                self.snapshot(app, "end");
                let hud = app.hud();
                let frames = hud["stats"]["frames"].as_u64().unwrap_or(0);
                let checks = [
                    (hud["ui"]["preview_drawn"] == json!(true), "the preview shows the compositor's picture"),
                    (hud["ui"]["status_bar"] == json!(true), "the status bar shows frame statistics"),
                    (hud["ui"]["stats"] == json!(true), "the stats window is open"),
                    (frames > 0, "the compositor has drawn frames"),
                    (hud["ui"]["docks"]["mixer"] == json!(true), "the audio mixer dock is shown"),
                ];
                let missing: Vec<&str> = checks.iter().filter(|(ok, _)| !ok).map(|(_, what)| *what).collect();
                let detail = if missing.is_empty() {
                    format!("{}; {frames} frames drawn", checks.iter().map(|(_, what)| *what).collect::<Vec<_>>().join(", "))
                } else {
                    format!("not true: {}", missing.join(", "))
                };
                self.shown = Some(Outcome { passed: missing.is_empty(), detail });
                self.finish(ctx, Ok(()));
            }
            _ => {}
        }
    }
}

fn save_png(path: &std::path::Path, img: &ColorImage) -> Result<(), String> {
    let [w, h] = img.size;
    let bytes: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_srgba_unmultiplied()).collect();
    image::save_buffer_with_format(path, &bytes, w as u32, h as u32, image::ExtendedColorType::Rgba8, image::ImageFormat::Png)
        .map_err(|e| format!("{}: {e}", path.display()))
}
