//! The compositor: GPU state and the per-frame passes.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::SyncSender;

use crossbeam_queue::ArrayQueue;
use studiocraft_media::Tick;
use studiocraft_media::queue::{Fifo, Latest};
use studiocraft_media::video::PixelFormat;
use studiocraft_model::{Crop, SourceId, Vec2};
use studiocraft_plugin::{VideoContent, VideoEffect, VideoOut};

use crate::nv12::{EncodeTarget, Nv12};
use crate::plan::{ItemDesc, Plan, SceneDesc};
use crate::{CANVAS_FORMAT, CANVAS_VIEW_FORMAT, Gpu, RenderError, shaders};

/// Values in one uniform slot.
const SLOT_FLOATS: usize = 32;
/// Size of one uniform slot.
const SLOT_BYTES: u64 = SLOT_FLOATS as u64 * 4;
/// Canvas textures per ring: the interface reads the newest while the compositor draws the next.
const RING_LEN: usize = 3;
/// Frames between sweeps of textures whose source is gone.
const SWEEP_FRAMES: u64 = 120;
/// Longest wait for a read-back buffer to map after the GPU finished.
const MAP_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// Mailboxes between the compositor thread and the rest of the app.
pub struct RenderShared {
    /// What to draw.
    pub plan: Latest<Arc<Plan>>,
    /// A new canvas size.
    pub canvas: Latest<(u32, u32)>,
    /// Start, change or stop converting frames for encoders.
    pub encode: Latest<Option<EncodeTarget>>,
    /// Start, change or stop the small NV12 copy of the program that the scopes read.
    pub scope: Latest<Option<EncodeTarget>>,
    /// New canvas textures for the interface, published after a resize.
    pub views: Latest<Arc<CanvasViews>>,
    /// Frames encoders never received because the read-back fell behind.
    pub readback_lost: AtomicU64,
    /// Program copies the scopes never received because their read-back fell behind.
    pub scope_skipped: AtomicU64,
    /// Shader and device errors for the interface to show.
    pub errors: Fifo<RenderError>,
    /// Still captures waiting for the next frame. A full queue refuses new requests.
    pub captures: ArrayQueue<CaptureRequest>,
}

/// Still capture requests waiting at once.
pub const CAPTURE_SLOTS: usize = 2;

impl Default for RenderShared {
    fn default() -> Self {
        Self {
            plan: Latest::new(),
            canvas: Latest::new(),
            encode: Latest::new(),
            scope: Latest::new(),
            views: Latest::new(),
            readback_lost: AtomicU64::new(0),
            scope_skipped: AtomicU64::new(0),
            errors: Fifo::new(16),
            captures: ArrayQueue::new(CAPTURE_SLOTS),
        }
    }
}

/// Which canvas picture a still capture reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    /// The picture viewers see.
    Program,
    /// The scene being prepared in studio mode.
    Preview,
}

/// A still picture of the canvas: tightly packed RGBA rows with straight alpha.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capture {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// A request for a still picture, answered after the next frame is drawn.
pub struct CaptureRequest {
    pub view: View,
    pub reply: SyncSender<Result<Capture, RenderError>>,
}

/// Convert premultiplied RGBA to straight alpha in place.
pub fn unpremultiply(rgba: &mut [u8]) {
    for px in rgba.as_chunks_mut::<4>().0 {
        let a = u32::from(px[3]);
        if a == 0 {
            px[..3].fill(0);
        } else if a < 255 {
            for c in &mut px[..3] {
                *c = ((u32::from(*c) * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
}

/// Canvas textures the interface shows, sampled through sRGB views.
pub struct Ring {
    views: Vec<wgpu::TextureView>,
    latest: AtomicUsize,
    drawn: AtomicBool,
}

impl Ring {
    /// All views of the ring, to register with the interface's renderer once.
    pub fn views(&self) -> &[wgpu::TextureView] {
        &self.views
    }

    /// The index of the newest finished picture, if one was drawn.
    pub fn latest(&self) -> Option<usize> {
        self.drawn.load(Ordering::Acquire).then(|| self.latest.load(Ordering::Acquire))
    }

    fn publish(&self, index: usize) {
        self.latest.store(index, Ordering::Release);
        self.drawn.store(true, Ordering::Release);
    }
}

/// The program and preview pictures of one canvas size.
pub struct CanvasViews {
    pub width: u32,
    pub height: u32,
    /// Changes whenever the textures are recreated.
    pub generation: u64,
    pub program: Ring,
    pub preview: Ring,
}

/// A canvas-size render target and the bind group that samples it.
struct Target {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    bind: wgpu::BindGroup,
}

/// A source's texture on the GPU.
struct Tex {
    texture: wgpu::Texture,
    bind: wgpu::BindGroup,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
}

/// Two textures the filter passes of one source alternate between.
struct FxPair {
    width: u32,
    height: u32,
    views: [wgpu::TextureView; 2],
    binds: [wgpu::BindGroup; 2],
}

struct SourceTex {
    video: Arc<VideoOut>,
    tex: Option<Tex>,
    /// The size the source shows at (a solid's size, or the frame's).
    native: (u32, u32),
    solid: Option<[u8; 4]>,
    fx: Option<FxPair>,
    /// The frame the filter passes last ran, and which texture of `fx` holds the result.
    fx_frame: u64,
    fx_result: usize,
}

struct Pipes {
    uniform_layout: wgpu::BindGroupLayout,
    tex_layout: wgpu::BindGroupLayout,
    tex2_layout: wgpu::BindGroupLayout,
    layout: wgpu::PipelineLayout,
    layout2: wgpu::PipelineLayout,
    item: wgpu::RenderPipeline,
    nv12_y: wgpu::RenderPipeline,
    nv12_uv: wgpu::RenderPipeline,
    effects: HashMap<&'static str, Option<wgpu::RenderPipeline>>,
    transitions: HashMap<String, Option<wgpu::RenderPipeline>>,
    sampler: wgpu::Sampler,
}

/// Per-pass uniforms, written to the GPU once per frame.
struct Uniforms {
    buffer: wgpu::Buffer,
    bind: wgpu::BindGroup,
    stride: u64,
    capacity: u64,
    staging: Vec<u8>,
}

impl Uniforms {
    fn new(device: &wgpu::Device, layout: &wgpu::BindGroupLayout, stride: u64, capacity: u64) -> Uniforms {
        let capacity = capacity.max(16);
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("pass uniforms"),
            size: stride * capacity,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let binding = wgpu::BufferBinding { buffer: &buffer, offset: 0, size: wgpu::BufferSize::new(SLOT_BYTES) };
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("pass uniforms"),
            layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::Buffer(binding) }],
        });
        let staging = Vec::with_capacity((stride * capacity) as usize);
        Uniforms { buffer, bind, stride, capacity, staging }
    }

    /// Start a frame needing up to `slots` slots.
    fn begin(&mut self, device: &wgpu::Device, layout: &wgpu::BindGroupLayout, slots: u64) {
        if slots > self.capacity {
            *self = Uniforms::new(device, layout, self.stride, slots.next_power_of_two());
        }
        self.staging.clear();
    }

    /// Add a slot of up to `SLOT_FLOATS` values (the rest are 0); its dynamic offset.
    fn push(&mut self, slot: &[f32]) -> u32 {
        let offset = self.staging.len();
        self.staging.extend_from_slice(bytemuck::cast_slice(&slot[..slot.len().min(SLOT_FLOATS)]));
        self.staging.resize(offset + self.stride as usize, 0);
        offset as u32
    }

    fn flush(&self, queue: &wgpu::Queue) {
        if !self.staging.is_empty() && self.staging.len() as u64 <= self.buffer.size() {
            queue.write_buffer(&self.buffer, 0, &self.staging);
        }
    }
}

/// The compositor. It lives on the frame thread; the app talks to it through [`RenderShared`].
pub struct Compositor {
    gpu: Gpu,
    pipes: Pipes,
    uniforms: Uniforms,
    width: u32,
    height: u32,
    program: Vec<Target>,
    preview: Vec<Target>,
    /// The two scenes of a transition.
    scratch: Vec<Target>,
    scratch_bind: wgpu::BindGroup,
    views: Arc<CanvasViews>,
    generation: u64,
    sources: HashMap<SourceId, SourceTex>,
    plan: Arc<Plan>,
    nv12: Option<Nv12>,
    /// The scopes' copy of the program, converted like the encoders' at a smaller size.
    scope_nv12: Option<Nv12>,
    /// Frames the scopes did not get because their read-back fell behind.
    scope_skipped: AtomicU64,
    frame_no: u64,
    errors: Vec<RenderError>,
    /// Captures asked for before this frame's plan was taken: they are answered with this frame.
    waiting: Vec<CaptureRequest>,
}

/// Largest canvas side.
pub const MAX_CANVAS: u32 = 8192;

fn clamp_canvas(width: u32, height: u32, limit: u32) -> (u32, u32) {
    let max = limit.min(MAX_CANVAS);
    (width.clamp(16, max), height.clamp(16, max))
}

impl Compositor {
    /// A compositor drawing a `width` x `height` canvas.
    pub fn new(gpu: Gpu, width: u32, height: u32) -> Result<Compositor, RenderError> {
        let pipes = gpu.scoped("compositor", || make_pipes(&gpu))?;
        let align = u64::from(gpu.device.limits().min_uniform_buffer_offset_alignment).max(SLOT_BYTES);
        let stride = SLOT_BYTES.div_ceil(align) * align;
        let uniforms = Uniforms::new(&gpu.device, &pipes.uniform_layout, stride, 64);
        let (width, height) = clamp_canvas(width, height, gpu.device.limits().max_texture_dimension_2d);
        let (program, preview, scratch, scratch_bind, views) = make_canvas(&gpu, &pipes, width, height, 1);
        Ok(Compositor {
            gpu,
            pipes,
            uniforms,
            width,
            height,
            program,
            preview,
            scratch,
            scratch_bind,
            views,
            generation: 1,
            sources: HashMap::new(),
            plan: Arc::new(Plan::default()),
            nv12: None,
            scope_nv12: None,
            scope_skipped: AtomicU64::new(0),
            frame_no: 0,
            errors: Vec::new(),
            waiting: Vec::with_capacity(CAPTURE_SLOTS),
        })
    }

    pub fn gpu(&self) -> &Gpu {
        &self.gpu
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// The canvas textures the interface shows.
    pub fn views(&self) -> Arc<CanvasViews> {
        Arc::clone(&self.views)
    }

    /// Errors since the last call (shaders that failed to compile).
    pub fn take_errors(&mut self) -> Vec<RenderError> {
        std::mem::take(&mut self.errors)
    }

    /// Resize the canvas; the new textures are in [`Compositor::views`].
    pub fn set_canvas(&mut self, width: u32, height: u32) {
        let (width, height) = clamp_canvas(width, height, self.gpu.device.limits().max_texture_dimension_2d);
        if (width, height) == (self.width, self.height) {
            return;
        }
        self.generation += 1;
        let (program, preview, scratch, scratch_bind, views) = make_canvas(&self.gpu, &self.pipes, width, height, self.generation);
        (self.width, self.height) = (width, height);
        (self.program, self.preview, self.scratch, self.scratch_bind, self.views) = (program, preview, scratch, scratch_bind, views);
    }

    /// Draw `plan` from the next frame on, compiling the shaders it needs first.
    pub fn set_plan(&mut self, plan: Arc<Plan>) {
        for item in plan.items() {
            for fx in &item.effects {
                if !self.pipes.effects.contains_key(fx.key) {
                    let p = self.compile(fx.key, &shaders::effect(fx.wgsl), false, "fs_effect");
                    self.pipes.effects.insert(fx.key, p);
                }
            }
        }
        if let Some(t) = &plan.transition
            && !self.pipes.transitions.contains_key(&t.key)
        {
            let p = self.compile(&t.key, &shaders::transition(t.wgsl), true, "fs_transition");
            self.pipes.transitions.insert(t.key.clone(), p);
        }
        self.plan = plan;
    }

    fn compile(&mut self, key: &str, source: &str, two_textures: bool, fs: &str) -> Option<wgpu::RenderPipeline> {
        let layout = if two_textures { &self.pipes.layout2 } else { &self.pipes.layout };
        let made = self.gpu.scoped(key, || {
            let module = self
                .gpu
                .device
                .create_shader_module(wgpu::ShaderModuleDescriptor { label: Some(key), source: wgpu::ShaderSource::Wgsl(source.into()) });
            pipeline(&self.gpu.device, key, layout, &module, "vs_full", fs, CANVAS_FORMAT, None)
        });
        match made {
            Ok(p) => Some(p),
            Err(e) => {
                log::warn!("{e}");
                self.errors.push(e);
                None
            }
        }
    }

    /// Start, change or stop converting frames for encoders.
    pub fn set_encode(&mut self, target: Option<EncodeTarget>) {
        self.nv12 = retarget(&self.gpu.device, self.nv12.take(), target);
    }

    /// Start, change or stop the scopes' NV12 copy of the program.
    pub fn set_scope(&mut self, target: Option<EncodeTarget>) {
        self.scope_nv12 = retarget(&self.gpu.device, self.scope_nv12.take(), target);
    }

    /// Frames the scopes' copy skipped because all its read-back buffers were in flight.
    pub fn scope_skipped(&self) -> u64 {
        self.scope_skipped.load(Ordering::Relaxed)
    }

    /// Apply what the app published, then draw a frame.
    pub fn run(&mut self, shared: &RenderShared, now: Tick) {
        if let Some((w, h)) = shared.canvas.take() {
            let generation = self.generation;
            self.set_canvas(w, h);
            if self.generation != generation {
                shared.views.publish(self.views());
            }
        }
        if let Some(e) = shared.encode.take() {
            self.set_encode(e);
        }
        if let Some(s) = shared.scope.take() {
            self.set_scope(s);
        }
        // A capture asked for after the plan is taken waits for the next frame, so it shows every
        // change published before it was asked for.
        while let Some(req) = shared.captures.pop() {
            self.waiting.push(req);
        }
        if let Some(p) = shared.plan.take() {
            self.set_plan(p);
        }
        for e in self.errors.drain(..) {
            shared.errors.push(e);
        }
        self.frame(now, &shared.readback_lost);
        shared.scope_skipped.store(self.scope_skipped(), Ordering::Relaxed);
        let mut waiting = std::mem::take(&mut self.waiting);
        for req in waiting.drain(..) {
            // A caller that gave up has dropped its receiver; nothing waits for the answer.
            let _ = req.reply.try_send(self.capture(req.view));
        }
        self.waiting = waiting;
    }

    /// The newest picture of `view` with straight alpha. Waits for the GPU.
    pub fn capture(&self, view: View) -> Result<Capture, RenderError> {
        let mut rgba = match view {
            View::Program => self.read_program()?,
            View::Preview => self.read_preview()?.ok_or_else(|| RenderError::Device("no preview picture has been drawn".into()))?,
        };
        unpremultiply(&mut rgba);
        Ok(Capture { width: self.width, height: self.height, rgba })
    }

    /// Draw one frame stamped `now`.
    pub fn frame(&mut self, now: Tick, readback_lost: &AtomicU64) {
        self.frame_no += 1;
        let plan = Arc::clone(&self.plan);
        // A uniform slot for each item and effect, and one for each fixed pass: the transition and
        // the encoders' and the scopes' NV12 conversions.
        let mut slots = u64::from(plan.transition.is_some()) + u64::from(self.nv12.is_some()) + u64::from(self.scope_nv12.is_some());
        for item in plan.items() {
            slots += 1 + item.effects.len() as u64;
            self.sync_source(item);
        }
        if self.frame_no.is_multiple_of(SWEEP_FRAMES) {
            self.sources.retain(|_, s| Arc::strong_count(&s.video) > 1);
        }
        let device = self.gpu.device.clone();
        self.uniforms.begin(&device, &self.pipes.uniform_layout, slots);
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });

        for item in plan.items() {
            if !item.effects.is_empty()
                && let Some(st) = self.sources.get_mut(&item.source)
            {
                run_effects(&device, &self.pipes, &mut self.uniforms, &mut enc, st, &item.effects, self.frame_no);
            }
        }

        let ctx = Ctx { pipes: &self.pipes, sources: &self.sources, frame_no: self.frame_no, canvas: (self.width as f32, self.height as f32) };
        let pi = self.views.program.latest().map_or(0, |i| (i + 1) % RING_LEN);
        let program = &self.program[pi];
        let transition = plan.transition.as_ref().and_then(|t| {
            let p = t.progress(now);
            let pipe = self.pipes.transitions.get(&t.key)?.as_ref()?;
            (p < 1.0).then_some((t, p, pipe))
        });
        match transition {
            Some((t, progress, pipe)) => {
                draw_scene(&ctx, &mut self.uniforms, &mut enc, &self.scratch[0].view, &t.from);
                draw_scene(&ctx, &mut self.uniforms, &mut enc, &self.scratch[1].view, &plan.program);
                let mut slot = [0.0; 16];
                slot[0] = progress;
                slot[4..8].copy_from_slice(&t.params);
                let off = self.uniforms.push(&slot);
                let mut pass = begin(&mut enc, &program.view, None);
                pass.set_pipeline(pipe);
                pass.set_bind_group(0, &self.uniforms.bind, &[off]);
                pass.set_bind_group(1, &self.scratch_bind, &[]);
                pass.draw(0..3, 0..1);
            }
            None => draw_scene(&ctx, &mut self.uniforms, &mut enc, &program.view, &plan.program),
        }

        let mut qi = None;
        if let Some(scene) = &plan.preview {
            let i = self.views.preview.latest().map_or(0, |i| (i + 1) % RING_LEN);
            draw_scene(&ctx, &mut self.uniforms, &mut enc, &self.preview[i].view, scene);
            qi = Some(i);
        }

        let copied = self.nv12.as_mut().is_some_and(|n| convert(n, &self.pipes, &mut self.uniforms, &mut enc, program, now, readback_lost));
        let scope_copied =
            self.scope_nv12.as_mut().is_some_and(|n| convert(n, &self.pipes, &mut self.uniforms, &mut enc, program, now, &self.scope_skipped));

        self.uniforms.flush(&self.gpu.queue);
        self.gpu.queue.submit([enc.finish()]);
        self.views.program.publish(pi);
        if let Some(i) = qi {
            self.views.preview.publish(i);
        }
        for (n, copied) in [(&mut self.nv12, copied), (&mut self.scope_nv12, scope_copied)] {
            if copied && let Some(n) = n {
                n.submitted();
            }
        }
        if self.nv12.is_some() || self.scope_nv12.is_some() {
            let _ = self.gpu.device.poll(wgpu::PollType::Poll);
        }
        if let Some(n) = &mut self.nv12 {
            n.collect(readback_lost);
        }
        if let Some(n) = &mut self.scope_nv12 {
            n.collect(&self.scope_skipped);
        }
    }

    /// Bring a source's texture up to date with its newest content.
    fn sync_source(&mut self, item: &ItemDesc) {
        let st = self.sources.entry(item.source).or_insert_with(|| SourceTex {
            video: Arc::clone(&item.video),
            tex: None,
            native: (0, 0),
            solid: None,
            fx: None,
            fx_frame: 0,
            fx_result: 0,
        });
        if !Arc::ptr_eq(&st.video, &item.video) {
            *st = SourceTex { video: Arc::clone(&item.video), tex: None, native: (0, 0), solid: None, fx: None, fx_frame: 0, fx_result: 0 };
        }
        let Some(content) = st.video.take() else { return };
        let (device, queue) = (&self.gpu.device, &self.gpu.queue);
        match content {
            VideoContent::Frame(f) => {
                let format = match f.format {
                    PixelFormat::Rgba8 => wgpu::TextureFormat::Rgba8Unorm,
                    PixelFormat::Bgra8 => wgpu::TextureFormat::Bgra8Unorm,
                    PixelFormat::Nv12 => return,
                };
                let limit = device.limits().max_texture_dimension_2d;
                if !f.is_consistent() || f.width > limit || f.height > limit {
                    return;
                }
                let tex = ensure_tex(device, &self.pipes, &mut st.tex, f.width, f.height, format);
                write(queue, tex, &f.data);
                st.native = (f.width, f.height);
                st.solid = None;
            }
            VideoContent::Solid { rgba, width, height } => {
                let fresh = st.tex.as_ref().is_none_or(|t| (t.width, t.height, t.format) != (1, 1, wgpu::TextureFormat::Rgba8Unorm));
                let tex = ensure_tex(device, &self.pipes, &mut st.tex, 1, 1, wgpu::TextureFormat::Rgba8Unorm);
                if fresh || st.solid != Some(rgba) {
                    write(queue, tex, &rgba);
                }
                st.native = (width.max(1), height.max(1));
                st.solid = Some(rgba);
            }
            VideoContent::Empty => st.native = (0, 0),
        }
    }

    /// Read the newest program picture back as tightly packed RGBA (premultiplied alpha). Waits
    /// for the GPU; meant for tests, benchmarks and still captures.
    pub fn read_program(&self) -> Result<Vec<u8>, RenderError> {
        let i = self.views.program.latest().unwrap_or(0);
        let target = self.program.get(i).ok_or_else(|| RenderError::Device("no program picture".into()))?;
        read_rgba(&self.gpu, &target.texture, self.width, self.height)
    }

    /// Read the newest preview picture back, if studio mode drew one.
    pub fn read_preview(&self) -> Result<Option<Vec<u8>>, RenderError> {
        let Some(i) = self.views.preview.latest() else { return Ok(None) };
        let target = self.preview.get(i).ok_or_else(|| RenderError::Device("no preview picture".into()))?;
        read_rgba(&self.gpu, &target.texture, self.width, self.height).map(Some)
    }

    /// Wait until the GPU has finished every frame submitted so far, and hand finished
    /// read-backs to the encoders.
    pub fn finish(&mut self, readback_lost: &AtomicU64) {
        let _ = self.gpu.device.poll(wgpu::PollType::wait_indefinitely());
        if let Some(n) = &mut self.nv12 {
            n.collect(readback_lost);
        }
        if let Some(n) = &mut self.scope_nv12 {
            n.collect(&self.scope_skipped);
        }
    }
}

/// `current` sending to `target`'s sinks when it already has `target`'s size, else a new converter,
/// or none without a target.
fn retarget(device: &wgpu::Device, current: Option<Nv12>, target: Option<EncodeTarget>) -> Option<Nv12> {
    match (target, current) {
        (None, _) => None,
        (Some(t), Some(mut n)) if (n.width, n.height) == ((t.width + 1) & !1, (t.height + 1) & !1) => {
            n.set_sinks(t.sinks);
            Some(n)
        }
        (Some(t), _) => Some(Nv12::new(device, t)),
    }
}

/// Record `n`'s two conversion passes from `program` and its copy into a free read-back buffer;
/// whether it was recorded. With every buffer in flight the frame is skipped and counted in `lost`.
fn convert(
    n: &mut Nv12,
    pipes: &Pipes,
    uniforms: &mut Uniforms,
    enc: &mut wgpu::CommandEncoder,
    program: &Target,
    now: Tick,
    lost: &AtomicU64,
) -> bool {
    if !n.has_room() {
        lost.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    let off = uniforms.push(&[0.0; 16]);
    for (pipe, view) in [(&pipes.nv12_y, &n.y_view), (&pipes.nv12_uv, &n.uv_view)] {
        let mut pass = begin(enc, view, Some(wgpu::Color::BLACK));
        pass.set_pipeline(pipe);
        pass.set_bind_group(0, &uniforms.bind, &[off]);
        pass.set_bind_group(1, &program.bind, &[]);
        pass.draw(0..3, 0..1);
    }
    n.copy(enc, now);
    true
}

fn read_rgba(gpu: &Gpu, texture: &wgpu::Texture, width: u32, height: u32) -> Result<Vec<u8>, RenderError> {
    let row = (width * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("program read-back"),
        size: u64::from(row) * u64::from(height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("program read-back") });
    enc.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo { texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: Some(height) },
        },
        wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
    );
    gpu.queue.submit([enc.finish()]);
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    buffer.map_async(wgpu::MapMode::Read, .., move |r| {
        let _ = tx.try_send(r.is_ok());
    });
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).map_err(|e| RenderError::Device(e.to_string()))?;
    // Another thread polling the same device (the window) may run the callback instead of this
    // poll, so the result can arrive a moment after the poll returns.
    if rx.recv_timeout(MAP_WAIT) != Ok(true) {
        return Err(RenderError::Device("the picture could not be read back".into()));
    }
    let view = buffer.get_mapped_range(..).map_err(|e| RenderError::Device(e.to_string()))?;
    let mut out = Vec::with_capacity((width * height * 4) as usize);
    for r in view.chunks(row as usize).take(height as usize) {
        out.extend_from_slice(r.get(..(width * 4) as usize).unwrap_or_default());
    }
    Ok(out)
}

/// What scene drawing reads.
struct Ctx<'a> {
    pipes: &'a Pipes,
    sources: &'a HashMap<SourceId, SourceTex>,
    frame_no: u64,
    canvas: (f32, f32),
}

fn begin<'e>(enc: &'e mut wgpu::CommandEncoder, view: &wgpu::TextureView, clear: Option<wgpu::Color>) -> wgpu::RenderPass<'e> {
    let load = match clear {
        Some(c) => wgpu::LoadOp::Clear(c),
        None => wgpu::LoadOp::Load,
    };
    enc.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: None,
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view,
            resolve_target: None,
            depth_slice: None,
            ops: wgpu::Operations { load, store: wgpu::StoreOp::Store },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    })
}

/// The texture rectangle left after `crop` of a `w` x `h` source.
fn crop_uv(crop: Crop, (w, h): (u32, u32)) -> [f32; 4] {
    let (cw, ch) = crop.apply(w, h);
    let (wf, hf) = (w.max(1) as f32, h.max(1) as f32);
    let u0 = crop.left.min(w.saturating_sub(1)) as f32 / wf;
    let v0 = crop.top.min(h.saturating_sub(1)) as f32 / hf;
    [u0, v0, (u0 + cw as f32 / wf).min(1.0), (v0 + ch as f32 / hf).min(1.0)]
}

/// Draw a scene's items bottom to top over black.
fn draw_scene(ctx: &Ctx<'_>, uniforms: &mut Uniforms, enc: &mut wgpu::CommandEncoder, view: &wgpu::TextureView, scene: &SceneDesc) {
    let mut pass = begin(enc, view, Some(wgpu::Color::BLACK));
    pass.set_pipeline(&ctx.pipes.item);
    let (cw, ch) = ctx.canvas;
    let clip = |v: Vec2| [v.x / cw * 2.0 - 1.0, 1.0 - v.y / ch * 2.0];
    for item in &scene.items {
        let Some(st) = ctx.sources.get(&item.source) else { continue };
        let Some(tex) = &st.tex else { continue };
        if st.native.0 == 0 || st.native.1 == 0 {
            continue;
        }
        let filtered = !item.effects.is_empty() && st.fx_frame == ctx.frame_no;
        let (bind, straight) = match (&st.fx, filtered) {
            (Some(fx), true) => (&fx.binds[st.fx_result], 0.0),
            _ => (&tex.bind, 1.0),
        };
        let uv = if st.solid.is_some() { [0.0, 0.0, 1.0, 1.0] } else { crop_uv(item.transform.crop, st.native) };
        let t = item.transform.sanitized();
        let [c0, c1, c2, c3] = t.corners(st.native).map(clip);
        let slot = [c0[0], c0[1], c1[0], c1[1], c2[0], c2[1], c3[0], c3[1], uv[0], uv[1], uv[2], uv[3], straight, 1.0, 0.0, 0.0];
        let off = uniforms.push(&slot);
        pass.set_bind_group(0, &uniforms.bind, &[off]);
        pass.set_bind_group(1, bind, &[]);
        pass.draw(0..6, 0..1);
    }
}

/// Run a source's filter passes once this frame.
fn run_effects(
    device: &wgpu::Device,
    pipes: &Pipes,
    uniforms: &mut Uniforms,
    enc: &mut wgpu::CommandEncoder,
    st: &mut SourceTex,
    effects: &[VideoEffect],
    frame_no: u64,
) {
    if st.fx_frame == frame_no {
        return;
    }
    let Some(tex) = &st.tex else { return };
    let (w, h) = if st.solid.is_some() { st.native } else { (tex.width, tex.height) };
    let limit = device.limits().max_texture_dimension_2d;
    let (w, h) = (w.clamp(1, limit), h.clamp(1, limit));
    if st.fx.as_ref().is_none_or(|f| (f.width, f.height) != (w, h)) {
        st.fx = Some(make_fx(device, pipes, w, h));
    }
    let Some(fx) = &st.fx else { return };
    let mut input = &tex.bind;
    let mut straight = 1.0;
    let mut out = 0;
    let mut ran = false;
    for e in effects {
        let Some(Some(pipe)) = pipes.effects.get(e.key) else { continue };
        let mut slot = [0.0; SLOT_FLOATS];
        slot[..8].copy_from_slice(&[0.0, 0.0, 1.0, 1.0, w as f32, h as f32, straight, 0.0]);
        slot[8..].copy_from_slice(&e.params);
        let off = uniforms.push(&slot);
        let mut pass = begin(enc, &fx.views[out], Some(wgpu::Color::TRANSPARENT));
        pass.set_pipeline(pipe);
        pass.set_bind_group(0, &uniforms.bind, &[off]);
        pass.set_bind_group(1, input, &[]);
        pass.draw(0..3, 0..1);
        drop(pass);
        input = &fx.binds[out];
        straight = 0.0;
        ran = true;
        st.fx_result = out;
        out = 1 - out;
    }
    if ran {
        st.fx_frame = frame_no;
    }
}

fn texture(
    device: &wgpu::Device,
    label: &str,
    w: u32,
    h: u32,
    format: wgpu::TextureFormat,
    usage: wgpu::TextureUsages,
    view_formats: &[wgpu::TextureFormat],
) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage,
        view_formats,
    })
}

fn tex_bind(device: &wgpu::Device, pipes: &Pipes, view: &wgpu::TextureView) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: None,
        layout: &pipes.tex_layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(view) },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&pipes.sampler) },
        ],
    })
}

fn ensure_tex<'t>(device: &wgpu::Device, pipes: &Pipes, slot: &'t mut Option<Tex>, w: u32, h: u32, format: wgpu::TextureFormat) -> &'t Tex {
    if slot.as_ref().is_some_and(|t| (t.width, t.height, t.format) != (w, h, format)) {
        *slot = None;
    }
    slot.get_or_insert_with(|| {
        let texture = texture(device, "source", w, h, format, wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST, &[]);
        let bind = tex_bind(device, pipes, &texture.create_view(&wgpu::TextureViewDescriptor::default()));
        Tex { texture, bind, width: w, height: h, format }
    })
}

fn write(queue: &wgpu::Queue, tex: &Tex, data: &[u8]) {
    queue.write_texture(
        wgpu::TexelCopyTextureInfo { texture: &tex.texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
        data,
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(tex.width * 4), rows_per_image: Some(tex.height) },
        wgpu::Extent3d { width: tex.width, height: tex.height, depth_or_array_layers: 1 },
    );
}

fn make_fx(device: &wgpu::Device, pipes: &Pipes, w: u32, h: u32) -> FxPair {
    let usage = wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING;
    let make = || {
        let t = texture(device, "filter pass", w, h, CANVAS_FORMAT, usage, &[]);
        let v = t.create_view(&wgpu::TextureViewDescriptor::default());
        let b = tex_bind(device, pipes, &v);
        (v, b)
    };
    let (v0, b0) = make();
    let (v1, b1) = make();
    FxPair { width: w, height: h, views: [v0, v1], binds: [b0, b1] }
}

fn make_target(gpu: &Gpu, pipes: &Pipes, label: &str, w: u32, h: u32) -> (Target, wgpu::TextureView) {
    let usage = wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC;
    let texture = texture(&gpu.device, label, w, h, CANVAS_FORMAT, usage, &[CANVAS_VIEW_FORMAT]);
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let srgb = texture.create_view(&wgpu::TextureViewDescriptor { format: Some(CANVAS_VIEW_FORMAT), ..Default::default() });
    let bind = tex_bind(&gpu.device, pipes, &view);
    (Target { texture, view, bind }, srgb)
}

fn make_canvas(
    gpu: &Gpu,
    pipes: &Pipes,
    w: u32,
    h: u32,
    generation: u64,
) -> (Vec<Target>, Vec<Target>, Vec<Target>, wgpu::BindGroup, Arc<CanvasViews>) {
    let ring = |label: &str| -> (Vec<Target>, Ring) {
        let (targets, views): (Vec<_>, Vec<_>) = (0..RING_LEN).map(|_| make_target(gpu, pipes, label, w, h)).unzip();
        (targets, Ring { views, latest: AtomicUsize::new(0), drawn: AtomicBool::new(false) })
    };
    let (program, program_ring) = ring("program");
    let (preview, preview_ring) = ring("preview");
    let scratch: Vec<Target> = (0..2).map(|_| make_target(gpu, pipes, "transition scene", w, h).0).collect();
    let views: Vec<&wgpu::TextureView> = scratch.iter().map(|t| &t.view).collect();
    let scratch_bind = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("transition scenes"),
        layout: &pipes.tex2_layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(views[0]) },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(views[1]) },
            wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&pipes.sampler) },
        ],
    });
    let views = Arc::new(CanvasViews { width: w, height: h, generation, program: program_ring, preview: preview_ring });
    (program, preview, scratch, scratch_bind, views)
}

fn pipeline(
    device: &wgpu::Device,
    label: &str,
    layout: &wgpu::PipelineLayout,
    module: &wgpu::ShaderModule,
    vs: &str,
    fs: &str,
    format: wgpu::TextureFormat,
    blend: Option<wgpu::BlendState>,
) -> wgpu::RenderPipeline {
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(layout),
        vertex: wgpu::VertexState { module, entry_point: Some(vs), buffers: &[], compilation_options: Default::default() },
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module,
            entry_point: Some(fs),
            targets: &[Some(wgpu::ColorTargetState { format, blend, write_mask: wgpu::ColorWrites::ALL })],
            compilation_options: Default::default(),
        }),
        multiview_mask: None,
        cache: None,
    })
}

fn make_pipes(gpu: &Gpu) -> Pipes {
    let device = &gpu.device;
    let frag_tex = |binding| wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            multisampled: false,
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
        },
        count: None,
    };
    let frag_sampler = |binding| wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
        count: None,
    };
    let uniform_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("pass uniforms"),
        entries: &[wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: true,
                min_binding_size: wgpu::BufferSize::new(SLOT_BYTES),
            },
            count: None,
        }],
    });
    let tex_layout =
        device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: Some("texture"), entries: &[frag_tex(0), frag_sampler(1)] });
    let tex2_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("two textures"),
        entries: &[frag_tex(0), frag_tex(1), frag_sampler(2)],
    });
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("one texture"),
        bind_group_layouts: &[Some(&uniform_layout), Some(&tex_layout)],
        immediate_size: 0,
    });
    let layout2 = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("two textures"),
        bind_group_layouts: &[Some(&uniform_layout), Some(&tex2_layout)],
        immediate_size: 0,
    });
    let module = |label: &str, source: String| {
        device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some(label), source: wgpu::ShaderSource::Wgsl(source.into()) })
    };
    let item_module = module("items", shaders::item());
    let nv12_module = module("nv12", shaders::nv12());
    // Premultiplied alpha over what is already drawn.
    let over = wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING;
    let item = pipeline(device, "items", &layout, &item_module, "vs_item", "fs_item", CANVAS_FORMAT, Some(over));
    let nv12_y = pipeline(device, "nv12 luma", &layout, &nv12_module, "vs_full", "fs_y", wgpu::TextureFormat::R8Unorm, None);
    let nv12_uv = pipeline(device, "nv12 chroma", &layout, &nv12_module, "vs_full", "fs_uv", wgpu::TextureFormat::Rg8Unorm, None);
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("linear"),
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });
    Pipes {
        uniform_layout,
        tex_layout,
        tex2_layout,
        layout,
        layout2,
        item,
        nv12_y,
        nv12_uv,
        effects: HashMap::new(),
        transitions: HashMap::new(),
        sampler,
    }
}

#[cfg(test)]
mod tests;
