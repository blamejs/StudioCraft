//! WGSL for the compositor's passes. Every pass reads one 144-byte uniform slot (`u`) at a
//! dynamic offset in one buffer written once per frame.

/// Shared by every module: the uniform slot and the full-canvas triangle.
const COMMON: &str = r"
struct Slot { a: vec4<f32>, b: vec4<f32>, c: vec4<f32>, d: vec4<f32>, e: vec4<f32>, f: vec4<f32>, g: vec4<f32>, h: vec4<f32>, i: vec4<f32> }
@group(0) @binding(0) var<uniform> u: Slot;

struct VOut { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> }

// One triangle covering the target; uv (0, 0) is the top-left corner.
@vertex fn vs_full(@builtin(vertex_index) vi: u32) -> VOut {
    let x = f32((vi << 1u) & 2u);
    let y = f32(vi & 2u);
    var o: VOut;
    o.pos = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    o.uv = vec2<f32>(x, y);
    return o;
}
";

/// Items: a textured quad placed by four corners in clip space.
///
/// `a` = corners 0 and 1, `b` = corners 2 and 3 (clockwise from the top left), `c` = the texture
/// rectangle (u0, v0, u1, v1), which runs backward on a flipped axis, `d.x` = 1 when the texture holds straight alpha, `d.y` = opacity.
const ITEM: &str = r"
@group(1) @binding(0) var tex: texture_2d<f32>;
@group(1) @binding(1) var samp: sampler;

@vertex fn vs_item(@builtin(vertex_index) vi: u32) -> VOut {
    var corner = array<u32, 6>(0u, 1u, 2u, 0u, 2u, 3u);
    let k = corner[vi];
    var o: VOut;
    var p = u.a.xy;
    var t = u.c.xy;
    if k == 1u { p = u.a.zw; t = vec2<f32>(u.c.z, u.c.y); }
    if k == 2u { p = u.b.xy; t = u.c.zw; }
    if k == 3u { p = u.b.zw; t = vec2<f32>(u.c.x, u.c.w); }
    o.pos = vec4<f32>(p, 0.0, 1.0);
    o.uv = t;
    return o;
}

@fragment fn fs_item(v: VOut) -> @location(0) vec4<f32> {
    // Keep the linear filter inside the crop: texels cut away never blend into the edge. A
    // flipped item's rectangle runs backward, so the bounds come from its smaller and larger ends.
    let half = 0.5 / vec2<f32>(textureDimensions(tex));
    let lo = min(u.c.xy, u.c.zw) + half;
    let hi = max(u.c.xy, u.c.zw) - half;
    let uv = clamp(v.uv, min(lo, hi), max(lo, hi));
    var c = textureSample(tex, samp, uv);
    if u.d.x > 0.5 { c = vec4<f32>(c.rgb * c.a, c.a); }
    return c * u.d.y;
}
";

/// Filter passes: `a` = the rectangle of `tex` under the output, `b.xy` = the output size in
/// pixels, `b.z` = 1 when the input holds straight alpha, `c` to `h` = the filter's parameters,
/// `i.xy` = the input's extent in `tex` (the input starts at the top-left corner) and `i.zw` =
/// the input's size in pixels. `c` and `d` reach `effect` as `p0` and `p1`; `extra_params()`
/// returns `e` to `h` as the columns of a matrix.
const EFFECT_HEAD: &str = r"
@group(1) @binding(0) var tex: texture_2d<f32>;
@group(1) @binding(1) var samp: sampler;

fn extra_params() -> mat4x4<f32> {
    return mat4x4<f32>(u.e, u.f, u.g, u.h);
}

fn input_size() -> vec2<f32> {
    return u.i.zw;
}

// The input's premultiplied color at `st` in `tex`; transparent outside the input. The linear
// filter stays inside the input, so texels beyond it never blend into its edge.
fn input_at(st: vec2<f32>) -> vec4<f32> {
    let half = 0.5 / vec2<f32>(textureDimensions(tex));
    let lo = half;
    let hi = max(u.i.xy - half, lo);
    var c = textureSampleLevel(tex, samp, clamp(st, lo, hi), 0.0);
    if u.b.z > 0.5 { c = vec4<f32>(c.rgb * c.a, c.a); }
    let inside = all(st >= vec2<f32>(0.0)) && all(st <= u.i.xy);
    return select(vec4<f32>(0.0), c, inside);
}

// The input's color at pixel position `px` (pixel centers at .5), clamped to its edges.
fn input_pixel(px: vec2<f32>) -> vec4<f32> {
    let size = max(input_size(), vec2<f32>(1.0));
    return input_at(clamp(px, vec2<f32>(0.5), size - 0.5) / size * u.i.xy);
}
";

/// Filter passes with a picture beside their input: the picture and its sampler in group 2.
const EFFECT_IMAGE_HEAD: &str = r"
@group(2) @binding(0) var image_tex: texture_2d<f32>;
@group(2) @binding(1) var image_samp: sampler;

// The picture's premultiplied color at `st` (0 to 1 across it); transparent outside it.
fn image_at(st: vec2<f32>) -> vec4<f32> {
    let c = textureSampleLevel(image_tex, image_samp, clamp(st, vec2<f32>(0.0), vec2<f32>(1.0)), 0.0);
    let inside = all(st >= vec2<f32>(0.0)) && all(st <= vec2<f32>(1.0));
    return select(vec4<f32>(0.0), c, inside);
}
";

const EFFECT_MAIN: &str = r"
@fragment fn fs_effect(v: VOut) -> @location(0) vec4<f32> {
    return effect(input_at(mix(u.a.xy, u.a.zw, v.uv)), v.uv, u.b.xy, u.c, u.d);
}
";

/// Transitions: `a.x` = progress, `b` = the transition's parameters.
const TRANSITION_HEAD: &str = r"
@group(1) @binding(0) var from_tex: texture_2d<f32>;
@group(1) @binding(1) var to_tex: texture_2d<f32>;
@group(1) @binding(2) var samp: sampler;
";

const TRANSITION_MAIN: &str = r"
@fragment fn fs_transition(v: VOut) -> @location(0) vec4<f32> {
    let a = textureSample(from_tex, samp, v.uv);
    let b = textureSample(to_tex, samp, v.uv);
    return transition(a, b, v.uv, clamp(u.a.x, 0.0, 1.0), u.b);
}
";

pub fn item() -> String {
    format!("{COMMON}{ITEM}")
}

pub fn effect(body: &str) -> String {
    format!("{COMMON}{EFFECT_HEAD}{body}\n{EFFECT_MAIN}")
}

pub fn effect_with_image(body: &str) -> String {
    format!("{COMMON}{EFFECT_HEAD}{EFFECT_IMAGE_HEAD}{body}\n{EFFECT_MAIN}")
}

pub fn transition(body: &str) -> String {
    format!("{COMMON}{TRANSITION_HEAD}{body}\n{TRANSITION_MAIN}")
}
