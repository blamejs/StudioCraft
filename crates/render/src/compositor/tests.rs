//! These tests need a graphics adapter; on a machine without one they pass without checking.

use std::sync::atomic::AtomicU64;
use std::sync::{Arc, OnceLock};

use studiocraft_media::Tick;
use studiocraft_media::queue::Fifo;
use studiocraft_media::video::PixelFormat;
use studiocraft_model::{Crop, SourceId, Transform, Vec2};
use studiocraft_plugin::{Plugin, Transition, VideoContent, VideoEffect, VideoOut};

use super::*;
use crate::transitions::Fade;
use crate::{EncodeTarget, ItemDesc, Plan, SceneDesc, TransitionDesc};

/// One device for every test in this binary; tests run in parallel and opening many devices at
/// once crashes some drivers.
fn gpu() -> Option<Gpu> {
    static GPU: OnceLock<Option<Gpu>> = OnceLock::new();
    GPU.get_or_init(|| Gpu::headless().inspect_err(|e| eprintln!("skipped: {e}")).ok()).clone()
}

fn compositor(w: u32, h: u32) -> Option<Compositor> {
    Some(Compositor::new(gpu()?, w, h).unwrap())
}

fn solid(rgba: [u8; 4], w: u32, h: u32) -> Arc<VideoOut> {
    let v = Arc::new(VideoOut::default());
    v.publish(VideoContent::Solid { rgba, width: w, height: h });
    v
}

fn frame(pixels: &[[u8; 4]], w: u32, h: u32) -> Arc<VideoOut> {
    let v = Arc::new(VideoOut::default());
    let mut f = v.frame();
    assert!(f.reshape(w, h, PixelFormat::Rgba8));
    for (dst, src) in f.data.as_chunks_mut::<4>().0.iter_mut().zip(pixels) {
        dst.copy_from_slice(src);
    }
    v.publish(VideoContent::Frame(f));
    v
}

fn item(id: u64, video: &Arc<VideoOut>, transform: Transform) -> ItemDesc {
    ItemDesc { source: SourceId(id), video: Arc::clone(video), transform, effects: vec![] }
}

fn at(x: f32, y: f32) -> Transform {
    Transform { position: Vec2::new(x, y), ..Transform::default() }
}

fn pixel(rgba: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
    let i = ((y * width + x) * 4) as usize;
    [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
}

fn close(a: [u8; 4], b: [u8; 4]) -> bool {
    a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= 3)
}

fn draw(c: &mut Compositor, plan: Plan, now: Tick) -> Vec<u8> {
    c.set_plan(Arc::new(plan));
    c.frame(now, &AtomicU64::new(0));
    c.read_program().unwrap()
}

#[test]
fn places_solid_items_over_black() {
    let Some(mut c) = compositor(64, 32) else { return };
    let red = solid([255, 0, 0, 255], 16, 16);
    let program = SceneDesc { items: vec![item(1, &red, at(8.0, 8.0))] };
    let px = draw(&mut c, Plan { program, ..Plan::default() }, Tick(0));
    assert!(close(pixel(&px, 64, 12, 12), [255, 0, 0, 255]));
    assert!(close(pixel(&px, 64, 2, 2), [0, 0, 0, 255]));
    assert!(close(pixel(&px, 64, 30, 12), [0, 0, 0, 255]));
}

#[test]
fn later_items_draw_over_earlier_ones_and_alpha_blends() {
    let Some(mut c) = compositor(32, 32) else { return };
    let red = solid([255, 0, 0, 255], 32, 32);
    let half_white = solid([255, 255, 255, 128], 16, 32);
    let program = SceneDesc { items: vec![item(1, &red, at(0.0, 0.0)), item(2, &half_white, at(0.0, 0.0))] };
    let px = draw(&mut c, Plan { program, ..Plan::default() }, Tick(0));
    assert!(close(pixel(&px, 32, 4, 4), [255, 128, 128, 255]), "{:?}", pixel(&px, 32, 4, 4));
    assert!(close(pixel(&px, 32, 24, 4), [255, 0, 0, 255]));
}

#[test]
fn frames_are_scaled_and_cropped() {
    let Some(mut c) = compositor(40, 10) else { return };
    // Four columns: red, green, blue, white. Cropping two from the left leaves blue and white.
    let v = frame(&[[255, 0, 0, 255], [0, 255, 0, 255], [0, 0, 255, 255], [255, 255, 255, 255]], 4, 1);
    let t = Transform { scale: Vec2::new(20.0, 10.0), crop: Crop { left: 2, ..Crop::default() }, ..Transform::default() };
    let px = draw(&mut c, Plan { program: SceneDesc { items: vec![item(1, &v, t)] }, ..Plan::default() }, Tick(0));
    assert!(close(pixel(&px, 40, 5, 5), [0, 0, 255, 255]), "{:?}", pixel(&px, 40, 5, 5));
    assert!(close(pixel(&px, 40, 35, 5), [255, 255, 255, 255]));
}

#[test]
fn the_last_frame_stays_until_a_new_one_arrives() {
    let Some(mut c) = compositor(16, 16) else { return };
    let v = frame(&[[0, 255, 0, 255]; 4], 2, 2);
    let t = Transform { scale: Vec2::new(8.0, 8.0), ..Transform::default() };
    let plan = Plan { program: SceneDesc { items: vec![item(1, &v, t)] }, ..Plan::default() };
    draw(&mut c, plan.clone(), Tick(0));
    let px = draw(&mut c, plan, Tick(1));
    assert!(close(pixel(&px, 16, 8, 8), [0, 255, 0, 255]));
}

#[test]
fn rotation_turns_around_the_center() {
    let Some(mut c) = compositor(40, 40) else { return };
    // A 30 x 4 bar centered on (20, 20), turned upright.
    let bar = solid([255, 255, 255, 255], 30, 4);
    let t = Transform { position: Vec2::new(5.0, 18.0), rotation: 90.0, ..Transform::default() };
    let px = draw(&mut c, Plan { program: SceneDesc { items: vec![item(1, &bar, t)] }, ..Plan::default() }, Tick(0));
    assert!(close(pixel(&px, 40, 20, 8), [255, 255, 255, 255]));
    assert!(close(pixel(&px, 40, 8, 20), [0, 0, 0, 255]));
}

const INVERT: &str = "fn effect(color: vec4<f32>, uv: vec2<f32>, size: vec2<f32>, p0: vec4<f32>, p1: vec4<f32>) -> vec4<f32> {
    return vec4<f32>((vec3<f32>(1.0) - color.rgb) * color.a * p0.x + color.rgb * (1.0 - p0.x), color.a);
}";

#[test]
fn filters_run_as_gpu_passes_in_order() {
    let Some(mut c) = compositor(16, 16) else { return };
    let v = solid([255, 0, 0, 255], 16, 16);
    let invert = VideoEffect { key: "test.invert", wgsl: INVERT, params: [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0] };
    let mut it = item(1, &v, at(0.0, 0.0));
    it.effects = vec![invert];
    let px = draw(&mut c, Plan { program: SceneDesc { items: vec![it.clone()] }, ..Plan::default() }, Tick(0));
    assert!(close(pixel(&px, 16, 8, 8), [0, 255, 255, 255]), "{:?}", pixel(&px, 16, 8, 8));
    it.effects = vec![invert, invert];
    let px = draw(&mut c, Plan { program: SceneDesc { items: vec![it] }, ..Plan::default() }, Tick(1));
    assert!(close(pixel(&px, 16, 8, 8), [255, 0, 0, 255]), "two inverts cancel");
}

#[test]
fn a_broken_filter_is_reported_and_skipped() {
    let Some(mut c) = compositor(16, 16) else { return };
    let v = solid([0, 0, 255, 255], 16, 16);
    let broken = VideoEffect { key: "test.broken", wgsl: "fn effect( this is not wgsl", params: [0.0; 8] };
    let mut it = item(1, &v, at(0.0, 0.0));
    it.effects = vec![broken];
    let px = draw(&mut c, Plan { program: SceneDesc { items: vec![it] }, ..Plan::default() }, Tick(0));
    assert!(close(pixel(&px, 16, 8, 8), [0, 0, 255, 255]));
    let errors = c.take_errors();
    assert_eq!(errors.len(), 1);
    assert!(matches!(&errors[0], RenderError::Shader { key, .. } if key == "test.broken"));
}

#[test]
fn transitions_blend_the_two_scenes_over_time() {
    let Some(mut c) = compositor(16, 16) else { return };
    let red = solid([255, 0, 0, 255], 16, 16);
    let blue = solid([0, 0, 255, 255], 16, 16);
    let fade = TransitionDesc {
        from: SceneDesc { items: vec![item(1, &red, at(0.0, 0.0))] },
        key: Fade.id().into(),
        wgsl: Fade.shader(),
        params: [0.0; 4],
        start: Tick(1000),
        duration: Tick(1000),
    };
    let plan = Plan { program: SceneDesc { items: vec![item(2, &blue, at(0.0, 0.0))] }, transition: Some(fade), preview: None };
    let px = draw(&mut c, plan.clone(), Tick(1500));
    let errors = c.take_errors();
    assert!(errors.is_empty(), "{errors:?}");
    assert!(close(pixel(&px, 16, 8, 8), [128, 0, 128, 255]), "{:?}", pixel(&px, 16, 8, 8));
    let px = draw(&mut c, plan, Tick(5000));
    assert!(close(pixel(&px, 16, 8, 8), [0, 0, 255, 255]));
}

#[test]
fn every_built_in_transition_compiles() {
    use crate::transitions::{Cut, FadeThroughColor, Swipe};
    let Some(mut c) = compositor(16, 16) else { return };
    let builtins: [&dyn Transition; 4] = [&Cut, &Fade, &FadeThroughColor, &Swipe];
    for t in builtins {
        let desc =
            TransitionDesc { from: SceneDesc::default(), key: t.id().into(), wgsl: t.shader(), params: [0.0; 4], start: Tick(0), duration: Tick(10) };
        draw(&mut c, Plan { transition: Some(desc), ..Plan::default() }, Tick(5));
        let errors = c.take_errors();
        assert!(errors.is_empty(), "{}: {errors:?}", t.id());
    }
}

#[test]
fn studio_mode_draws_the_preview_separately() {
    let Some(mut c) = compositor(16, 16) else { return };
    assert!(c.views().preview.latest().is_none());
    let red = solid([255, 0, 0, 255], 16, 16);
    let green = solid([0, 255, 0, 255], 16, 16);
    let plan = Plan {
        program: SceneDesc { items: vec![item(1, &red, at(0.0, 0.0))] },
        preview: Some(SceneDesc { items: vec![item(2, &green, at(0.0, 0.0))] }),
        transition: None,
    };
    let px = draw(&mut c, plan, Tick(0));
    assert!(close(pixel(&px, 16, 8, 8), [255, 0, 0, 255]));
    let preview = c.read_preview().unwrap().unwrap();
    assert!(close(pixel(&preview, 16, 8, 8), [0, 255, 0, 255]));
}

#[test]
fn resizing_the_canvas_makes_new_views() {
    let Some(mut c) = compositor(16, 16) else { return };
    let g = c.views().generation;
    c.set_canvas(32, 18);
    assert_eq!(c.size(), (32, 18));
    assert_eq!(c.views().generation, g + 1);
    assert_eq!(c.views().program.views().len(), 3);
}

#[test]
fn encoders_receive_nv12_frames_in_order() {
    let Some(mut c) = compositor(32, 16) else { return };
    let white = solid([255, 255, 255, 255], 16, 16);
    c.set_plan(Arc::new(Plan { program: SceneDesc { items: vec![item(1, &white, at(0.0, 0.0))] }, ..Plan::default() }));
    let sink = Arc::new(Fifo::new(16));
    c.set_encode(Some(EncodeTarget { width: 16, height: 8, sinks: vec![Arc::clone(&sink)] }));
    let lost = AtomicU64::new(0);
    for i in 0..5 {
        c.frame(Tick(i * 100), &lost);
        c.finish(&lost);
    }
    let frames: Vec<_> = std::iter::from_fn(|| sink.pop()).collect();
    assert!(frames.len() + lost.load(std::sync::atomic::Ordering::Relaxed) as usize == 5, "{} frames", frames.len());
    assert!(frames.len() >= 4);
    assert!(frames.windows(2).all(|w| w[0].pts < w[1].pts));
    let f = &frames[0];
    assert_eq!((f.width, f.height, f.format), (16, 8, PixelFormat::Nv12));
    assert_eq!(f.data.len(), 16 * 8 * 3 / 2);
    // Left half white (luma 235), right half black (luma 16), in limited range.
    assert!(f.data[2].abs_diff(235) <= 2, "{}", f.data[2]);
    assert!(f.data[13].abs_diff(16) <= 2, "{}", f.data[13]);
    // Chroma of white and black is neutral.
    let chroma = &f.data[16 * 8..];
    assert!(chroma.iter().all(|&v| v.abs_diff(128) <= 2), "{chroma:?}");
}

#[test]
fn the_scope_copy_runs_beside_the_encoders_at_its_own_size() {
    let Some(mut c) = compositor(32, 16) else { return };
    let white = solid([255, 255, 255, 255], 16, 16);
    c.set_plan(Arc::new(Plan { program: SceneDesc { items: vec![item(1, &white, at(0.0, 0.0))] }, ..Plan::default() }));
    let (enc, scope) = (Arc::new(Fifo::new(16)), Arc::new(Fifo::new(16)));
    c.set_encode(Some(EncodeTarget { width: 32, height: 16, sinks: vec![Arc::clone(&enc)] }));
    c.set_scope(Some(EncodeTarget { width: 8, height: 4, sinks: vec![Arc::clone(&scope)] }));
    let lost = AtomicU64::new(0);
    for i in 0..4 {
        c.frame(Tick(i * 100), &lost);
        c.finish(&lost);
    }
    let frames: Vec<_> = std::iter::from_fn(|| scope.pop()).collect();
    assert_eq!(frames.len() + c.scope_skipped() as usize, 4);
    assert!(!frames.is_empty());
    let f = &frames[0];
    assert_eq!((f.width, f.height, f.format), (8, 4, PixelFormat::Nv12));
    // Left half white, right half black, in limited range.
    assert!(f.data[1].abs_diff(235) <= 2 && f.data[6].abs_diff(16) <= 2, "{:?}", &f.data[..8]);
    assert!(enc.pop().is_some_and(|e| (e.width, e.height) == (32, 16)), "the encoders keep their own size");
    // Stopping the scope copy leaves the encoders running.
    c.set_scope(None);
    c.frame(Tick(500), &lost);
    c.finish(&lost);
    assert!(scope.pop().is_none());
}

#[test]
fn unpremultiply_restores_straight_alpha() {
    let mut px = [128, 64, 0, 128, 10, 20, 30, 0, 255, 0, 0, 255];
    unpremultiply(&mut px);
    assert_eq!(px, [255, 128, 0, 128, 0, 0, 0, 0, 255, 0, 0, 255]);
}

#[test]
fn the_capture_queue_refuses_a_request_when_full() {
    let shared = RenderShared::default();
    let request = || CaptureRequest { view: View::Program, reply: std::sync::mpsc::sync_channel(1).0 };
    for _ in 0..CAPTURE_SLOTS {
        assert!(shared.captures.push(request()).is_ok());
    }
    assert!(shared.captures.push(request()).is_err());
}

#[test]
fn a_frame_answers_waiting_captures() {
    let Some(mut c) = compositor(32, 16) else { return };
    let shared = RenderShared::default();
    let red = solid([255, 0, 0, 255], 32, 16);
    let program = SceneDesc { items: vec![item(1, &red, Transform::default())] };
    shared.plan.publish(Arc::new(Plan { program, ..Plan::default() }));
    let (program_tx, program_rx) = std::sync::mpsc::sync_channel(1);
    let (preview_tx, preview_rx) = std::sync::mpsc::sync_channel(1);
    assert!(shared.captures.push(CaptureRequest { view: View::Program, reply: program_tx }).is_ok());
    assert!(shared.captures.push(CaptureRequest { view: View::Preview, reply: preview_tx }).is_ok());
    c.run(&shared, Tick(0));
    let shot = program_rx.try_recv().unwrap().unwrap();
    assert_eq!((shot.width, shot.height, shot.rgba.len()), (32, 16, 32 * 16 * 4));
    assert!(close(pixel(&shot.rgba, 32, 16, 8), [255, 0, 0, 255]));
    assert!(preview_rx.try_recv().unwrap().is_err(), "no preview outside studio mode");
    assert!(shared.captures.is_empty());
}
