//! These tests need a graphics adapter; on a machine without one they pass without checking.

use std::sync::atomic::AtomicU64;
use std::sync::{Arc, OnceLock};

use studiocraft_media::Tick;
use studiocraft_media::queue::Fifo;
use studiocraft_media::video::PixelFormat;
use studiocraft_model::{Crop, Settings, SourceId, Transform, Vec2};
use studiocraft_plugin::{Animation, Easing, Plugin, Reframe, Transition, VIDEO_EFFECT_PARAMS, VideoContent, VideoEffect, VideoOut};

use super::*;
use crate::transitions::Fade;
use crate::{EncodeTarget, ItemDesc, MAX_THUMBNAILS, Plan, SceneDesc, ThumbnailRequest, TransitionDesc};

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
fn flips_mirror_the_picture_inside_the_box() {
    let Some(mut c) = compositor(20, 20) else { return };
    // Red, green on the top row; blue, white below.
    let v = frame(&[[255, 0, 0, 255], [0, 255, 0, 255], [0, 0, 255, 255], [255, 255, 255, 255]], 2, 2);
    let (red, green, blue, white) = ([255, 0, 0, 255], [0, 255, 0, 255], [0, 0, 255, 255], [255, 255, 255, 255]);
    let cases = [
        ((false, false), [red, green, blue, white]),
        ((true, false), [green, red, white, blue]),
        ((false, true), [blue, white, red, green]),
        ((true, true), [white, blue, green, red]),
    ];
    for ((h, v_flip), want) in cases {
        let t = Transform { scale: Vec2::new(10.0, 10.0), flip_horizontal: h, flip_vertical: v_flip, ..Transform::default() };
        let px = draw(&mut c, Plan { program: SceneDesc { items: vec![item(1, &v, t)] }, ..Plan::default() }, Tick(0));
        // Pixels near the outer edges, where the filter stays inside one texel.
        let got = [pixel(&px, 20, 2, 2), pixel(&px, 20, 17, 2), pixel(&px, 20, 2, 17), pixel(&px, 20, 17, 17)];
        assert!(got.iter().zip(want).all(|(g, w)| close(*g, w)), "flip ({h}, {v_flip}): {got:?}");
    }
}

#[test]
fn a_flip_mirrors_what_the_crop_leaves() {
    let Some(mut c) = compositor(40, 10) else { return };
    // Red, green, blue, white; cropping two from the left leaves blue and white, which the flip
    // swaps. The crop's edge is the source's left edge, before the flip.
    let v = frame(&[[255, 0, 0, 255], [0, 255, 0, 255], [0, 0, 255, 255], [255, 255, 255, 255]], 4, 1);
    let t = Transform { scale: Vec2::new(20.0, 10.0), crop: Crop { left: 2, ..Crop::default() }, flip_horizontal: true, ..Transform::default() };
    let px = draw(&mut c, Plan { program: SceneDesc { items: vec![item(1, &v, t)] }, ..Plan::default() }, Tick(0));
    assert!(close(pixel(&px, 40, 5, 5), [255, 255, 255, 255]), "{:?}", pixel(&px, 40, 5, 5));
    assert!(close(pixel(&px, 40, 35, 5), [0, 0, 255, 255]), "{:?}", pixel(&px, 40, 35, 5));
    // The crop's edge is at the right after the flip: the green that the crop removed does not
    // blend into it.
    assert!(close(pixel(&px, 40, 39, 5), [0, 0, 255, 255]), "{:?}", pixel(&px, 40, 39, 5));
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
fn a_shared_capture_the_device_cannot_take_falls_back_and_the_program_goes_on() {
    let Some(mut c) = compositor(16, 16) else { return };
    let v = frame(&[[0, 255, 0, 255]; 4], 2, 2);
    let t = Transform { scale: Vec2::new(8.0, 8.0), ..Transform::default() };
    let plan = Plan { program: SceneDesc { items: vec![item(1, &v, t)] }, ..Plan::default() };
    draw(&mut c, plan.clone(), Tick(0));
    // A shared picture from a ring no device can open: the reason is recorded on the ring, and
    // the last picture from memory stays on screen.
    let ring = craft_gpu_interop::SharedRing::detached(2, 2);
    v.publish(VideoContent::Shared(craft_gpu_interop::SharedFrame::new(Arc::clone(&ring), 0, 1).unwrap()));
    let px = draw(&mut c, plan.clone(), Tick(1));
    assert!(ring.fallback().is_some(), "the capture is told to copy through memory");
    assert!(close(pixel(&px, 16, 8, 8), [0, 255, 0, 255]));
    // The next picture from memory is drawn as before.
    let mut f = v.frame();
    assert!(f.reshape(2, 2, PixelFormat::Rgba8));
    f.data.as_chunks_mut::<4>().0.iter_mut().for_each(|p| *p = [255, 0, 0, 255]);
    v.publish(VideoContent::Frame(f));
    let px = draw(&mut c, plan, Tick(2));
    assert!(close(pixel(&px, 16, 8, 8), [255, 0, 0, 255]));
}

#[test]
fn a_shared_capture_of_no_size_is_refused() {
    let Some(mut c) = compositor(8, 8) else { return };
    let v = Arc::new(VideoOut::default());
    let ring = craft_gpu_interop::SharedRing::detached(0, 0);
    v.publish(VideoContent::Shared(craft_gpu_interop::SharedFrame::new(Arc::clone(&ring), 0, 1).unwrap()));
    let plan = Plan { program: SceneDesc { items: vec![item(1, &v, at(0.0, 0.0))] }, ..Plan::default() };
    let px = draw(&mut c, plan, Tick(0));
    assert_eq!(ring.fallback(), Some(craft_gpu_interop::Fallback::Format));
    assert!(close(pixel(&px, 8, 4, 4), [0, 0, 0, 255]));
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
    let mut params = [0.0; VIDEO_EFFECT_PARAMS];
    params[0] = 1.0;
    let invert = VideoEffect::new("test.invert", INVERT, params);
    let mut it = item(1, &v, at(0.0, 0.0));
    it.effects = vec![invert.clone()];
    let px = draw(&mut c, Plan { program: SceneDesc { items: vec![it.clone()] }, ..Plan::default() }, Tick(0));
    assert!(close(pixel(&px, 16, 8, 8), [0, 255, 255, 255]), "{:?}", pixel(&px, 16, 8, 8));
    it.effects = vec![invert.clone(), invert];
    let px = draw(&mut c, Plan { program: SceneDesc { items: vec![it] }, ..Plan::default() }, Tick(1));
    assert!(close(pixel(&px, 16, 8, 8), [255, 0, 0, 255]), "two inverts cancel");
}

/// Returns the column of `extra_params()` that `p0.x` names.
const EXTRA_PARAMS_COLUMN: &str = "fn effect(color: vec4<f32>, uv: vec2<f32>, size: vec2<f32>, p0: vec4<f32>, p1: vec4<f32>) -> vec4<f32> {
    let m = extra_params();
    if p0.x < 0.5 { return m[0]; }
    if p0.x < 1.5 { return m[1]; }
    if p0.x < 2.5 { return m[2]; }
    return m[3];
}";

#[test]
fn every_filter_parameter_reaches_the_shader() {
    let Some(mut c) = compositor(16, 16) else { return };
    let v = solid([0, 0, 0, 255], 16, 16);
    // Each of params[8..24] holds its own value: column j, row k is (4j + k + 1) / 16, and the
    // fourth row is 1 so the pixel reads back opaque.
    let mut params = [0.0; VIDEO_EFFECT_PARAMS];
    for j in 0..4 {
        for k in 0..3 {
            params[8 + 4 * j + k] = (4 * j + k + 1) as f32 / 16.0;
        }
        params[11 + 4 * j] = 1.0;
    }
    for j in 0..4u8 {
        params[0] = f32::from(j);
        let mut it = item(1, &v, at(0.0, 0.0));
        it.effects = vec![VideoEffect::new("test.extra_params", EXTRA_PARAMS_COLUMN, params)];
        let px = draw(&mut c, Plan { program: SceneDesc { items: vec![it] }, ..Plan::default() }, Tick(0));
        let want = [0, 1, 2].map(|k| ((4 * j + k + 1) as f32 / 16.0 * 255.0).round() as u8);
        assert!(close(pixel(&px, 16, 8, 8), [want[0], want[1], want[2], 255]), "column {j}: {:?} vs {want:?}", pixel(&px, 16, 8, 8));
    }
}

#[test]
fn a_broken_filter_is_reported_and_skipped() {
    let Some(mut c) = compositor(16, 16) else { return };
    let v = solid([0, 0, 255, 255], 16, 16);
    let broken = VideoEffect::new("test.broken", "fn effect( this is not wgsl", [0.0; VIDEO_EFFECT_PARAMS]);
    let mut it = item(1, &v, at(0.0, 0.0));
    it.effects = vec![broken];
    let px = draw(&mut c, Plan { program: SceneDesc { items: vec![it] }, ..Plan::default() }, Tick(0));
    assert!(close(pixel(&px, 16, 8, 8), [0, 0, 255, 255]));
    let errors = c.take_errors();
    assert_eq!(errors.len(), 1);
    assert!(matches!(&errors[0], RenderError::Shader { key, .. } if key == "test.broken"));
}

/// Shows the filter's picture, stretched over the output.
const PICTURE: &str = "fn effect(color: vec4<f32>, uv: vec2<f32>, size: vec2<f32>, p0: vec4<f32>, p1: vec4<f32>) -> vec4<f32> {
    return image_at(uv);
}";

fn red_and_blue() -> EffectImage {
    EffectImage::new(2, 1, vec![255, 0, 0, 255, 0, 0, 255, 255]).unwrap()
}

#[test]
fn a_filter_reads_its_picture_and_the_picture_is_released_with_the_plan() {
    let Some(mut c) = compositor(16, 16) else { return };
    let v = solid([0, 255, 0, 255], 16, 16);
    let mut it = item(1, &v, at(0.0, 0.0));
    it.effects = vec![VideoEffect { image: Some(red_and_blue()), ..VideoEffect::new("test.picture", PICTURE, [0.0; VIDEO_EFFECT_PARAMS]) }];
    let px = draw(&mut c, Plan { program: SceneDesc { items: vec![it.clone()] }, ..Plan::default() }, Tick(0));
    assert!(c.take_errors().is_empty());
    assert!(close(pixel(&px, 16, 2, 8), [255, 0, 0, 255]), "{:?}", pixel(&px, 16, 2, 8));
    assert!(close(pixel(&px, 16, 13, 8), [0, 0, 255, 255]), "{:?}", pixel(&px, 16, 13, 8));
    assert_eq!(c.images.len(), 1);
    // The same plan again uploads nothing new; a plan without the filter releases the picture.
    draw(&mut c, Plan { program: SceneDesc { items: vec![it.clone()] }, ..Plan::default() }, Tick(1));
    assert_eq!(c.images.len(), 1);
    it.effects.clear();
    let px = draw(&mut c, Plan { program: SceneDesc { items: vec![it] }, ..Plan::default() }, Tick(2));
    assert!(c.images.is_empty());
    assert!(close(pixel(&px, 16, 2, 8), [0, 255, 0, 255]));
}

/// Between an opaque white pixel and a transparent red one, a stretched picture blends toward
/// transparent, without the red the transparent pixel stores.
#[test]
fn a_stretched_picture_blends_without_the_color_of_its_transparent_pixels() {
    let Some(mut c) = compositor(16, 16) else { return };
    let v = solid([0, 0, 0, 255], 16, 16);
    let image = EffectImage::new(2, 1, vec![255, 255, 255, 255, 255, 0, 0, 0]).unwrap();
    let mut it = item(1, &v, at(0.0, 0.0));
    it.effects = vec![VideoEffect { image: Some(image), ..VideoEffect::new("test.picture", PICTURE, [0.0; VIDEO_EFFECT_PARAMS]) }];
    let px = draw(&mut c, Plan { program: SceneDesc { items: vec![it] }, ..Plan::default() }, Tick(0));
    // Pixel 7's center is 0.5625 of the way from the red texel to the white one.
    let got = pixel(&px, 16, 7, 8);
    assert!(got[..3].iter().all(|v| v.abs_diff(143) <= 4), "{got:?}");
}

/// A key compiled with a picture is skipped when a pass under it comes without one, and the
/// other way round.
#[test]
fn a_pass_that_differs_from_its_key_about_a_picture_is_skipped() {
    let Some(mut c) = compositor(16, 16) else { return };
    let v = solid([0, 255, 0, 255], 16, 16);
    let with = VideoEffect { image: Some(red_and_blue()), ..VideoEffect::new("test.picture", PICTURE, [0.0; VIDEO_EFFECT_PARAMS]) };
    let without = VideoEffect::new("test.picture", PICTURE, [0.0; VIDEO_EFFECT_PARAMS]);
    let mut it = item(1, &v, at(0.0, 0.0));
    it.effects = vec![with];
    draw(&mut c, Plan { program: SceneDesc { items: vec![it.clone()] }, ..Plan::default() }, Tick(0));
    it.effects = vec![without.clone()];
    let px = draw(&mut c, Plan { program: SceneDesc { items: vec![it.clone()] }, ..Plan::default() }, Tick(1));
    assert!(close(pixel(&px, 16, 2, 8), [0, 255, 0, 255]), "{:?}", pixel(&px, 16, 2, 8));
    let keep = VideoEffect::new("test.keep", KEEP, [0.0; VIDEO_EFFECT_PARAMS]);
    it.effects = vec![keep, VideoEffect { image: Some(red_and_blue()), ..VideoEffect::new("test.keep", KEEP, [0.0; VIDEO_EFFECT_PARAMS]) }];
    let px = draw(&mut c, Plan { program: SceneDesc { items: vec![it] }, ..Plan::default() }, Tick(2));
    assert!(close(pixel(&px, 16, 2, 8), [0, 255, 0, 255]), "{:?}", pixel(&px, 16, 2, 8));
    assert!(c.take_errors().is_empty());
}

/// Red is `p0.x` and green `p0.y`.
const SHOW_PARAMS: &str = "fn effect(color: vec4<f32>, uv: vec2<f32>, size: vec2<f32>, p0: vec4<f32>, p1: vec4<f32>) -> vec4<f32> {
    return vec4<f32>(p0.x, p0.y, 0.0, 1.0);
}";

/// A tenth of the seconds in `p[0]` and a sixteenth of the input's width in `p[1]`.
fn tenths(p: &mut [f32; VIDEO_EFFECT_PARAMS], seconds: f64, (w, _): (u32, u32)) {
    p[0] = (seconds / 10.0) as f32;
    p[1] = w as f32 / 16.0;
}

/// An animated pass steps from the first frame that draws it; a new animation, or one that
/// left the plan and came back, starts at 0 again.
#[test]
fn an_animated_filter_steps_its_parameters_from_its_first_frame() {
    let Some(mut c) = compositor(16, 16) else { return };
    let v = solid([0, 0, 0, 255], 8, 8);
    let s = |n: i64| Tick::from_units(n, 1);
    let plan = |a: Animation| {
        let mut it = item(1, &v, at(0.0, 0.0));
        it.effects = vec![VideoEffect { animation: Some(a), ..VideoEffect::new("test.animated", SHOW_PARAMS, [0.0; VIDEO_EFFECT_PARAMS]) }];
        Plan { program: SceneDesc { items: vec![it] }, ..Plan::default() }
    };
    let a = Animation::new(tenths);
    let red_green = |px: &[u8]| [pixel(px, 16, 2, 2)[0], pixel(px, 16, 2, 2)[1]];
    assert_eq!(red_green(&draw(&mut c, plan(a), s(100))), [0, 128]);
    // 0.5 x 255, rounded either way.
    let half = red_green(&draw(&mut c, plan(a), s(105)))[0];
    assert!(half.abs_diff(128) <= 1, "{half}");
    draw(&mut c, Plan::default(), s(106));
    assert_eq!(red_green(&draw(&mut c, plan(a), s(108)))[0], 0, "a returning animation starts again");
    assert_eq!(red_green(&draw(&mut c, plan(Animation::new(tenths)), s(109)))[0], 0, "a new animation starts at 0");
}

const KEEP: &str = "fn effect(color: vec4<f32>, uv: vec2<f32>, size: vec2<f32>, p0: vec4<f32>, p1: vec4<f32>) -> vec4<f32> {
    return color;
}";

/// Every output pixel shows the input's bottom-right pixel.
const LAST_PIXEL: &str = "fn effect(color: vec4<f32>, uv: vec2<f32>, size: vec2<f32>, p0: vec4<f32>, p1: vec4<f32>) -> vec4<f32> {
    return input_pixel(input_size() - vec2<f32>(0.5));
}";

fn reframe(key: &'static str, wgsl: &'static str, reframe: Reframe) -> VideoEffect {
    VideoEffect { reframe, ..VideoEffect::new(key, wgsl, [0.0; VIDEO_EFFECT_PARAMS]) }
}

/// Four columns: red, green, blue, white.
fn four_columns() -> Arc<VideoOut> {
    frame(&[[255, 0, 0, 255], [0, 255, 0, 255], [0, 0, 255, 255], [255, 255, 255, 255]], 4, 1)
}

#[test]
fn a_filter_that_crops_draws_the_source_at_the_cropped_size() {
    let Some(mut c) = compositor(40, 20) else { return };
    let v = four_columns();
    // One column off each side leaves green and blue, drawn 2 x 1 at a scale of 20 x 10.
    let crop = reframe("test.keep", KEEP, Reframe::Edges { left: 1, top: 0, right: 1, bottom: 0 });
    let mut it = item(1, &v, Transform { scale: Vec2::new(20.0, 10.0), ..Transform::default() });
    it.effects = vec![crop];
    let px = draw(&mut c, Plan { program: SceneDesc { items: vec![it.clone()] }, ..Plan::default() }, Tick(0));
    assert!(close(pixel(&px, 40, 5, 5), [0, 255, 0, 255]), "{:?}", pixel(&px, 40, 5, 5));
    assert!(close(pixel(&px, 40, 35, 5), [0, 0, 255, 255]), "{:?}", pixel(&px, 40, 35, 5));
    assert!(close(pixel(&px, 40, 20, 15), [0, 0, 0, 255]), "the item is 10 pixels tall");
    // An area two columns wide from the third column: blue and white.
    it.effects = vec![reframe("test.keep", KEEP, Reframe::Area { x: 2, y: 0, width: 0, height: 0 })];
    let px = draw(&mut c, Plan { program: SceneDesc { items: vec![it.clone()] }, ..Plan::default() }, Tick(1));
    assert!(close(pixel(&px, 40, 5, 5), [0, 0, 255, 255]), "{:?}", pixel(&px, 40, 5, 5));
    assert!(close(pixel(&px, 40, 35, 5), [255, 255, 255, 255]), "{:?}", pixel(&px, 40, 35, 5));
    // The item's own crop applies to the filtered picture: a column off the left leaves white.
    it.transform.crop = Crop { left: 1, ..Crop::default() };
    let px = draw(&mut c, Plan { program: SceneDesc { items: vec![it] }, ..Plan::default() }, Tick(2));
    assert!(close(pixel(&px, 40, 10, 5), [255, 255, 255, 255]), "{:?}", pixel(&px, 40, 10, 5));
    assert!(close(pixel(&px, 40, 30, 5), [0, 0, 0, 255]), "{:?}", pixel(&px, 40, 30, 5));
    assert!(c.take_errors().is_empty());
}

#[test]
fn a_filter_that_pads_adds_transparent_pixels() {
    let Some(mut c) = compositor(16, 16) else { return };
    let red = solid([255, 0, 0, 255], 4, 4);
    let mut it = item(1, &red, at(0.0, 0.0));
    it.effects = vec![reframe("test.keep", KEEP, Reframe::Edges { left: -2, top: -2, right: -2, bottom: -2 })];
    let bottom = solid([0, 0, 255, 255], 16, 16);
    let program = SceneDesc { items: vec![item(2, &bottom, at(0.0, 0.0)), it] };
    let px = draw(&mut c, Plan { program, ..Plan::default() }, Tick(0));
    // The padding shows the blue item under it; the red is 2 pixels in and 4 wide.
    for (x, y, want) in
        [(1, 1, [0, 0, 255, 255]), (3, 3, [255, 0, 0, 255]), (5, 4, [255, 0, 0, 255]), (6, 6, [0, 0, 255, 255]), (7, 3, [0, 0, 255, 255])]
    {
        assert!(close(pixel(&px, 16, x, y), want), "({x}, {y}): {:?}", pixel(&px, 16, x, y));
    }
}

#[test]
fn a_later_filter_reads_only_the_earlier_output() {
    let Some(mut c) = compositor(16, 16) else { return };
    let v = four_columns();
    // Padding then cropping back leaves the 4 x 1 picture in a 6 x 3 filter texture; the last
    // pass must read white at its bottom-right, not the transparent texels around it.
    let mut it = item(1, &v, Transform { scale: Vec2::new(4.0, 4.0), ..Transform::default() });
    it.effects = vec![
        reframe("test.keep", KEEP, Reframe::Edges { left: -1, top: -1, right: -1, bottom: -1 }),
        reframe("test.keep", KEEP, Reframe::Edges { left: 1, top: 1, right: 1, bottom: 1 }),
        VideoEffect::new("test.last_pixel", LAST_PIXEL, [0.0; VIDEO_EFFECT_PARAMS]),
    ];
    let px = draw(&mut c, Plan { program: SceneDesc { items: vec![it] }, ..Plan::default() }, Tick(0));
    assert!(c.take_errors().is_empty());
    assert!(close(pixel(&px, 16, 1, 1), [255, 255, 255, 255]), "{:?}", pixel(&px, 16, 1, 1));
    assert!(close(pixel(&px, 16, 14, 2), [255, 255, 255, 255]), "{:?}", pixel(&px, 16, 14, 2));
    assert!(close(pixel(&px, 16, 8, 6), [0, 0, 0, 255]), "the picture is 4 pixels tall: {:?}", pixel(&px, 16, 8, 6));
}

#[test]
fn a_filter_that_crops_everything_draws_nothing() {
    let Some(mut c) = compositor(16, 16) else { return };
    let red = solid([255, 0, 0, 255], 16, 16);
    let mut it = item(1, &red, at(0.0, 0.0));
    it.effects = vec![
        reframe("test.keep", KEEP, Reframe::Edges { left: 8, top: 0, right: 8, bottom: 0 }),
        reframe("test.keep", KEEP, Reframe::Edges { left: -4, top: -4, right: -4, bottom: -4 }),
    ];
    let px = draw(&mut c, Plan { program: SceneDesc { items: vec![it.clone()] }, ..Plan::default() }, Tick(0));
    assert!(close(pixel(&px, 16, 8, 8), [0, 0, 0, 255]), "{:?}", pixel(&px, 16, 8, 8));
    assert!(c.take_errors().is_empty());
    // Taking the filters off draws the source again.
    it.effects.clear();
    let px = draw(&mut c, Plan { program: SceneDesc { items: vec![it] }, ..Plan::default() }, Tick(1));
    assert!(close(pixel(&px, 16, 8, 8), [255, 0, 0, 255]));
}

/// A solid wider than any texture is cropped from its own size: 8 pixels are left of 40 000.
#[test]
fn a_solid_wider_than_a_texture_is_cropped_from_its_own_size() {
    let Some(mut c) = compositor(16, 16) else { return };
    let wide = solid([255, 0, 0, 255], 40_000, 4);
    let mut it = item(1, &wide, at(0.0, 0.0));
    it.effects = vec![reframe("test.keep", KEEP, Reframe::Edges { left: 39_992, top: 0, right: 0, bottom: 0 })];
    let px = draw(&mut c, Plan { program: SceneDesc { items: vec![it] }, ..Plan::default() }, Tick(0));
    assert!(close(pixel(&px, 16, 4, 2), [255, 0, 0, 255]), "{:?}", pixel(&px, 16, 4, 2));
    assert!(close(pixel(&px, 16, 12, 2), [0, 0, 0, 255]), "{:?}", pixel(&px, 16, 12, 2));
}

#[test]
fn uniform_slots_follow_the_offset_alignment() {
    for align in [0, 1, 16, 32, 64, 100, 128, 256] {
        let stride = slot_stride(align);
        assert!(stride >= SLOT_BYTES && stride.is_multiple_of(u64::from(align.max(1))), "{align}: {stride}");
    }
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
        ease: Easing::Linear,
    };
    let plan = Plan { program: SceneDesc { items: vec![item(2, &blue, at(0.0, 0.0))] }, transition: Some(fade.clone()), preview: None };
    let px = draw(&mut c, plan.clone(), Tick(1500));
    let errors = c.take_errors();
    assert!(errors.is_empty(), "{errors:?}");
    assert!(close(pixel(&px, 16, 8, 8), [128, 0, 128, 255]), "{:?}", pixel(&px, 16, 8, 8));
    let px = draw(&mut c, plan.clone(), Tick(5000));
    assert!(close(pixel(&px, 16, 8, 8), [0, 0, 255, 255]));
    // Eased in, halfway through the time is a quarter of the way through the blend.
    let eased = Plan { transition: Some(TransitionDesc { ease: Easing::QuadIn, ..fade }), ..plan };
    let px = draw(&mut c, eased, Tick(1500));
    assert!(close(pixel(&px, 16, 8, 8), [191, 0, 64, 255]), "{:?}", pixel(&px, 16, 8, 8));
}

/// An ease that passes 1 before the end (Back Out) keeps the transition drawn until its duration
/// ends, and its shader receives `t` clamped to 1 meanwhile. The test shader shows the old scene
/// at `t = 1` and a gray it can only draw for `t` above 1, so the new scene appears only when
/// the transition stops being drawn.
#[test]
fn an_overshooting_ease_keeps_the_transition_drawn_until_it_ends() {
    let Some(mut c) = compositor(16, 16) else { return };
    let red = solid([255, 0, 0, 255], 16, 16);
    let blue = solid([0, 0, 255, 255], 16, 16);
    let desc = TransitionDesc {
        from: SceneDesc { items: vec![item(1, &red, at(0.0, 0.0))] },
        key: "test.old_scene".into(),
        wgsl: "fn transition(a: vec4<f32>, b: vec4<f32>, uv: vec2<f32>, t: f32, p: vec4<f32>) -> vec4<f32> { return select(a, vec4<f32>(0.5, 0.5, 0.5, 1.0), t > 1.0); }",
        params: [0.0; 4],
        start: Tick(1000),
        duration: Tick(1000),
        ease: Easing::BackOut,
    };
    // 70 % of the way through, Back Out gives about 1.08.
    assert!(desc.progress(Tick(1700)) > 1.05, "{}", desc.progress(Tick(1700)));
    let plan = Plan { program: SceneDesc { items: vec![item(2, &blue, at(0.0, 0.0))] }, transition: Some(desc), preview: None };
    let px = draw(&mut c, plan.clone(), Tick(1700));
    let errors = c.take_errors();
    assert!(errors.is_empty(), "{errors:?}");
    assert!(close(pixel(&px, 16, 8, 8), [255, 0, 0, 255]), "{:?}", pixel(&px, 16, 8, 8));
    // After the end only the new scene is drawn.
    let px = draw(&mut c, plan, Tick(2000));
    assert!(close(pixel(&px, 16, 8, 8), [0, 0, 255, 255]), "{:?}", pixel(&px, 16, 8, 8));
}

#[test]
fn every_built_in_transition_compiles() {
    use crate::transitions::{Cut, FadeThroughColor, Swipe};
    let Some(mut c) = compositor(16, 16) else { return };
    let builtins: [&dyn Transition; 4] = [&Cut, &Fade, &FadeThroughColor, &Swipe];
    for t in builtins {
        let desc = TransitionDesc {
            from: SceneDesc::default(),
            key: t.id().into(),
            wgsl: t.shader(),
            params: [0.0; 4],
            start: Tick(0),
            duration: Tick(10),
            ease: Easing::Linear,
        };
        draw(&mut c, Plan { transition: Some(desc), ..Plan::default() }, Tick(5));
        let errors = c.take_errors();
        assert!(errors.is_empty(), "{}: {errors:?}", t.id());
    }
}

/// Each built-in draws the same frames as before it moved to `craft-transitions`: the old shader
/// and parameters and the plug-in's are drawn over the same two patterned scenes at fixed times
/// and compared byte for byte.
#[test]
fn built_in_transitions_draw_the_same_frames_as_before() {
    use crate::transitions::tests::{before, built_ins};
    let Some(mut c) = compositor(16, 16) else { return };
    let pattern = |seed: u8| -> Vec<[u8; 4]> {
        (0..=255u8).map(|i| [i.wrapping_mul(seed), (i / 16).wrapping_mul(17), (i % 16).wrapping_mul(16) ^ seed, 255]).collect()
    };
    let old_scene = frame(&pattern(3), 16, 16);
    let new_scene = frame(&pattern(7), 16, 16);
    let settings = [
        Settings::default(),
        Settings::default().with("color", "#3366cc").with("direction", "top").with("fade_easing", "ease_in").with("swipe_easing", "ease_out"),
        Settings::default().with("color", "#ffffff80").with("direction", "right").with("swipe_easing", "linear"),
        Settings::default().with("direction", "bottom").with("fade_easing", "ease_in_out"),
    ];
    for t in built_ins() {
        for (n, s) in settings.iter().enumerate() {
            let desc = |key: String, wgsl: &'static str, params: [f32; 4], ease: Easing| TransitionDesc {
                from: SceneDesc { items: vec![item(1, &old_scene, at(0.0, 0.0))] },
                key,
                wgsl,
                params,
                start: Tick(1000),
                duration: Tick(1000),
                ease,
            };
            let old = desc(format!("before.{}", t.id()), before::shader(t.id()), before::params(t.id(), s), before::easing(t.id(), s));
            let new = desc(t.id().into(), t.shader(), t.params(s), t.easing(s));
            let plan = |d: &TransitionDesc| Plan {
                program: SceneDesc { items: vec![item(2, &new_scene, at(0.0, 0.0))] },
                transition: Some(d.clone()),
                preview: None,
            };
            for now in [1000, 1250, 1500, 1750, 1999] {
                let want = draw(&mut c, plan(&old), Tick(now));
                let got = draw(&mut c, plan(&new), Tick(now));
                let errors = c.take_errors();
                assert!(errors.is_empty(), "{}: {errors:?}", t.id());
                assert!(want == got, "{} with settings {n} at {now}", t.id());
            }
        }
    }
}

/// The CPU blend of `craft-transitions` gives the colors the GPU draws.
#[test]
fn the_cpu_blends_match_the_gpu() {
    use craft_transitions::{Edge, Kind, Params};
    let Some(mut c) = compositor(16, 16) else { return };
    let red = solid([255, 0, 0, 255], 16, 16);
    let blue = solid([0, 0, 255, 255], 16, 16);
    let params = Params { color: [0, 255, 0, 255], edge: Edge::Left, ..Params::default() };
    for kind in Kind::ALL {
        for (now, t) in [(0, 0.0f32), (250, 0.25), (500, 0.5), (750, 0.75)] {
            let desc = TransitionDesc {
                from: SceneDesc { items: vec![item(1, &red, at(0.0, 0.0))] },
                key: kind.id().into(),
                wgsl: kind.shader(),
                params: kind.uniform(&params),
                start: Tick(0),
                duration: Tick(1000),
                ease: Easing::Linear,
            };
            let plan = Plan { program: SceneDesc { items: vec![item(2, &blue, at(0.0, 0.0))] }, transition: Some(desc), preview: None };
            let px = draw(&mut c, plan, Tick(now));
            let errors = c.take_errors();
            assert!(errors.is_empty(), "{}: {errors:?}", kind.id());
            for x in [1, 6, 9, 14] {
                let uv = [(x as f32 + 0.5) / 16.0, 0.5];
                let want = kind.blend([1.0, 0.0, 0.0, 1.0], [0.0, 0.0, 1.0, 1.0], uv, t, kind.uniform(&params), |_| [0.0, 0.0, 1.0, 1.0]);
                let want = want.map(|v| (v * 255.0).round() as u8);
                let got = pixel(&px, 16, x, 8);
                assert!(close(got, want), "{} at {t}, x {x}: {got:?} vs {want:?}", kind.id());
            }
        }
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
fn finish_hands_over_every_read_back_while_another_thread_polls_the_device() {
    let Some(gpu) = gpu() else { return };
    let mut c = Compositor::new(gpu.clone(), 32, 16).unwrap();
    let white = solid([255, 255, 255, 255], 16, 16);
    c.set_plan(Arc::new(Plan { program: SceneDesc { items: vec![item(1, &white, at(0.0, 0.0))] }, ..Plan::default() }));
    let sink = Arc::new(Fifo::new(4));
    c.set_encode(Some(EncodeTarget { width: 16, height: 8, sinks: vec![Arc::clone(&sink)] }));
    // A second thread polls the same device, as the window does, and can run a read-back's
    // callback in place of the wait in `finish`.
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let poller = std::thread::spawn({
        let (gpu, stop) = (gpu.clone(), Arc::clone(&stop));
        move || {
            while !stop.load(Ordering::Acquire) {
                gpu.poll();
            }
        }
    });
    let lost = AtomicU64::new(0);
    let mut delivered = 0;
    let mut short = None;
    for i in 0..200 {
        c.frame(Tick(i * 100), &lost);
        c.finish(&lost);
        delivered += std::iter::from_fn(|| sink.pop()).count();
        let accounted = delivered + lost.load(Ordering::Relaxed) as usize;
        if accounted != i as usize + 1 {
            short = Some((i, accounted));
            break;
        }
    }
    stop.store(true, Ordering::Release);
    poller.join().unwrap();
    assert_eq!(short, None, "(frame, frames delivered or counted lost) after finish");
}

#[test]
fn an_output_larger_than_the_device_allows_is_an_error_not_a_converter() {
    let Some(mut c) = compositor(32, 16) else { return };
    let sink = Arc::new(Fifo::new(4));
    c.set_encode(Some(EncodeTarget { width: 1 << 20, height: 8, sinks: vec![Arc::clone(&sink)] }));
    let errors = c.take_errors();
    assert!(errors.iter().any(|e| e.to_string().contains("frames for the encoders cannot be converted")), "{errors:?}");
    let lost = AtomicU64::new(0);
    c.frame(Tick(0), &lost);
    c.finish(&lost);
    assert!(sink.pop().is_none());
    // A size the device holds starts the conversion.
    c.set_encode(Some(EncodeTarget { width: 16, height: 8, sinks: vec![Arc::clone(&sink)] }));
    assert!(c.take_errors().is_empty());
}

#[test]
fn another_output_size_drops_the_gpu_queue_of_the_old_ring() {
    let Some(mut c) = compositor(32, 16) else { return };
    let target = |width, height| Some(EncodeTarget { width, height, sinks: Vec::new() });
    let gpu_sink = Arc::new(Fifo::new(2));
    c.set_encode(target(16, 8));
    c.set_encode_gpu(Some(Arc::clone(&gpu_sink)));
    c.set_encode(target(16, 8));
    assert!(c.export_sink.is_some(), "the same size keeps the ring and its queue");
    c.set_encode(target(32, 16));
    assert!(c.export_sink.is_none(), "the session on the old ring cannot read the new one");
    assert!(c.nv12.as_ref().is_some_and(|n| !n.exporting()));
    // Stopping drops it too, and a queue given before the conversion starts is kept for it.
    c.set_encode_gpu(Some(Arc::clone(&gpu_sink)));
    c.set_encode(None);
    assert!(c.export_sink.is_none());
    c.set_encode_gpu(Some(gpu_sink));
    c.set_encode(target(16, 8));
    assert!(c.export_sink.is_some());
}

/// A submission counts as taken only while the device reports no new error and no fault: a device
/// lost during it reaches the device-lost callback, not the error count.
#[test]
fn a_submission_on_a_lost_device_is_not_taken_though_no_error_was_counted() {
    let health = craft_gpu_device::DeviceHealth::new();
    assert!(submission_accepted(&health, health.errors()));
    health.mark(craft_gpu_device::Fault::Lost("lost by the driver".into()));
    assert_eq!(health.errors(), 0, "a loss is not an uncaptured error");
    assert!(!submission_accepted(&health, 0));
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
    // The count of skipped copies reaches the app through RenderShared.
    let shared = RenderShared::default();
    c.scope_skipped.store(7, Ordering::Relaxed);
    c.run(&shared, Tick(600));
    c.finish(&shared.readback_lost);
    assert_eq!(shared.scope_skipped.load(Ordering::Relaxed), 7);
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
    // A frame drawn while the picture is read back does not change it.
    let green = solid([0, 255, 0, 255], 32, 16);
    shared.plan.publish(Arc::new(Plan { program: SceneDesc { items: vec![item(2, &green, Transform::default())] }, ..Plan::default() }));
    c.run(&shared, ms(33));
    c.finish(&shared.readback_lost);
    let shot = program_rx.try_recv().unwrap().unwrap();
    assert_eq!((shot.width, shot.height, shot.rgba.len()), (32, 16, 32 * 16 * 4));
    assert!(close(pixel(&shot.rgba, 32, 16, 8), [255, 0, 0, 255]));
    assert!(preview_rx.try_recv().unwrap().is_err(), "no preview outside studio mode");
    assert!(shared.captures.is_empty());
}

#[test]
fn a_frame_does_not_wait_for_a_capture_to_be_read_back() {
    let mut c = noop_compositor(32, 16);
    let shared = RenderShared::default();
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    assert!(shared.captures.push(CaptureRequest { view: View::Program, reply: tx }).is_ok());
    c.run(&shared, Tick(0));
    assert_eq!(rx.try_recv().err(), Some(std::sync::mpsc::TryRecvError::Empty), "the frame returned before the picture was read back");
    // The next frames are drawn while the read-back is outstanding.
    c.run(&shared, ms(33));
    c.run(&shared, ms(66));
    assert_eq!(c.frame_no, 3);
    c.finish(&shared.readback_lost);
    assert!(rx.try_recv().is_ok(), "the capture is answered");
}

#[test]
fn a_read_back_that_does_not_report_holds_its_slot_until_it_times_out() {
    use std::sync::mpsc::TryRecvError;
    let mut c = noop_compositor(32, 16);
    let shared = RenderShared::default();
    let mut answers = Vec::new();
    let mut ask = |shared: &RenderShared| {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let queued = shared.captures.push(CaptureRequest { view: View::Program, reply: tx }).is_ok();
        if queued {
            answers.push(rx);
        }
        queued
    };
    for _ in 0..CAPTURE_SLOTS {
        assert!(ask(&shared));
    }
    c.run(&shared, Tick(0));
    assert_eq!(c.reads.len(), CAPTURE_SLOTS);
    // Mappings that have not reported back: no callback holds these states.
    for read in &mut c.reads {
        read.state = Arc::new(AtomicU8::new(READ_PENDING));
    }
    // While every slot is being read back, new requests stay queued and the queue fills.
    for _ in 0..CAPTURE_SLOTS {
        assert!(ask(&shared));
    }
    c.run(&shared, ms(33));
    assert_eq!(shared.captures.len(), CAPTURE_SLOTS);
    assert!(!ask(&shared), "a full queue refuses the request");
    assert!(answers.iter().all(|rx| rx.try_recv().err() == Some(TryRecvError::Empty)));
    // Past the wait, counted from the frame at 33 ms, the stuck read-backs are answered with an
    // error, and the queued requests copy their pictures.
    c.run(&shared, tick_of(MAP_WAIT + std::time::Duration::from_millis(33)));
    for rx in &answers[..CAPTURE_SLOTS] {
        let err = rx.try_recv().unwrap().unwrap_err();
        assert!(err.to_string().contains("could not be read back"), "{err}");
    }
    assert!(shared.captures.is_empty());
    assert_eq!(c.reads.len(), CAPTURE_SLOTS);
    c.finish(&shared.readback_lost);
    assert!(c.reads.is_empty());
    assert!(answers[CAPTURE_SLOTS..].iter().all(|rx| rx.try_recv().is_ok()), "the queued captures are answered");
}

#[test]
fn the_read_back_wait_starts_after_the_frame_that_copied_the_picture() {
    use std::sync::mpsc::TryRecvError;
    let mut c = noop_compositor(32, 16);
    let shared = RenderShared::default();
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    assert!(shared.captures.push(CaptureRequest { view: View::Program, reply: tx }).is_ok());
    c.run(&shared, Tick(0));
    for read in &mut c.reads {
        read.state = Arc::new(AtomicU8::new(READ_PENDING));
    }
    // The frame stamped 0 submitted its copy late, so the next frame comes a whole wait after
    // that stamp. The wait starts at that next frame.
    c.run(&shared, tick_of(MAP_WAIT));
    assert_eq!(rx.try_recv().err(), Some(TryRecvError::Empty), "the read-back still has its whole wait");
    c.run(&shared, tick_of(MAP_WAIT * 2));
    let err = rx.try_recv().unwrap().unwrap_err();
    assert!(err.to_string().contains("could not be read back"), "{err}");
}

#[test]
fn a_lost_device_answers_captures_with_an_error_and_takes_no_new_targets() {
    // a device of its own on wgpu's noop adapter, since this one is destroyed
    let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
    desc.backends = wgpu::Backends::NOOP;
    desc.backend_options.noop = wgpu::NoopBackendOptions { enable: true, ..Default::default() };
    let (gpu, _) = Gpu::open_on(wgpu::Instance::new(desc), &crate::device::Preference::Software).unwrap();
    let mut c = Compositor::new(gpu.clone(), 32, 16).unwrap();
    let shared = RenderShared::default();
    gpu.device.destroy();
    gpu.poll();
    assert!(gpu.is_lost());
    shared.canvas.publish((64, 32));
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    assert!(shared.captures.push(CaptureRequest { view: View::Program, reply: tx }).is_ok());
    c.run(&shared, Tick(0));
    let err = rx.try_recv().unwrap().unwrap_err();
    assert!(err.to_string().contains("lost"), "{err}");
    assert!(c.capture(View::Program).is_err());
    assert_eq!(c.size(), (32, 16), "no canvas is made on a lost device");
    assert!(shared.captures.is_empty());
}

/// A compositor on wgpu's noop adapter, which draws nothing but keeps the bookkeeping, so the
/// thumbnail scheduling tests run on a machine without a graphics adapter too.
fn noop_compositor(w: u32, h: u32) -> Compositor {
    let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
    desc.backends = wgpu::Backends::NOOP;
    desc.backend_options.noop = wgpu::NoopBackendOptions { enable: true, ..Default::default() };
    let (gpu, _) = Gpu::open_on(wgpu::Instance::new(desc), &crate::device::Preference::Software).unwrap();
    Compositor::new(gpu, w, h).unwrap()
}

fn thumbnails(scenes: Vec<SceneDesc>, fps: f32) -> Option<Arc<ThumbnailRequest>> {
    Some(Arc::new(ThumbnailRequest { scenes, fps, ..ThumbnailRequest::default() }))
}

fn one_item_scene(id: u64, video: &Arc<VideoOut>) -> SceneDesc {
    SceneDesc { items: vec![item(id, video, Transform::default())] }
}

/// The newest picture index of each thumbnail slot.
fn drawn(c: &Compositor) -> Vec<Option<usize>> {
    c.thumbnail_views().map(|v| v.slots.iter().map(Ring::latest).collect()).unwrap_or_default()
}

fn ms(n: u64) -> Tick {
    studiocraft_media::clock::tick_of(std::time::Duration::from_millis(n))
}

#[test]
fn thumbnails_draw_each_scene_at_the_canvas_aspect() {
    let Some(mut c) = compositor(64, 32) else { return };
    let (red, green) = (solid([255, 0, 0, 255], 64, 32), solid([0, 255, 0, 255], 32, 32));
    let blue = solid([0, 0, 255, 255], 64, 32);
    c.set_plan(Arc::new(Plan { program: one_item_scene(3, &blue), ..Plan::default() }));
    let request =
        ThumbnailRequest { scenes: vec![one_item_scene(1, &red), one_item_scene(2, &green)], size: (16, 16), ..ThumbnailRequest::default() };
    c.set_thumbnails(Some(Arc::new(request)));
    c.frame(Tick(0), &AtomicU64::new(0));
    let views = c.thumbnail_views().unwrap();
    assert_eq!((views.width, views.height, views.slots.len()), (16, 8, 2));
    assert_eq!(views.slots[0].views().len(), RING_LEN);
    let first = c.read_thumbnail(0).unwrap().unwrap();
    assert_eq!(first.len(), 16 * 8 * 4);
    assert!(close(pixel(&first, 16, 8, 4), [255, 0, 0, 255]), "{:?}", pixel(&first, 16, 8, 4));
    // The green item covers the left half of the canvas, so of the picture too.
    let second = c.read_thumbnail(1).unwrap().unwrap();
    assert!(close(pixel(&second, 16, 3, 4), [0, 255, 0, 255]), "{:?}", pixel(&second, 16, 3, 4));
    assert!(close(pixel(&second, 16, 12, 4), [0, 0, 0, 255]), "{:?}", pixel(&second, 16, 12, 4));
    assert_eq!(c.read_thumbnail(2).unwrap(), None);
    // The program is drawn as without thumbnails.
    let program = c.read_program().unwrap();
    assert!(close(pixel(&program, 64, 32, 16), [0, 0, 255, 255]));
}

#[test]
fn thumbnail_filters_run_and_keep_their_pictures_when_the_plan_changes() {
    let Some(mut c) = compositor(16, 16) else { return };
    let v = solid([0, 255, 0, 255], 16, 16);
    let mut params = [0.0; VIDEO_EFFECT_PARAMS];
    params[0] = 1.0;
    let mut inverted = item(1, &v, Transform::default());
    inverted.effects = vec![VideoEffect::new("test.invert", INVERT, params)];
    let mut pictured = item(2, &solid([0, 255, 0, 255], 16, 16), Transform::default());
    pictured.effects = vec![VideoEffect { image: Some(red_and_blue()), ..VideoEffect::new("test.picture", PICTURE, [0.0; VIDEO_EFFECT_PARAMS]) }];
    let request = ThumbnailRequest {
        scenes: vec![SceneDesc { items: vec![inverted] }, SceneDesc { items: vec![pictured] }],
        size: (16, 16),
        ..ThumbnailRequest::default()
    };
    c.set_thumbnails(Some(Arc::new(request)));
    // A new plan without these filters keeps the picture the thumbnails read.
    c.set_plan(Arc::new(Plan::default()));
    assert_eq!(c.images.len(), 1);
    c.frame(Tick(0), &AtomicU64::new(0));
    let px = c.read_thumbnail(0).unwrap().unwrap();
    assert!(close(pixel(&px, 16, 8, 8), [255, 0, 255, 255]), "{:?}", pixel(&px, 16, 8, 8));
    let px = c.read_thumbnail(1).unwrap().unwrap();
    assert!(close(pixel(&px, 16, 2, 8), [255, 0, 0, 255]), "{:?}", pixel(&px, 16, 2, 8));
    assert!(close(pixel(&px, 16, 13, 8), [0, 0, 255, 255]), "{:?}", pixel(&px, 16, 13, 8));
    assert!(c.take_errors().is_empty());
    // Without the thumbnails the picture is released.
    c.set_thumbnails(None);
    assert!(c.images.is_empty());
}

#[test]
fn thumbnails_are_redrawn_at_most_at_their_rate() {
    let mut c = noop_compositor(32, 18);
    let v = solid([255, 0, 0, 255], 32, 18);
    c.set_thumbnails(thumbnails(vec![one_item_scene(1, &v)], 2.0));
    let lost = AtomicU64::new(0);
    assert_eq!(drawn(&c), [None], "the request makes the textures, and frames draw into them");
    c.frame(ms(1000), &lost);
    assert_eq!(drawn(&c), [Some(0)], "the first frame draws");
    c.frame(ms(1017), &lost);
    c.frame(ms(1499), &lost);
    assert_eq!(drawn(&c), [Some(0)], "not again within half a second");
    c.frame(ms(1500), &lost);
    assert_eq!(drawn(&c), [Some(1)]);
    // A new request does not draw sooner than the rate allows.
    c.set_thumbnails(thumbnails(vec![one_item_scene(1, &v)], 2.0));
    c.frame(ms(1600), &lost);
    assert_eq!(drawn(&c), [Some(1)]);
    c.frame(ms(2000), &lost);
    assert_eq!(drawn(&c), [Some(2)]);
    // A rate above the cap is held to it.
    c.set_thumbnails(thumbnails(vec![one_item_scene(1, &v)], 60.0));
    c.frame(ms(2100), &lost);
    assert_eq!(drawn(&c), [Some(2)]);
    c.frame(ms(2250), &lost);
    assert_eq!(drawn(&c), [Some(0)]);
    // A clock that went back draws instead of waiting for the old time.
    c.frame(ms(10), &lost);
    assert_eq!(drawn(&c), [Some(1)]);
}

#[test]
fn thumbnails_are_spread_over_frames_oldest_first() {
    let mut c = noop_compositor(32, 18);
    let scenes = (0..3).map(|i| one_item_scene(i, &solid([255, 0, 0, 255], 32, 18))).collect();
    c.set_thumbnails(thumbnails(scenes, 4.0));
    let lost = AtomicU64::new(0);
    c.frame(ms(0), &lost);
    assert_eq!(drawn(&c), [Some(0), Some(0), None], "two pictures a frame");
    c.frame(ms(1), &lost);
    assert_eq!(drawn(&c), [Some(0), Some(0), Some(0)]);
    c.frame(ms(2), &lost);
    assert_eq!(drawn(&c), [Some(0), Some(0), Some(0)], "nothing is due");
    c.frame(ms(251), &lost);
    assert_eq!(drawn(&c), [Some(1), Some(1), Some(0)], "all three are due; the slot drawn last waits");
    // Slot 2 waited, so it is now the oldest and goes first, then the lower of the two others.
    c.frame(ms(600), &lost);
    assert_eq!(drawn(&c), [Some(2), Some(1), Some(1)]);
}

#[test]
fn a_slot_whose_scene_changed_is_drawn_without_waiting_for_the_rate() {
    let mut c = noop_compositor(32, 18);
    let (a, b) = (solid([255, 0, 0, 255], 32, 18), solid([0, 255, 0, 255], 32, 18));
    let lost = AtomicU64::new(0);
    c.set_thumbnails(thumbnails(vec![one_item_scene(1, &a), one_item_scene(2, &b)], 2.0));
    c.frame(ms(0), &lost);
    assert_eq!(drawn(&c), [Some(0), Some(0)]);
    // A moved item keeps its slot's rate.
    let moved = SceneDesc { items: vec![item(1, &a, at(4.0, 4.0))] };
    c.set_thumbnails(thumbnails(vec![moved, one_item_scene(2, &b)], 2.0));
    c.frame(ms(100), &lost);
    assert_eq!(drawn(&c), [Some(0), Some(0)]);
    // Swapped scenes are drawn in their new slots at once.
    c.set_thumbnails(thumbnails(vec![one_item_scene(2, &b), one_item_scene(1, &a)], 2.0));
    c.frame(ms(200), &lost);
    assert_eq!(drawn(&c), [Some(1), Some(1)]);
    // An item added to a scene changes its slot too.
    let two = SceneDesc { items: vec![item(2, &b, Transform::default()), item(1, &a, Transform::default())] };
    c.set_thumbnails(thumbnails(vec![two, one_item_scene(1, &a)], 2.0));
    c.frame(ms(300), &lost);
    assert_eq!(drawn(&c), [Some(2), Some(1)]);
}

#[test]
fn a_slot_whose_key_changed_is_drawn_without_waiting_for_the_rate() {
    let mut c = noop_compositor(32, 18);
    let a = solid([255, 0, 0, 255], 32, 18);
    let lost = AtomicU64::new(0);
    let keyed = |keys: Vec<u64>, x: f32| {
        let scenes = vec![SceneDesc { items: vec![item(1, &a, at(x, 0.0))] }, one_item_scene(1, &a)];
        Some(Arc::new(ThumbnailRequest { scenes, keys, ..ThumbnailRequest::default() }))
    };
    c.set_thumbnails(keyed(vec![7, 8], 0.0));
    c.frame(ms(0), &lost);
    assert_eq!(drawn(&c), [Some(0), Some(0)]);
    // The same keys keep the rate.
    c.set_thumbnails(keyed(vec![7, 8], 4.0));
    c.frame(ms(100), &lost);
    assert_eq!(drawn(&c), [Some(0), Some(0)]);
    // Another scene with the same sources in slot 0 is drawn at once; slot 1 waits.
    c.set_thumbnails(keyed(vec![9, 8], 8.0));
    c.frame(ms(200), &lost);
    assert_eq!(drawn(&c), [Some(1), Some(0)]);
    // A key that is dropped counts as a change too.
    c.set_thumbnails(keyed(vec![9], 8.0));
    c.frame(ms(300), &lost);
    assert_eq!(drawn(&c), [Some(1), Some(1)]);
}

#[test]
fn a_request_without_a_rate_draws_once() {
    let mut c = noop_compositor(32, 18);
    let v = solid([255, 0, 0, 255], 32, 18);
    let lost = AtomicU64::new(0);
    for fps in [0.0, -3.0, f32::NAN] {
        c.set_thumbnails(thumbnails(vec![one_item_scene(1, &v)], fps));
        let before = drawn(&c).first().copied().flatten();
        c.frame(ms(5000), &lost);
        let after = drawn(&c)[0];
        assert_ne!(after, before, "fps {fps} draws with the next frame");
        for t in [6000, 60_000] {
            c.frame(ms(t), &lost);
        }
        assert_eq!(drawn(&c)[0], after, "fps {fps} draws only once");
    }
}

#[test]
fn a_thumbnail_drawn_once_waits_for_a_shared_picture_still_being_copied() {
    let mut c = noop_compositor(32, 18);
    let v = solid([255, 0, 0, 255], 32, 18);
    c.set_thumbnails(thumbnails(vec![one_item_scene(1, &v)], 0.0));
    c.frame(ms(5000), &AtomicU64::new(0));
    assert_eq!(c.thumbnails_due(ms(6000)).1, 0, "drawn once");
    // The source's shared picture was still being copied when the slot was drawn.
    let ring = craft_gpu_interop::SharedRing::detached(2, 2);
    let st = c.sources.get_mut(&SourceId(1)).unwrap();
    st.shared.get_or_insert_with(Default::default).offer(craft_gpu_interop::SharedFrame::new(ring, 0, 1).unwrap());
    c.thumbnails_drawn(&[0], ms(6000));
    assert_eq!(c.thumbnails_due(ms(7000)), ([0, 0], 1), "the slot is drawn again");
    // The picture was drawn: the slot is done.
    c.sources.get_mut(&SourceId(1)).unwrap().shared.as_mut().unwrap().clear();
    c.thumbnails_drawn(&[0], ms(7000));
    assert_eq!(c.thumbnails_due(ms(8000)).1, 0);
}

#[test]
fn a_shared_picture_is_drawn_into_the_view_made_with_the_texture() {
    let c = noop_compositor(8, 8);
    let format = wgpu::TextureFormat::Bgra8Unorm;
    let mut slot = None;
    let first = shared_target(&c.gpu.device, &c.pipes, &mut slot, (4, 4), format);
    assert_eq!(shared_target(&c.gpu.device, &c.pipes, &mut slot, (4, 4), format), first, "no view per frame");
    let resized = shared_target(&c.gpu.device, &c.pipes, &mut slot, (8, 4), format);
    assert_ne!(resized, first, "a new size makes a new texture and view");
    assert_eq!(slot.map(|t| t.view), Some(resized));
}

#[test]
fn a_removed_or_empty_request_stops_drawing() {
    let mut c = noop_compositor(32, 18);
    let v = solid([255, 0, 0, 255], 32, 18);
    let lost = AtomicU64::new(0);
    c.set_thumbnails(thumbnails(vec![], 2.0));
    c.frame(ms(0), &lost);
    assert!(c.thumbnail_views().is_none(), "an empty request makes no textures");
    c.set_thumbnails(thumbnails(vec![one_item_scene(1, &v)], 4.0));
    c.frame(ms(0), &lost);
    assert_eq!(drawn(&c), [Some(0)]);
    c.set_thumbnails(None);
    for t in 1..10 {
        c.frame(ms(t * 1000), &lost);
    }
    assert_eq!(drawn(&c), [Some(0)], "the textures are kept with their last picture");
    c.set_thumbnails(thumbnails(vec![], 4.0));
    c.frame(ms(20_000), &lost);
    assert_eq!(drawn(&c), [Some(0)]);
}

#[test]
fn the_thumbnail_textures_are_kept_until_the_size_or_count_outgrows_them() {
    let mut c = noop_compositor(1920, 1080);
    let v = solid([255, 0, 0, 255], 32, 18);
    let lost = AtomicU64::new(0);
    let scenes = |n: u64| (0..n).map(|i| one_item_scene(i, &v)).collect::<Vec<_>>();
    c.set_thumbnails(thumbnails(scenes(3), 4.0));
    c.frame(ms(0), &lost);
    c.frame(ms(16), &lost);
    let first = c.thumbnail_views().unwrap();
    assert_eq!((first.width, first.height, first.slots.len()), (256, 144, 3));
    // Fewer scenes keep the textures, and only their slots are drawn.
    c.set_thumbnails(thumbnails(scenes(2), 4.0));
    for t in [266, 282, 298] {
        c.frame(ms(t), &lost);
    }
    assert_eq!(c.thumbnail_views().unwrap().generation, first.generation);
    assert_eq!(drawn(&c), [Some(1), Some(1), Some(0)]);
    // More scenes make a larger set.
    c.set_thumbnails(thumbnails(scenes(5), 4.0));
    c.frame(ms(500), &lost);
    let more = c.thumbnail_views().unwrap();
    assert_eq!((more.slots.len(), more.generation), (5, first.generation + 1));
    // The new textures are filled over the next frames.
    assert_eq!(drawn(&c), [Some(0), Some(0), None, None, None]);
    c.frame(ms(516), &lost);
    c.frame(ms(532), &lost);
    assert_eq!(drawn(&c), [Some(0); 5]);
    // A canvas of another shape makes pictures of its shape at once, while no slot is due, and
    // every slot of the new textures is drawn without waiting for its rate: slots 2 and 3 were
    // drawn 244 ms before the frame at 760 ms.
    c.set_canvas(1080, 1920);
    let tall = c.thumbnail_views().unwrap();
    assert_eq!((tall.width, tall.height, tall.slots.len(), tall.generation), (81, 144, 5, more.generation + 1));
    assert_eq!(drawn(&c), [None; 5]);
    c.frame(ms(750), &lost);
    assert_eq!(drawn(&c), [Some(0), Some(0), None, None, None]);
    c.frame(ms(760), &lost);
    assert_eq!(drawn(&c), [Some(0), Some(0), Some(0), Some(0), None]);
}

#[test]
fn a_request_draws_sixteen_scenes_and_the_app_sees_the_rest_counted() {
    let mut c = noop_compositor(64, 36);
    let shared = RenderShared::default();
    let v = solid([255, 0, 0, 255], 32, 18);
    let scenes: Vec<_> = (0..MAX_THUMBNAILS as u64 + 2).map(|i| one_item_scene(i, &v)).collect();
    shared.thumbnails.publish(thumbnails(scenes, 2.0));
    c.run(&shared, ms(0));
    assert_eq!(shared.thumbnails_skipped.load(Ordering::Relaxed), 2);
    assert_eq!(c.thumbnails_skipped(), 2);
    let views = shared.thumbnail_views.take().expect("new textures are published");
    assert_eq!(views.slots.len(), MAX_THUMBNAILS);
    // Two pictures a frame: eight frames draw all sixteen, and the kept textures are not
    // published again.
    for t in 1..8 {
        assert_eq!(views.slots.iter().filter(|r| r.latest().is_some()).count(), 2 * t as usize);
        c.run(&shared, ms(t));
    }
    assert!(views.slots.iter().all(|r| r.latest() == Some(0)));
    assert!(shared.thumbnail_views.take().is_none());
    shared.thumbnails.publish(None);
    c.run(&shared, ms(1000));
    assert_eq!(shared.thumbnails_skipped.load(Ordering::Relaxed), 0);
}
